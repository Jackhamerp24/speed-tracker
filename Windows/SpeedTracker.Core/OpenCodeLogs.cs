using Microsoft.Data.Sqlite;
using System.Text.Json;

namespace SpeedTracker.Core;

// Read-only telemetry projections never return prompts, text, tool inputs or credentials.
public sealed class OpenCodeLogWatcher : IDisposable
{
    private const int MaximumRemembered = 32768;
    private const int MaximumPending = 512;
    private const int MaximumFileBytes = 1024 * 1024;
    private readonly string[] roots;
    private readonly Dictionary<string, Database> databases = new(StringComparer.Ordinal);
    private readonly Dictionary<string, (long Size, DateTime Time)> jsonRevisions = new(StringComparer.Ordinal);
    private readonly Dictionary<string, IEnumerator<string>> scans = new(StringComparer.Ordinal);
    private readonly Dictionary<string, DateTimeOffset> nextScan = new(StringComparer.Ordinal);
    private readonly Dictionary<string, PendingFile> pendingFiles = new(StringComparer.Ordinal);
    private readonly Dictionary<string, (Telemetry Telemetry, LiveCall Call)> active = new(StringComparer.Ordinal);
    private readonly Dictionary<string, DateTimeOffset> completed = new(StringComparer.Ordinal);
    private readonly Dictionary<string, (DateTimeOffset Created, string Id)> latestSession = new(StringComparer.Ordinal);
    private int jsonBytesRemaining;
    private bool readableJson;
    public IReadOnlyList<string> Roots => Array.AsReadOnly(roots);
    public bool HasLogs { get; private set; }
    public string? Limitation { get; private set; }
    public OpenCodeLogWatcher(IEnumerable<string> roots) { this.roots = roots.Distinct(StringComparer.Ordinal).ToArray(); }
    public IReadOnlyList<LiveCall> Active(DateTimeOffset now) => active.Values.Where(value =>
        latestSession.TryGetValue(value.Telemetry.Session, out var latest) && latest.Id == value.Telemetry.Id &&
        value.Call.StartedAt <= now && now - value.Call.LastActivity < TimeSpan.FromMinutes(10) &&
        now - value.Call.StartedAt < TimeSpan.FromMinutes(10)).Select(value => value.Call).ToArray();

    public IReadOnlyList<LogRecord> Poll(DateTimeOffset now)
    {
        var records = new List<LogRecord>();
        var cutoff = now.AddDays(-7);
        jsonBytesRemaining = 4 * 1024 * 1024;
        Limitation = null;
        foreach (var root in roots)
        {
            var path = root.EndsWith(".db", StringComparison.OrdinalIgnoreCase) ? root : Path.Combine(root, "opencode.db");
            if (File.Exists(path))
            {
                try
                {
                    if (databases.TryGetValue(path, out var old) && !old.IsCurrentFile) { old.Dispose(); databases.Remove(path); RemoveActiveSource(path); }
                    if (!databases.TryGetValue(path, out var database)) databases[path] = database = new Database(path, cutoff);
                    foreach (var telemetry in database.Poll()) Accept(telemetry, cutoff, records, path + ":");
                }
                catch (Exception ex) when (ex is SqliteException or IOException or UnauthorizedAccessException or JsonException)
                {
                    if (databases.Remove(path, out var failed)) failed.Dispose();
                    Limitation = $"OpenCode database telemetry unavailable: {ex.GetType().Name}.";
                    RemoveActiveSource(path);
                }
            }
            else if (databases.Remove(path, out var removed)) { removed.Dispose(); RemoveActiveSource(path); }
            if (root.EndsWith(".db", StringComparison.OrdinalIgnoreCase)) continue;
            var storage = Path.GetFileName(root) == "storage" ? root : Path.Combine(root, "storage");
            ScanJson(storage, cutoff, now, records);
        }
        foreach (var value in pendingFiles.Values.ToArray())
        {
            if (jsonBytesRemaining <= 0) break;
            ReadJson(value.File, value.Storage, cutoff, records);
        }
        foreach (var key in active.Where(p => p.Value.Telemetry.Updated < cutoff || now - p.Value.Call.StartedAt >= TimeSpan.FromMinutes(10)).Select(p => p.Key).ToArray()) active.Remove(key);
        foreach (var key in pendingFiles.Where(p => p.Value.Updated < cutoff).Select(p => p.Key).ToArray()) pendingFiles.Remove(key);
        foreach (var key in completed.Where(p => p.Value < cutoff).Select(p => p.Key).ToArray()) completed.Remove(key);
        foreach (var key in latestSession.Where(p => p.Value.Created < cutoff).Select(p => p.Key).ToArray()) latestSession.Remove(key);
        Trim(completed, MaximumRemembered, p => p.Value);
        Trim(latestSession, MaximumRemembered, p => p.Value.Created);
        Trim(jsonRevisions, MaximumRemembered, p => new DateTimeOffset(p.Value.Time, TimeSpan.Zero));
        Trim(pendingFiles, MaximumPending, p => p.Value.Updated);
        Trim(active, MaximumPending, p => p.Value.Telemetry.Updated);
        HasLogs = readableJson || databases.Values.Any(d => d.HasTelemetry);
        return records.OrderBy(r => r.End).ToArray();
    }
    private void RemoveActiveSource(string path)
    {
        foreach (var key in active.Where(p => p.Value.Call.Id.StartsWith(path + ":", StringComparison.Ordinal)).Select(p => p.Key).ToArray()) active.Remove(key);
    }

    private static void Trim<T>(Dictionary<string, T> values, int maximum, Func<KeyValuePair<string, T>, DateTimeOffset> date)
    {
        if (values.Count <= maximum) return;
        foreach (var key in values.OrderByDescending(date).Skip(maximum).Select(p => p.Key).ToArray()) values.Remove(key);
    }
    private void Accept(Telemetry telemetry, DateTimeOffset cutoff, List<LogRecord> records, string? source = null)
    {
        if (telemetry.Updated < cutoff) return;
        if (!latestSession.TryGetValue(telemetry.Session, out var latest) || telemetry.Created > latest.Created ||
            telemetry.Created == latest.Created && string.CompareOrdinal(telemetry.Id, latest.Id) >= 0)
            latestSession[telemetry.Session] = (telemetry.Created, telemetry.Id);
        var id = telemetry.Id;
        if (telemetry.Awaiting && !completed.ContainsKey(id))
        {
            var phase = telemetry.Current ? "Streaming · log timing limited" : telemetry.First == null ? "Waiting" : telemetry.Visible == null ? "Thinking" : "Streaming";
            active[id] = (telemetry, new((source ?? "") + id, "opencode", telemetry.Model, telemetry.Provider, phase,
                telemetry.Created, telemetry.Updated, !telemetry.Current && telemetry.First >= telemetry.Created ? (telemetry.First.Value - telemetry.Created).TotalSeconds : null));
        }
        else active.Remove(id);
        if (telemetry.Record is not LogRecord record) return;
        pendingFiles.Remove(id);
        if (record.End >= cutoff && completed.TryAdd(id, record.End)) records.Add(record);
    }
    private void ScanJson(string storage, DateTimeOffset cutoff, DateTimeOffset now, List<LogRecord> records)
    {
        if (!scans.TryGetValue(storage, out var iterator))
        {
            if (nextScan.TryGetValue(storage, out var next) && now < next) return;
            iterator = LogDiscovery.Files(Path.Combine(storage, "message"), "*.json").GetEnumerator();
            scans[storage] = iterator;
            nextScan[storage] = now.AddSeconds(4);
        }
        for (var visited = 0; visited < 256 && jsonBytesRemaining > 0; visited++)
        {
            if (!iterator.MoveNext()) { iterator.Dispose(); scans.Remove(storage); break; }
            var file = iterator.Current;
            try
            {
                var info = new FileInfo(file);
                if (!info.Exists || info.Length > MaximumFileBytes || info.LastWriteTimeUtc < cutoff.UtcDateTime && readableJson) continue;
                var revision = (info.Length, info.LastWriteTimeUtc);
                if (jsonRevisions.TryGetValue(file, out var previous) && previous == revision) continue;
                if (ReadJson(file, storage, cutoff, records)) jsonRevisions[file] = revision;
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            { Limitation = $"OpenCode JSON telemetry unavailable: {ex.GetType().Name}."; }
        }
    }
    private bool ReadJson(string file, string storage, DateTimeOffset cutoff, List<LogRecord> records)
    {
        try
        {
            using var document = JsonFile(file);
            if (document == null) return false;
            var data = document.RootElement;
            var id = data.S("id"); var session = data.S("sessionID");
            if (data.S("role") != "assistant" || !SafeComponent(id) || !SafeComponent(session)) return true;
            var parts = new List<Part>();
            var complete = true;
            using var iterator = LogDiscovery.Files(Path.Combine(storage, "part", id!), "*.json").GetEnumerator();
            for (var count = 0; count < 128 && iterator.MoveNext(); count++)
            {
                using var part = JsonFile(iterator.Current);
                if (part == null) { complete = false; continue; }
                parts.Add(Part.Read(part.RootElement, null));
            }
            if (iterator.MoveNext()) complete = false;
            var updated = new DateTimeOffset(File.GetLastWriteTimeUtc(file), TimeSpan.Zero);
            var telemetry = Telemetry.Read(id!, session!, data, parts, false, updated, complete);
            if (telemetry == null) return true;
            readableJson = true;
            if (telemetry.Record == null) pendingFiles[id!] = new(file, storage, telemetry.Updated);
            Accept(telemetry, cutoff, records);
            return true;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or JsonException)
        { Limitation = $"OpenCode JSON telemetry unavailable: {ex.GetType().Name}."; return false; }
    }
    private JsonDocument? JsonFile(string path)
    {
        var info = new FileInfo(path);
        if (!info.Exists || info.Length > MaximumFileBytes || info.Length + 1 > jsonBytesRemaining) return null;
        var size = (int)info.Length;
        var bytes = new byte[size + 1];
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
        var count = 0;
        while (count < bytes.Length)
        {
            var read = stream.Read(bytes, count, bytes.Length - count);
            if (read == 0) break;
            count += read;
        }
        jsonBytesRemaining -= count;
        return count == size ? JsonDocument.Parse(bytes.AsMemory(0, size)) : null;
    }
    private static bool SafeComponent(string? value) => !string.IsNullOrEmpty(value) && value is not ("." or "..") && value.AsSpan().IndexOfAny('/', '\\', ':') < 0;
    public void Dispose()
    {
        foreach (var database in databases.Values) database.Dispose();
        foreach (var iterator in scans.Values) iterator.Dispose();
        databases.Clear(); scans.Clear();
    }
    private sealed record PendingFile(string File, string Storage, DateTimeOffset Updated);
    private sealed record Part(string Type, DateTimeOffset? Start, DateTimeOffset? Created, DateTimeOffset? Persisted, bool Synthetic, bool Ignored, string? Status)
    {
        public static Part Read(JsonElement value, DateTimeOffset? persisted) => new(value.S("type") ?? "", value.Get("time").Get("start").Date(),
            value.Get("time").Get("created").Date(), persisted, value.B("synthetic") || value.N("synthetic") == 1, value.B("ignored") || value.N("ignored") == 1, value.Get("state").S("status"));
    }
    private sealed record Telemetry(string Id, string Session, string Model, string Provider, DateTimeOffset Created, DateTimeOffset Updated,
        bool Current, bool Awaiting, DateTimeOffset? First, DateTimeOffset? Visible, LogRecord? Record)
    {
        public static Telemetry? Read(string id, string session, JsonElement data, IReadOnlyList<Part> parts, bool current, DateTimeOffset? updated, bool complete)
        {
            var model = current ? data.Get("model").S("id") : data.S("modelID");
            var provider = (current ? data.Get("model").S("providerID") : data.S("providerID")) ?? "";
            var created = data.Get("time").Get("created").Date();
            if (string.IsNullOrEmpty(model) || created == null) return null;
            var completed = data.Get("time").Get("completed").Date();
            var finish = data.S("finish");
            var failed = data.Get("error").ValueKind is not (JsonValueKind.Null or JsonValueKind.Undefined) || finish == "error";
            var terminal = completed != null || finish != null || failed;
            var awaiting = complete && !terminal && !data.B("summary") && data.N("summary") != 1 && !parts.Any(p => p.Type == "tool" && p.Status is "running" or "completed" or "error");
            DateTimeOffset? first, visible;
            if (current)
            {
                first = parts.FirstOrDefault() is Part initial && initial.Type is "reasoning" or "tool" ? initial.Created : null;
                var firstVisible = parts.FirstOrDefault(p => p.Type is "text" or "tool");
                visible = firstVisible?.Type == "tool" ? firstVisible.Created : null;
            }
            else
            {
                var generated = parts.Where(p => p.Type is "reasoning" or "text" && !p.Synthetic && !p.Ignored);
                first = generated.Select(p => p.Start).Aggregate((DateTimeOffset?)null, J.Min);
                visible = generated.Where(p => p.Type == "text").Select(p => p.Start).Aggregate((DateTimeOffset?)null, J.Min);
            }
            var end = current ? completed : parts.Where(p => p.Type == "step-finish").Select(p => p.Persisted).Max() ?? completed;
            var tokens = data.Get("tokens"); var cache = tokens.Get("cache");
            var output = tokens.I("output"); var reasoning = tokens.I("reasoning");
            var totalOutput = output == null ? null : J.Add(output, reasoning);
            var input = tokens.I("input") is int uncached ? J.Add(uncached, cache.I("read"), cache.I("write")) : null;
            LogRecord? record = null;
            if (terminal && end >= created && totalOutput != null && (totalOutput > 0 || input > 0))
            {
                first = complete && first >= created && first <= end ? first : null;
                visible = complete && visible >= created && visible <= end ? visible : null;
                record = new("opencode:" + id, "opencode", model, provider, current ? null : created, first, visible, end!.Value,
                    totalOutput.Value, reasoning, input, cache.I("read"), failed || finish is "abort" or "aborted" or "cancelled");
            }
            var activity = updated ?? completed ?? created.Value;
            if (activity < created) activity = created.Value;
            return new(id, session, model, provider, created.Value, activity, current, awaiting, first, visible, record);
        }
    }
    private sealed class Table(string name, long high, long cutoff)
    {
        public string Name { get; } = name;
        public bool Current => Name == "session_message";
        public bool IndexedHistory;
        public bool SessionHistory;
        public long HighRow = high;
        public long FallbackRow = high == long.MaxValue ? high : high + 1;
        public long HistoryTime = cutoff;
        public string HistoryId = "";
        public bool HistoryDone;
        public bool ProbeDone;
        public long SessionRow;
        public bool SessionsDone;
        public readonly Queue<string> Sessions = new();
        public readonly Queue<string> Work = new();
        public readonly HashSet<string> Queued = new(StringComparer.Ordinal);
        public readonly Dictionary<string, DateTimeOffset> Pending = new(StringComparer.Ordinal);
        public void Enqueue(string id) { if (Queued.Add(id)) Work.Enqueue(id); }
    }
    private sealed class Database : IDisposable
    {
        private const int Batch = 128;
        private const int MaxJsonBytes = 4 * 1024 * 1024;
        private readonly string path;
        private readonly DateTime creation;
        private readonly SqliteConnection connection;
        private readonly List<Table> tables = new();
        private bool hasPart;
        private long schemaVersion = -1;
        private long version = -1;
        private readonly long cutoff;
        private int bytesRemaining;
        public bool HasTelemetry { get; private set; }
        public bool IsCurrentFile => File.Exists(path) && File.GetCreationTimeUtc(path) == creation;
        public Database(string path, DateTimeOffset from)
        {
            this.path = path;
            creation = File.GetCreationTimeUtc(path);
            cutoff = from.ToUnixTimeMilliseconds();
            connection = new(new SqliteConnectionStringBuilder { DataSource = path, Mode = SqliteOpenMode.ReadOnly, Pooling = false, DefaultTimeout = 1 }.ToString());
            try { connection.Open(); }
            catch { connection.Dispose(); throw; }
        }
        private long Scalar(string sql)
        { using var command = connection.CreateCommand(); command.CommandText = sql; return Convert.ToInt64(command.ExecuteScalar() ?? 0); }
        private HashSet<string> Columns(string table)
        {
            using var command = connection.CreateCommand(); command.CommandText = $"PRAGMA table_info(\"{table}\")";
            using var reader = command.ExecuteReader(); var columns = new HashSet<string>(StringComparer.Ordinal);
            while (reader.Read()) columns.Add(reader.GetString(1));
            return columns;
        }
        private IEnumerable<string[]> IndexColumns(string table)
        {
            var names = new List<string>();
            using (var command = connection.CreateCommand())
            {
                command.CommandText = "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=$table";
                command.Parameters.AddWithValue("$table", table);
                using var reader = command.ExecuteReader(); while (reader.Read()) names.Add(reader.GetString(0));
            }
            foreach (var name in names)
            {
                using var command = connection.CreateCommand(); command.CommandText = "PRAGMA index_info('" + name.Replace("'", "''") + "')";
                using var reader = command.ExecuteReader(); var fields = new List<string>();
                while (reader.Read()) if (!reader.IsDBNull(2)) fields.Add(reader.GetString(2));
                yield return fields.ToArray();
            }
        }
        private void Discover()
        {
            var names = new HashSet<string>(StringComparer.Ordinal);
            using (var command = connection.CreateCommand())
            {
                command.CommandText = "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('message','session_message','session','part')";
                using var reader = command.ExecuteReader(); while (reader.Read()) names.Add(reader.GetString(0));
            }
            hasPart = names.Contains("part") && new[] { "id", "message_id", "time_created", "data" }.All(Columns("part").Contains) &&
                IndexColumns("part").Any(fields => fields.FirstOrDefault() == "message_id");
            var hasSession = names.Contains("session") && new[] { "id", "time_updated" }.All(Columns("session").Contains);
            tables.Clear();
            foreach (var name in new[] { "session_message", "message" }.Where(names.Contains))
            {
                var columns = Columns(name);
                if (!new[] { "id", "session_id", "time_created", "time_updated", "data" }.All(columns.Contains) || name == "session_message" && !columns.Contains("type")) continue;
                var table = new Table(name, Scalar($"SELECT COALESCE(MAX(rowid),0) FROM \"{name}\""), cutoff);
                foreach (var fields in IndexColumns(name))
                {
                    if (fields.FirstOrDefault() == "time_created") table.IndexedHistory = true;
                    if (fields.Length > 1 && fields[0] == "session_id" && fields[1] == "time_created") table.SessionHistory = hasSession;
                }
                tables.Add(table);
            }
        }
        public IReadOnlyList<Telemetry> Poll()
        {
            bytesRemaining = 16 * 1024 * 1024;
            var schema = Scalar("PRAGMA schema_version");
            if (schema != schemaVersion) { Discover(); schemaVersion = schema; version = -1; }
            var currentVersion = Scalar("PRAGMA data_version");
            var changed = currentVersion != version;
            var moreInsertions = false;
            var result = new List<Telemetry>();
            foreach (var table in tables)
            {
                if (changed) foreach (var id in table.Pending.Keys) table.Enqueue(id);
                if (!table.ProbeDone)
                {
                    using var probe = connection.CreateCommand(); probe.CommandText = $"SELECT id FROM \"{table.Name}\" ORDER BY rowid DESC LIMIT 8";
                    using var reader = probe.ExecuteReader(); while (reader.Read()) table.Enqueue(reader.GetString(0));
                    table.ProbeDone = true;
                }
                if (changed)
                {
                    using var inserts = connection.CreateCommand(); inserts.CommandText = $"SELECT rowid,id,time_created FROM \"{table.Name}\" WHERE rowid>$row ORDER BY rowid LIMIT {Batch}";
                    inserts.Parameters.AddWithValue("$row", table.HighRow);
                    using var reader = inserts.ExecuteReader(); var count = 0;
                    while (reader.Read()) { count++; table.HighRow = reader.GetInt64(0); if (reader.GetInt64(2) >= cutoff) table.Enqueue(reader.GetString(1)); }
                    moreInsertions |= count == Batch;
                }
                if (!table.HistoryDone && table.Work.Count < 512) History(table);
                for (var count = 0; count < Batch && table.Work.TryPeek(out var id); count++)
                {
                    if (!Read(table, id, out var telemetry)) break;
                    table.Work.Dequeue(); table.Queued.Remove(id);
                    if (telemetry == null) { table.Pending.Remove(id); continue; }
                    HasTelemetry = true;
                    if (telemetry.Record == null && telemetry.Updated.ToUnixTimeMilliseconds() >= cutoff) table.Pending[id] = telemetry.Updated;
                    else table.Pending.Remove(id);
                    result.Add(telemetry);
                }
                foreach (var id in table.Pending.Where(p => p.Value.ToUnixTimeMilliseconds() < cutoff).Select(p => p.Key).ToArray()) table.Pending.Remove(id);
                Trim(table.Pending, MaximumPending, p => p.Value);
            }
            version = moreInsertions ? -1 : currentVersion;
            return result;
        }
        private void History(Table table)
        {
            if (!table.IndexedHistory && table.SessionHistory && table.Sessions.Count == 0 && !table.SessionsDone)
            {
                using var sessions = connection.CreateCommand(); sessions.CommandText = "SELECT rowid,id,time_updated FROM session WHERE rowid>$row ORDER BY rowid LIMIT 32";
                sessions.Parameters.AddWithValue("$row", table.SessionRow);
                using var reader = sessions.ExecuteReader(); var count = 0;
                while (reader.Read()) { count++; table.SessionRow = reader.GetInt64(0); if (reader.GetInt64(2) >= cutoff) table.Sessions.Enqueue(reader.GetString(1)); }
                table.SessionsDone = count < 32;
            }
            if (!table.IndexedHistory && table.SessionHistory && table.Sessions.Count == 0) { table.HistoryDone = table.SessionsDone; return; }
            using var command = connection.CreateCommand();
            if (table.IndexedHistory || table.SessionHistory)
            {
                command.CommandText = $"SELECT id,time_created FROM \"{table.Name}\" WHERE " + (!table.IndexedHistory ? "session_id=$session AND " : "") +
                    $"time_created >= $cutoff AND (time_created>$time OR (time_created=$time AND id>$id)) ORDER BY time_created,id LIMIT {Batch}";
                command.Parameters.AddWithValue("$cutoff", cutoff); command.Parameters.AddWithValue("$time", table.HistoryTime); command.Parameters.AddWithValue("$id", table.HistoryId);
                if (!table.IndexedHistory) command.Parameters.AddWithValue("$session", table.Sessions.Peek());
                using var reader = command.ExecuteReader(); var count = 0;
                while (reader.Read()) { count++; table.HistoryId = reader.GetString(0); table.HistoryTime = reader.GetInt64(1); table.Enqueue(table.HistoryId); }
                if (count < Batch)
                {
                    if (table.IndexedHistory) table.HistoryDone = true;
                    else { table.Sessions.Dequeue(); table.HistoryTime = cutoff; table.HistoryId = ""; table.HistoryDone = table.SessionsDone && table.Sessions.Count == 0; }
                }
            }
            else
            {
                command.CommandText = $"SELECT rowid,id,time_created FROM \"{table.Name}\" WHERE rowid<$row ORDER BY rowid DESC LIMIT {Batch}";
                command.Parameters.AddWithValue("$row", table.FallbackRow);
                using var reader = command.ExecuteReader(); var count = 0;
                while (reader.Read()) { count++; table.FallbackRow = reader.GetInt64(0); if (reader.GetInt64(2) >= cutoff) table.Enqueue(reader.GetString(1)); }
                table.HistoryDone = count < Batch;
            }
        }
        private bool Read(Table table, string id, out Telemetry? telemetry)
        {
            telemetry = null;
            using var size = connection.CreateCommand(); size.CommandText = $"SELECT length(CAST(data AS BLOB)) FROM \"{table.Name}\" WHERE id=$id"; size.Parameters.AddWithValue("$id", id);
            var length = size.ExecuteScalar();
            if (length == null || length is DBNull) return true;
            var bytes = Convert.ToInt64(length);
            if (bytes > MaxJsonBytes) return true;
            if (bytes + (table.Current ? 0 : MaxJsonBytes) > bytesRemaining) return false;
            bytesRemaining -= (int)bytes;
            using var command = connection.CreateCommand();
            command.CommandText = $"SELECT session_id,time_updated,json_object('time',json_extract(data,'$.time'),'model',json_extract(data,'$.model'),'modelID',json_extract(data,'$.modelID'),'providerID',json_extract(data,'$.providerID'),'tokens',json_extract(data,'$.tokens'),'finish',json_extract(data,'$.finish'),'error',CASE WHEN json_type(data,'$.error') NOT IN ('null') THEN 1 ELSE NULL END,'summary',json_extract(data,'$.summary')) FROM \"{table.Name}\" WHERE id=$id AND json_valid(data)=1 AND " + (table.Current ? "type='assistant'" : "json_extract(data,'$.role')='assistant'");
            command.Parameters.AddWithValue("$id", id);
            string session, json; DateTimeOffset? updated;
            using (var reader = command.ExecuteReader())
            {
                if (!reader.Read()) return true;
                session = reader.GetString(0); updated = Milliseconds(reader, 1); json = reader.GetString(2);
            }
            using var document = JsonDocument.Parse(json);
            var parts = new List<Part>(); var complete = true;
            if (table.Current)
            {
                using var content = connection.CreateCommand();
                content.CommandText = $"SELECT json_object('type',json_extract(value,'$.type'),'time',json_extract(value,'$.time'),'state',json_object('status',json_extract(value,'$.state.status'))) FROM \"{table.Name}\",json_each(data,'$.content') WHERE \"{table.Name}\".id=$id LIMIT 129";
                content.Parameters.AddWithValue("$id", id);
                using var reader = content.ExecuteReader();
                while (reader.Read())
                {
                    if (parts.Count == 128) { complete = false; break; }
                    using var part = JsonDocument.Parse(reader.GetString(0)); parts.Add(Part.Read(part.RootElement, null));
                }
            }
            else if (hasPart)
            {
                var headers = new List<(string Id, long Bytes)>();
                using (var headersCommand = connection.CreateCommand())
                {
                    headersCommand.CommandText = "SELECT id,length(CAST(data AS BLOB)) FROM part WHERE message_id=$id ORDER BY id LIMIT 129";
                    headersCommand.Parameters.AddWithValue("$id", id);
                    using var reader = headersCommand.ExecuteReader(); while (reader.Read()) headers.Add((reader.GetString(0), reader.GetInt64(1)));
                }
                if (headers.Count > 128) complete = false;
                var partBytes = 0L;
                foreach (var header in headers.Take(128))
                {
                    if (header.Bytes > MaxJsonBytes || partBytes + header.Bytes > MaxJsonBytes) { complete = false; continue; }
                    if (header.Bytes > bytesRemaining) return false;
                    bytesRemaining -= (int)header.Bytes; partBytes += header.Bytes;
                    using var partCommand = connection.CreateCommand();
                    partCommand.CommandText = "SELECT time_created,json_object('type',json_extract(data,'$.type'),'time',json_extract(data,'$.time'),'synthetic',json_extract(data,'$.synthetic'),'ignored',json_extract(data,'$.ignored'),'state',json_object('status',json_extract(data,'$.state.status'))) FROM part WHERE id=$id AND json_valid(data)=1";
                    partCommand.Parameters.AddWithValue("$id", header.Id);
                    using var reader = partCommand.ExecuteReader();
                    if (!reader.Read()) { complete = false; continue; }
                    using var part = JsonDocument.Parse(reader.GetString(1)); parts.Add(Part.Read(part.RootElement, Milliseconds(reader, 0)));
                }
            }
            else complete = false;
            telemetry = Telemetry.Read(id, session, document.RootElement, parts, table.Current, updated, complete);
            return true;
        }
        private static DateTimeOffset? Milliseconds(SqliteDataReader reader, int index)
        {
            if (reader.IsDBNull(index) || !long.TryParse(Convert.ToString(reader.GetValue(index), System.Globalization.CultureInfo.InvariantCulture), out var value) || value <= 0 || value >= 253402300799999) return null;
            return DateTimeOffset.FromUnixTimeMilliseconds(value);
        }
        public void Dispose() => connection.Dispose();
    }
}
