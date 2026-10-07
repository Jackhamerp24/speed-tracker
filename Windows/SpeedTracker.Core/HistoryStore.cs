using System.Text.Json;

namespace SpeedTracker.Core;

public sealed class HistoryStore
{
    private readonly object gate = new();
    private readonly string path;
    private readonly List<RequestRecord> records = new();
    private readonly HashSet<string> keys = new(StringComparer.Ordinal);
    private long loadedLength = -1;
    private DateTime loadedWrite;
    private int skippedLines;
    public string Home { get; }
    public int SkippedLines { get { lock (gate) return skippedLines; } }
    public event Action<RequestRecord>? Recorded;

    public HistoryStore(string? home = null)
    {
        Home = Path.GetFullPath(home ?? Environment.GetEnvironmentVariable("SPEEDTRACKER_HOME") ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "SpeedTracker"));
        path = Path.Combine(Home, "history.jsonl");
    }
    public IReadOnlyList<RequestRecord> Read(DashboardFilter? filter = null)
    {
        lock (gate)
        {
            Load();
            return Array.AsReadOnly(records.Where(r => filter == null || filter.Includes(r)).ToArray());
        }
    }
    public bool Append(RequestRecord record)
    {
        lock (gate)
        {
            Load();
            if (keys.Contains(record.DedupKey)) return false;
            var line = JsonSerializer.SerializeToUtf8Bytes(record, RequestRecord.JsonOptions);
            Directory.CreateDirectory(Home);
            using (var stream = new FileStream(path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.Read))
            {
                // A valid last object without a newline must not merge with the next append.
                if (stream.Length > 0)
                {
                    stream.Position = stream.Length - 1;
                    if (stream.ReadByte() != '\n') stream.WriteByte((byte)'\n');
                }
                stream.Position = stream.Length;
                stream.Write(line);
                stream.WriteByte((byte)'\n');
                stream.Flush(true);
                loadedLength = stream.Length;
            }
            loadedWrite = File.GetLastWriteTimeUtc(path);
            keys.Add(record.DedupKey);
            records.Add(record);
        }
        Recorded?.Invoke(record);
        return true;
    }
    private void Load()
    {
        if (Directory.Exists(path)) throw new IOException("History path is a directory.");
        var info = new FileInfo(path);
        var length = info.Exists ? info.Length : 0;
        var modified = info.Exists ? info.LastWriteTimeUtc : default;
        if (loadedLength == length && loadedWrite == modified) return;
        var next = new List<RequestRecord>(); var seen = new HashSet<string>(StringComparer.Ordinal); var skipped = 0;
        if (info.Exists)
        {
            using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            var lines = new BoundedLines();
            lines.Drain(stream, line =>
            {
                if (line == null) { skipped++; return; }
                try
                {
                    using var doc = JsonDocument.Parse(line);
                    if (doc.RootElement.ValueKind != JsonValueKind.Object || !doc.RootElement.TryGetProperty("startedAt", out _) || !doc.RootElement.TryGetProperty("id", out _)) { skipped++; return; }
                    var record = doc.RootElement.Deserialize<RequestRecord>(RequestRecord.JsonOptions);
                    if (record == null) { skipped++; return; }
                    if (seen.Add(record.DedupKey)) next.Add(record);
                }
                catch (JsonException) { skipped++; }
            });
            lines.Finish(line =>
            {
                if (line == null) { skipped++; return; }
                try
                {
                    using var doc = JsonDocument.Parse(line);
                    if (!doc.RootElement.TryGetProperty("startedAt", out _) || !doc.RootElement.TryGetProperty("id", out _)) { skipped++; return; }
                    var record = doc.RootElement.Deserialize<RequestRecord>(RequestRecord.JsonOptions);
                    if (record == null) skipped++;
                    else if (seen.Add(record.DedupKey)) next.Add(record);
                }
                catch (JsonException) { skipped++; }
            });
        }
        records.Clear(); records.AddRange(next); keys.Clear(); keys.UnionWith(seen);
        skippedLines = skipped; loadedLength = length; loadedWrite = modified;
    }
}

// Bounds both telemetry tails and history lines; oversize prompt/tool rows are never retained.
internal sealed class BoundedLines
{
    public const int Maximum = 1024 * 1024;
    private readonly byte[] chunk = new byte[64 * 1024];
    private readonly MemoryStream pending = new();
    private bool oversized;
    public bool Drain(Stream stream, Action<byte[]?> consume, long budget = long.MaxValue)
    {
        while (budget > 0)
        {
            var count = stream.Read(chunk, 0, (int)Math.Min(chunk.Length, budget));
            if (count == 0) return true;
            budget -= count;
            var start = 0;
            for (var index = 0; index < count; index++)
            {
                if (chunk[index] != '\n') continue;
                Add(chunk.AsSpan(start, index - start));
                Consume(consume);
                start = index + 1;
            }
            Add(chunk.AsSpan(start, count - start));
        }
        return false;
    }
    private void Add(ReadOnlySpan<byte> bytes)
    {
        if (oversized) return;
        if (bytes.Length > Maximum - pending.Length) { oversized = true; pending.SetLength(0); }
        else pending.Write(bytes);
    }
    private void Consume(Action<byte[]?> consume)
    {
        if (oversized) consume(null);
        else if (pending.Length > 0) consume(pending.ToArray());
        pending.SetLength(0); oversized = false;
    }
    public void Finish(Action<byte[]?> consume)
    {
        if (oversized || pending.Length > 0) Consume(consume);
    }
}
