using Microsoft.Data.Sqlite;

namespace SpeedTracker.Core;

// Passive, read-only Antigravity telemetry, for both the editor and the agy terminal agent.
//
// Each conversation is a SQLite file in ~/.gemini/antigravity/conversations. Its gen_metadata table
// holds one protobuf per model call, and Antigravity measures the call itself: time to first token,
// streaming time and token counts. The steps table says when the call was made. Field numbers were
// read from the schema embedded in Antigravity 2.21 and checked against a real conversation.
//
// The same rows also hold the prompt. Only numbers, the model name and ids are decoded; text fields
// are stepped over without being turned into strings. Mirrors AntigravityLogs.swift.
public sealed class AntigravityLogWatcher : IDisposable
{
    private sealed class Conversation
    {
        public (long Size, DateTime Modified, long? LogSize, DateTime? LogModified)? Stamp;
        // Calls below this index are finished with: emitted, or permanently unusable.
        public long NextIndex;
        public DateTimeOffset? AwaitingSince;
    }
    private readonly string[] roots;
    private readonly Dictionary<string, Conversation> conversations = new(StringComparer.Ordinal);
    private readonly HashSet<string> emitted = new(StringComparer.Ordinal);
    private string[] files = Array.Empty<string>();
    private DateTimeOffset lastScan = DateTimeOffset.MinValue;
    private DateTimeOffset latestModelAt = DateTimeOffset.MinValue;
    // The newest call keeps its whole prompt until the next one replaces it.
    private const long MaxBlobBytes = 64 * 1024 * 1024;
    private const int MaxRowsPerPoll = 256;

    public bool HasLogs { get; private set; }
    public string? Limitation { get; private set; }
    public string? LatestModel { get; private set; }
    public DateTimeOffset? LastRequestAt { get; private set; }

    // Each root is a folder of <conversation id>.db files.
    public AntigravityLogWatcher(IEnumerable<string> roots) { this.roots = roots.Distinct(StringComparer.Ordinal).ToArray(); }

    // A call has been made, by Antigravity's own record, and its reply is not complete.
    public IReadOnlyList<LiveCall> Active(DateTimeOffset now) => conversations
        .Where(pair => pair.Value.AwaitingSince is DateTimeOffset since && since <= now && now - since < TimeSpan.FromMinutes(10))
        .Select(pair => new LiveCall(pair.Key, "Antigravity", LatestModel ?? "unknown", "google", "Waiting", pair.Value.AwaitingSince!.Value, pair.Value.AwaitingSince!.Value))
        .ToArray();

    public IReadOnlyList<LogRecord> Poll(DateTimeOffset now)
    {
        if (now - lastScan >= TimeSpan.FromSeconds(4))
        {
            lastScan = now;
            files = roots.SelectMany(root => { try { return Directory.Exists(root) ? Directory.GetFiles(root, "*.db") : Array.Empty<string>(); } catch (IOException) { return Array.Empty<string>(); } catch (UnauthorizedAccessException) { return Array.Empty<string>(); } })
                .Where(path => path.EndsWith(".db", StringComparison.OrdinalIgnoreCase)).ToArray();
            foreach (var gone in conversations.Keys.Except(files, StringComparer.Ordinal).ToArray()) conversations.Remove(gone);
            if (!HasLogs) HasLogs = Probe();
        }
        var cutoff = now.AddDays(-7);
        var records = new List<LogRecord>();
        Limitation = null;
        foreach (var file in files)
        {
            (long Size, DateTime Modified, long? LogSize, DateTime? LogModified) stamp;
            try
            {
                var info = new FileInfo(file); var log = new FileInfo(file + "-wal");
                if (!info.Exists) continue;
                stamp = (info.Length, info.LastWriteTimeUtc, log.Exists ? log.Length : null, log.Exists ? log.LastWriteTimeUtc : null);
            }
            catch (IOException) { continue; }
            catch (UnauthorizedAccessException) { continue; }
            if (!conversations.TryGetValue(file, out var conversation)) conversations[file] = conversation = new Conversation();
            var touched = stamp.LogModified is DateTime logged && logged > stamp.Modified ? logged : stamp.Modified;
            // An untouched file has nothing new. Old conversations are never opened at all.
            if (conversation.Stamp == stamp || touched < cutoff.UtcDateTime) continue;
            try
            {
                using var database = new Database(file);
                var result = database.Read(Path.GetFileNameWithoutExtension(file), conversation.NextIndex);
                HasLogs = true;
                conversation.NextIndex = result.NextIndex; conversation.AwaitingSince = result.AwaitingSince;
                if (result.LastRequestAt is DateTimeOffset request && (LastRequestAt == null || request > LastRequestAt)) LastRequestAt = request;
                foreach (var record in result.Records)
                {
                    if (record.End < cutoff || !emitted.Add(record.Key)) continue;
                    records.Add(record);
                    if (record.End >= latestModelAt) { latestModelAt = record.End; LatestModel = record.Model; }
                }
                // More rows than one pass reads: leave the stamp unset so the next poll continues.
                if (!result.Truncated) conversation.Stamp = stamp;
            }
            // Busy, locked or not a conversation: come back on the next poll. Nothing is guessed.
            catch (SqliteException) { Limitation = "Antigravity conversation telemetry is temporarily unreadable."; }
            catch (IOException) { Limitation = "Antigravity conversation telemetry is temporarily unreadable."; }
            catch (UnauthorizedAccessException) { Limitation = "Antigravity conversation telemetry is not readable by this user."; }
        }
        return records.OrderBy(r => r.End).ToArray();
    }

    // Whether Antigravity keeps conversations here in a form this reader understands, even if none
    // is recent enough to read. One look at the newest file answers it.
    private bool Probe()
    {
        try
        {
            var newest = files.OrderByDescending(File.GetLastWriteTimeUtc).FirstOrDefault();
            if (newest == null) return false;
            using var database = new Database(newest);
            return database.HasGenerations;
        }
        catch (SqliteException) { return false; }
        catch (IOException) { return false; }
        catch (UnauthorizedAccessException) { return false; }
    }

    public void Dispose() { conversations.Clear(); }

    private sealed class Database : IDisposable
    {
        public sealed record Result(List<LogRecord> Records, long NextIndex, bool Truncated, DateTimeOffset? AwaitingSince, DateTimeOffset? LastRequestAt);
        private readonly SqliteConnection connection;

        public Database(string path)
        {
            // These files are in write-ahead-log mode. Opening one normally makes SQLite create its -shm
            // and -wal companions, and a read-only connection cannot remove them again, so a plain open
            // would leave files behind in Antigravity's folder. With no log present the main file is
            // complete on its own and is read as an immutable snapshot, which creates nothing.
            var source = path;
            if (!File.Exists(path + "-wal") && Immutable(path) is string uri) source = uri;
            connection = new(new SqliteConnectionStringBuilder { DataSource = source, Mode = SqliteOpenMode.ReadOnly, Pooling = false, DefaultTimeout = 1 }.ToString());
            connection.Open();
        }

        // SQLite URI filenames cannot name a network share, so those are opened the plain way.
        private static string? Immutable(string path)
        {
            var normal = path.Replace('\\', '/');
            if (normal.StartsWith("//", StringComparison.Ordinal)) return null;
            var escaped = string.Join('/', normal.Split('/').Select((part, index) => index == 0 && part.EndsWith(':') ? part : Uri.EscapeDataString(part)));
            return "file:" + (escaped.StartsWith('/') ? "" : "/") + escaped + "?immutable=1";
        }

        public bool HasGenerations
        {
            get
            {
                using var command = connection.CreateCommand();
                command.CommandText = "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('gen_metadata', 'steps')";
                return Convert.ToInt64(command.ExecuteScalar()) == 2;
            }
        }

        public Result Read(string conversation, long first)
        {
            var rows = new List<(long Index, long Size)>();
            using (var command = connection.CreateCommand())
            {
                command.CommandText = "SELECT idx, length(data) FROM gen_metadata WHERE idx >= $first ORDER BY idx LIMIT $limit";
                command.Parameters.AddWithValue("$first", first); command.Parameters.AddWithValue("$limit", MaxRowsPerPoll + 1);
                using var reader = command.ExecuteReader();
                while (reader.Read()) rows.Add((reader.GetInt64(0), reader.IsDBNull(1) ? 0 : reader.GetInt64(1)));
            }
            var truncated = rows.Count > MaxRowsPerPoll;
            long? newest = truncated || rows.Count == 0 ? null : rows[^1].Index;
            var records = new List<LogRecord>(); var next = first; var blocked = false;
            foreach (var row in rows.Take(MaxRowsPerPoll))
            {
                var finished = true;
                if (row.Size is > 0 and <= MaxBlobBytes && Blob("SELECT data FROM gen_metadata WHERE idx = $index", row.Index) is byte[] data && AntigravityGeneration.Parse(data) is AntigravityGeneration generation)
                {
                    if (generation.IsComplete)
                    {
                        if (generation.StepIndices.Count > 0 && StepTime(generation.StepIndices[0], 1) is DateTimeOffset started && generation.Record(conversation, row.Index, started) is LogRecord record) records.Add(record);
                    }
                    // Written before its reply finished; look again when the file changes. An unfinished
                    // call with later calls after it was abandoned and never will be.
                    else if (row.Index == newest) finished = false;
                }
                if (!finished) blocked = true;
                if (!blocked) next = row.Index + 1;
            }
            // The newest model step tells when the last request went out, and whether its reply is still due.
            // CortexStepMetadata: created_at = 1, completed_at = 8. Step type 15 is the model's response.
            DateTimeOffset? awaiting = null, lastRequest = null;
            using (var command = connection.CreateCommand())
            {
                command.CommandText = "SELECT idx FROM steps WHERE step_type = 15 ORDER BY idx DESC LIMIT 1";
                if (command.ExecuteScalar() is long latest && StepTime(latest, 1) is DateTimeOffset created)
                {
                    lastRequest = created;
                    if (StepTime(latest, 8) == null) awaiting = created;
                }
            }
            return new Result(records, next, truncated, awaiting, lastRequest);
        }

        private DateTimeOffset? StepTime(long index, int field)
        {
            if (Blob("SELECT metadata FROM steps WHERE idx = $index AND length(metadata) <= 1048576", index) is not byte[] data || Protobuf.Fields(data) is not { } fields) return null;
            foreach (var candidate in fields) if (candidate.Number == field && candidate.IsBytes) return Protobuf.Timestamp(candidate.Bytes(data));
            return null;
        }

        private byte[]? Blob(string sql, long index)
        {
            using var command = connection.CreateCommand();
            command.CommandText = sql; command.Parameters.AddWithValue("$index", index);
            using var reader = command.ExecuteReader();
            return reader.Read() && !reader.IsDBNull(0) ? reader.GetFieldValue<byte[]>(0) : null;
        }

        public void Dispose() => connection.Dispose();
    }
}

// What one gen_metadata row says about a model call.
public sealed class AntigravityGeneration
{
    public List<long> StepIndices { get; } = new();
    public string ExecutionId { get; private set; } = "";
    public string? Model { get; private set; }
    public int? InputTokens { get; private set; }
    public int? OutputTokens { get; private set; }
    public int? CacheWriteTokens { get; private set; }
    public int? CacheReadTokens { get; private set; }
    public int? ThinkingTokens { get; private set; }
    public double? TimeToFirstToken { get; private set; }
    public double? StreamingDuration { get; private set; }
    // A stream shorter than this is one or two network chunks: it shows that the reply arrived,
    // not how fast it was written.
    public const double ShortestTimedStream = 1;

    // CortexStepGeneratorMetadata: chat_model = 1, step_indices = 2, execution_id = 4.
    public static AntigravityGeneration? Parse(byte[] data)
    {
        if (Protobuf.Fields(data) is not { } fields) return null;
        var value = new AntigravityGeneration(); ArraySegment<byte>? chatModel = null;
        foreach (var field in fields)
        {
            if (field.Number == 1 && field.IsBytes) chatModel = field.Bytes(data);
            else if (field.Number == 2 && field.IsBytes) value.StepIndices.AddRange(Protobuf.PackedVarints(field.Bytes(data)).Select(v => unchecked((long)v)));
            else if (field.Number == 2 && !field.IsBytes) value.StepIndices.Add(unchecked((long)field.Value));
            else if (field.Number == 4 && field.IsBytes) value.ExecutionId = Protobuf.Identifier(field.Bytes(data)) ?? "";
        }
        // ChatModelMetadata: usage = 4, time_to_first_token = 11, streaming_duration = 12, response_model = 19.
        if (chatModel is not { } chat || Protobuf.Fields(chat) is not { } chatFields) return null;
        foreach (var field in chatFields.Where(f => f.IsBytes))
        {
            var bytes = field.Bytes(chat);
            if (field.Number == 4) value.ReadUsage(bytes);
            else if (field.Number == 11) value.TimeToFirstToken = Protobuf.Duration(bytes);
            else if (field.Number == 12) value.StreamingDuration = Protobuf.Duration(bytes);
            else if (field.Number == 19) value.Model = Protobuf.Identifier(bytes);
        }
        return value;
    }

    // ModelUsageStats: input = 2, output = 3, cache_write = 4, cache_read = 5, thinking_output = 9.
    // Output already includes thinking.
    private void ReadUsage(ArraySegment<byte> data)
    {
        foreach (var field in Protobuf.Fields(data) ?? new List<Protobuf.Field>())
        {
            if (field.IsBytes || field.Value > int.MaxValue) continue;
            var number = (int)field.Value;
            if (field.Number == 2) InputTokens = number; else if (field.Number == 3) OutputTokens = number;
            else if (field.Number == 4) CacheWriteTokens = number; else if (field.Number == 5) CacheReadTokens = number;
            else if (field.Number == 9) ThinkingTokens = number;
        }
    }

    // The call is finished: Antigravity writes usage and timing when the reply ends.
    public bool IsComplete => OutputTokens is > 0 && (TimeToFirstToken != null || StreamingDuration != null);

    public LogRecord? Record(string conversation, long index, DateTimeOffset startedAt)
    {
        if (!IsComplete || OutputTokens is not int output) return null;
        var streaming = Math.Max(StreamingDuration ?? 0, 0);
        DateTimeOffset? firstToken = TimeToFirstToken is double ttft ? startedAt.AddSeconds(Math.Max(ttft, 0)) : null;
        var end = (firstToken ?? startedAt).AddSeconds(streaming);
        var total = (end - startedAt).TotalSeconds;
        var thinking = Math.Min(ThinkingTokens ?? 0, output);
        // Antigravity's first token is the first visible one: thinking is over by then. So the streaming
        // window holds only the visible reply, and counting every output token in it would inflate the
        // rate. When the window is too short to be a rate at all, the honest figure is the whole call.
        double? speed = streaming >= ShortestTimedStream && output > thinking ? (output - thinking) / streaming : total >= .05 ? output / total : null;
        var input = (InputTokens ?? 0) + (CacheReadTokens ?? 0) + (CacheWriteTokens ?? 0);
        return new LogRecord($"antigravity:{conversation}:{(ExecutionId.Length == 0 ? "-" : ExecutionId)}:{index}", "Antigravity", Model ?? "unknown", "google",
            startedAt, firstToken, firstToken, end, output, thinking > 0 ? thinking : null, input > 0 ? input : null, CacheReadTokens, false, speed);
    }
}

// Just enough of the protobuf wire format to pick numbered fields out of a message without its
// schema. Unknown fields are stepped over, never interpreted.
public static class Protobuf
{
    public readonly record struct Field(int Number, bool IsBytes, ulong Value, int Offset, int Length)
    {
        public ArraySegment<byte> Bytes(ArraySegment<byte> parent) => parent.Slice(Offset, Length);
    }

    // The top-level fields of a message, or null if the bytes are not a well-formed one.
    public static List<Field>? Fields(ArraySegment<byte> data)
    {
        var fields = new List<Field>(); var index = 0;
        while (index < data.Count)
        {
            if (Varint(data, ref index) is not ulong key) return null;
            var number = (int)(key >> 3);
            if (number <= 0) return null;
            switch (key & 7)
            {
                case 0:
                    if (Varint(data, ref index) is not ulong value) return null;
                    fields.Add(new(number, false, value, 0, 0)); break;
                case 1:
                    if (data.Count - index < 8) return null;
                    fields.Add(new(number, false, BitConverter.ToUInt64(data.Slice(index, 8)), 0, 0)); index += 8; break;
                case 2:
                    if (Varint(data, ref index) is not ulong length || length > (ulong)(data.Count - index)) return null;
                    fields.Add(new(number, true, 0, index, (int)length)); index += (int)length; break;
                case 5:
                    if (data.Count - index < 4) return null;
                    fields.Add(new(number, false, BitConverter.ToUInt32(data.Slice(index, 4)), 0, 0)); index += 4; break;
                default: return null;
            }
        }
        return fields;
    }

    public static List<ulong> PackedVarints(ArraySegment<byte> data)
    {
        var values = new List<ulong>(); var index = 0;
        while (index < data.Count && Varint(data, ref index) is ulong value) values.Add(value);
        return values;
    }

    // google.protobuf.Duration: seconds = 1, nanos = 2.
    public static double? Duration(ArraySegment<byte> data)
    {
        if (SecondsAndNanos(data) is not var (seconds, nanos)) return null;
        var value = seconds + nanos / 1_000_000_000d;
        return value is >= 0 and < 86_400 ? value : null;
    }

    // google.protobuf.Timestamp: seconds = 1, nanos = 2.
    public static DateTimeOffset? Timestamp(ArraySegment<byte> data)
    {
        if (SecondsAndNanos(data) is not var (seconds, nanos) || seconds <= 1_000_000_000 || seconds >= 10_000_000_000) return null;
        return DateTimeOffset.FromUnixTimeSeconds(seconds).AddTicks(nanos / 100);
    }

    // A short ASCII token such as a model name or UUID. Anything else is refused, so free text sitting
    // in a neighbouring field can never be mistaken for one.
    public static string? Identifier(ArraySegment<byte> data)
    {
        if (data.Count is 0 or > 128) return null;
        foreach (var b in data) if (!(b is >= 0x30 and <= 0x39 or >= 0x41 and <= 0x5A or >= 0x61 and <= 0x7A or 0x2D or 0x2E or 0x5F or 0x2F or 0x3A)) return null;
        return System.Text.Encoding.ASCII.GetString(data);
    }

    private static (long Seconds, long Nanos)? SecondsAndNanos(ArraySegment<byte> data)
    {
        if (Fields(data) is not { } fields) return null;
        long seconds = 0, nanos = 0;
        foreach (var field in fields)
        {
            if (field.IsBytes) return null;
            if (field.Number == 1) seconds = unchecked((long)field.Value);
            if (field.Number == 2) nanos = unchecked((long)field.Value);
        }
        return (seconds, nanos);
    }

    private static ulong? Varint(ArraySegment<byte> data, ref int index)
    {
        ulong value = 0; var shift = 0;
        while (index < data.Count && shift < 64)
        {
            var b = data[index++];
            value |= (ulong)(b & 0x7F) << shift;
            if ((b & 0x80) == 0) return value;
            shift += 7;
        }
        return null;
    }
}
