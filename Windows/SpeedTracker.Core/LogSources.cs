using System.Text.Json;
using System.Text.RegularExpressions;
using ZstdSharp;

namespace SpeedTracker.Core;

public sealed record LogSource(string Harness, string Root);

public static class LogDiscovery
{
    // The Windows profile, plus the home folders of WSL distributions that are already mounted.
    public static IReadOnlyList<string> Homes(string? home = null)
    {
        home ??= Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        var homes = new List<string> { home };
        // Only already-mounted WSL filesystem views; no wsl.exe call or distro startup.
        if (OperatingSystem.IsWindows() && Directory.Exists(@"\\wsl.localhost"))
        {
            foreach (var distro in Directories(@"\\wsl.localhost"))
                foreach (var user in Directories(Path.Combine(distro, "home"))) homes.Add(user);
        }
        return homes.Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
    }
    public static IReadOnlyList<LogSource> DefaultSources(string? home = null)
    {
        home ??= Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        var homes = Homes(home);
        var sources = new List<LogSource>();
        var dshOverride = Environment.GetEnvironmentVariable("DSH_HOME")?.Trim();
        foreach (var user in homes)
        {
            sources.AddRange(new[]
            {
                new LogSource("Claude Code", Path.Combine(user, ".claude", "projects")),
                new LogSource("Codex", Path.Combine(user, ".codex", "sessions")),
                new LogSource("OMP", Path.Combine(user, ".omp", "agent", "sessions")),
                new LogSource("Pi", Path.Combine(user, ".pi", "agent", "sessions")),
                new LogSource("opencode", Path.Combine(user, ".local", "share", "opencode")),
                new LogSource("Gemini CLI", Path.Combine(user, ".gemini", "tmp")),
                new LogSource("Antigravity", Path.Combine(user, ".gemini", "antigravity", "conversations"))
            });
            if (user != home || string.IsNullOrEmpty(dshOverride)) sources.Add(new("DeepSeek CLI", Path.Combine(user, ".dsh", "sessions")));
        }
        if (!string.IsNullOrEmpty(dshOverride))
        {
            if (dshOverride == "~") dshOverride = home;
            else if (dshOverride.StartsWith("~/", StringComparison.Ordinal) || dshOverride.StartsWith("~\\", StringComparison.Ordinal)) dshOverride = Path.Combine(home, dshOverride[2..]);
            sources.Add(new("DeepSeek CLI", Path.Combine(dshOverride, "sessions")));
        }
        if (Environment.GetEnvironmentVariable("XDG_DATA_HOME") is string xdg && xdg.Length > 0) sources.Add(new("opencode", Path.Combine(xdg, "opencode")));
        foreach (var appData in new[] { Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData) }.Where(p => p.Length > 0))
        {
            sources.Add(new("opencode", Path.Combine(appData, "opencode")));
        }
        return Array.AsReadOnly(sources.Distinct().ToArray());
    }
    internal static IEnumerable<string> Directories(string root)
    {
        try { return Directory.GetDirectories(root); }
        catch (IOException) { return Array.Empty<string>(); }
        catch (UnauthorizedAccessException) { return Array.Empty<string>(); }
    }
    internal static IEnumerable<string> Files(string root, string pattern)
    {
        // Retain a lazy DFS cursor, not all paths in a potentially large session directory.
        var stack = new Stack<string>(); stack.Push(root);
        while (stack.TryPop(out var directory))
        {
            foreach (var file in Entries(directory, pattern, false)) yield return file;
            foreach (var child in Entries(directory, "*", true))
            {
                bool link;
                try { link = (File.GetAttributes(child) & FileAttributes.ReparsePoint) != 0; }
                catch (IOException) { continue; }
                catch (UnauthorizedAccessException) { continue; }
                if (!link) stack.Push(child);
            }
        }
    }
    private static IEnumerable<string> Entries(string root, string pattern, bool directories)
    {
        IEnumerator<string> iterator;
        try { iterator = (directories ? Directory.EnumerateDirectories(root, pattern) : Directory.EnumerateFiles(root, pattern)).GetEnumerator(); }
        catch (IOException) { yield break; }
        catch (UnauthorizedAccessException) { yield break; }
        using (iterator)
        {
            while (true)
            {
                string current;
                try { if (!iterator.MoveNext()) break; current = iterator.Current; }
                catch (IOException) { break; }
                catch (UnauthorizedAccessException) { break; }
                yield return current;
            }
        }
    }
}

internal sealed class SessionLogWatcher : IDisposable
{
    private readonly LogSource[] sources;
    private readonly Dictionary<string, Tail> tails = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> errors = new(StringComparer.Ordinal);
    private DateTimeOffset lastScan = DateTimeOffset.MinValue;
    private readonly Dictionary<string, string> discoveryErrors = new(StringComparer.Ordinal);
    private int pollCursor;
    private Tail[] tailOrder = Array.Empty<Tail>();
    private readonly HashSet<string> recentKeys = new(StringComparer.Ordinal);
    private readonly Queue<string> keyOrder = new();
    private static readonly Regex GenerationName = new(@"^session(?:\.v([1-9][0-9]*))?\.jsonl(?:\.zstd)?$", RegexOptions.CultureInvariant);
    public SessionLogWatcher(IEnumerable<LogSource> sources) { this.sources = sources.Where(s => s.Harness is not ("opencode" or "Antigravity")).ToArray(); }
    public IReadOnlyDictionary<string, string> Errors => errors;
    public bool HasLogs(string harness) => tails.Values.Any(t => t.Ready && !t.Failed && t.Parser.Harness == harness && t.Parser.Supported);
    public IReadOnlyList<LiveCall> Active(DateTimeOffset now) => tails.Values.Where(t => t.Ready && !t.Failed && t.Parser.AwaitingResponse && t.Parser.LastRequestAt is DateTimeOffset start && start <= now && now - start < TimeSpan.FromMinutes(10) && now - t.LastGrowth < TimeSpan.FromMinutes(10)).Select(t => new LiveCall(t.Path, t.Parser.Harness, t.Parser.CurrentModel ?? "unknown", t.Parser.CurrentProvider, t.Parser.FirstTokenAt == null ? "Waiting" : t.Parser.FirstVisibleAt == null ? "Thinking" : "Streaming", t.Parser.LastRequestAt!.Value, t.LastGrowth, t.Parser.FirstTokenAt.HasValue ? (t.Parser.FirstTokenAt.Value - t.Parser.LastRequestAt!.Value).TotalSeconds : null)).ToArray();
    public IReadOnlyList<LogRecord> Poll(DateTimeOffset now)
    {
        if (now - lastScan >= TimeSpan.FromSeconds(4)) { Scan(now); lastScan = now; }
        var records = new List<LogRecord>();
        errors.Clear();
        foreach (var pair in discoveryErrors) errors[pair.Key] = pair.Value;
        var budget = 16 * 1024 * 1024;
        var work = tailOrder;
        var visited = 0;
        while (visited < work.Length && budget > 0)
        {
            var tail = work[(pollCursor + visited) % work.Length];
            visited++;
            try { tail.Poll(now, ref budget, records); }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidDataException or ZstdException)
            { tail.MarkFailed(ex is InvalidDataException or ZstdException); errors[tail.Parser.Harness] = $"Session telemetry unavailable: {ex.GetType().Name}."; }
        }
        if (work.Length > 0) pollCursor = (pollCursor + visited) % work.Length;
        foreach (var tail in work)
        {
            if (tail.Failed) errors.TryAdd(tail.Parser.Harness, "Session telemetry unavailable until its source is repaired.");
            else if (tail.Parser.Limitation is string limitation) errors.TryAdd(tail.Parser.Harness, limitation);
        }
        var kept = 0;
        for (var index = 0; index < records.Count; index++)
        {
            var record = records[index];
            if (!recentKeys.Add(record.Key)) continue;
            keyOrder.Enqueue(record.Key);
            if (keyOrder.Count > 8192) recentKeys.Remove(keyOrder.Dequeue());
            records[kept++] = record;
        }
        if (kept < records.Count) records.RemoveRange(kept, records.Count - kept);
        return records;
    }
    private void Scan(DateTimeOffset now)
    {
        discoveryErrors.Clear();
        foreach (var source in sources)
        {
            var files = LogDiscovery.Files(source.Root, source.Harness == "DeepSeek CLI" ? "session*.jsonl*" : "*.jsonl");
            if (source.Harness == "DeepSeek CLI")
            {
                var selected = new List<string>();
                foreach (var group in files.Select(p => (Path: p, Match: GenerationName.Match(System.IO.Path.GetFileName(p)))).Where(p => p.Match.Success).GroupBy(p => System.IO.Path.GetDirectoryName(p.Path)))
                {
                    var generations = group.Select(p => (p.Path, Version: p.Match.Groups[1].Success && int.TryParse(p.Match.Groups[1].Value, out var version) ? version : p.Match.Groups[1].Success ? int.MaxValue : 0)).ToArray();
                    var highest = generations.Max(p => p.Version);
                    var candidates = generations.Where(p => p.Version == highest).ToArray();
                    if (candidates.Length != 1) { discoveryErrors[source.Harness] = "Ambiguous DeepSeek raw/compressed generation; no telemetry guessed."; continue; }
                    if (highest > 4) { discoveryErrors[source.Harness] = $"Unsupported DeepSeek session generation v{highest}."; continue; }
                    selected.Add(candidates[0].Path);
                }
                files = selected;
                var retained = files.ToHashSet(StringComparer.Ordinal);
                foreach (var obsolete in tails.Values.Where(t => t.Parser.Harness == source.Harness && t.Path.StartsWith(source.Root + System.IO.Path.DirectorySeparatorChar, StringComparison.Ordinal) && !retained.Contains(t.Path)).ToArray()) { tails.Remove(obsolete.Path); obsolete.Dispose(); }
            }
            foreach (var path in files)
            {
                if (tails.ContainsKey(path)) continue;
                try
                {
                    if (File.GetLastWriteTimeUtc(path) < now.AddDays(-7).UtcDateTime) continue;
                    tails[path] = new Tail(path, MakeParser(source.Harness), now);
                }
                catch (IOException) { }
                catch (UnauthorizedAccessException) { }
            }
        }
        foreach (var old in tails.Values.Where(t => now - t.LastGrowth > TimeSpan.FromDays(7)).ToArray()) { tails.Remove(old.Path); old.Dispose(); }
        tailOrder = tails.Values.ToArray();
    }
    private static SessionLogParser MakeParser(string harness) => harness switch
    {
        "Claude Code" => new ClaudeCodeLogParser(), "Codex" => new CodexLogParser(), "DeepSeek CLI" => new DeepSeekLogParser(), "Gemini CLI" => new GeminiCliLogParser(), _ => new OMPLogParser(harness)
    };
    public void Dispose() { foreach (var tail in tails.Values) tail.Dispose(); tails.Clear(); tailOrder = Array.Empty<Tail>(); }
    private sealed class Tail : IDisposable
    {
        public string Path { get; }
        public SessionLogParser Parser { get; private set; }
        public DateTimeOffset LastGrowth { get; private set; }
        public bool Ready { get; private set; }
        public bool Failed { get; private set; }
        private FileStream? file;
        private Stream? decoder;
        private BoundedLines lines = new();
        private long previousSize;
        private DateTime previousWrite;
        private DateTime creation;
        private bool preflight;
        private readonly int? expectedVersion;
        private readonly byte[] probe = new byte[64];
        private int probeCount;
        private long probeOffset;
        public Tail(string path, SessionLogParser parser, DateTimeOffset now)
        {
            Path = path;
            expectedVersion = parser is DeepSeekLogParser ? Generation(path) : null;
            Parser = parser is DeepSeekLogParser ? new DeepSeekLogParser(expectedVersion: expectedVersion) : parser;
            LastGrowth = now;
            preflight = parser is DeepSeekLogParser;
            Ready = !preflight;
        }
        private static int? Generation(string path)
        {
            var match = GenerationName.Match(System.IO.Path.GetFileName(path));
            return match.Success ? match.Groups[1].Success ? int.Parse(match.Groups[1].Value) : 0 : null;
        }
        private void Open()
        {
            file = new FileStream(Path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete, 64 * 1024, FileOptions.SequentialScan);
            decoder = Path.EndsWith(".zstd", StringComparison.Ordinal) ? new DecompressionStream(file, checkEndOfStream: false, leaveOpen: true) : file;
        }
        private void Reset()
        {
            Dispose();
            lines = new();
            Parser = expectedVersion != null ? new DeepSeekLogParser(expectedVersion: expectedVersion) : MakeParser(Parser.Harness);
            preflight = Parser is DeepSeekLogParser;
            Ready = !preflight;
            Failed = false;
            probeCount = 0;
        }
        public void MarkFailed(bool corrupt = false)
        {
            Failed = true;
            Ready = false;
            if (!corrupt) { Dispose(); return; }
            try { CaptureProbe(); }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
        }
        public void Poll(DateTimeOffset now, ref int budget, List<LogRecord> result)
        {
            var info = new FileInfo(Path);
            if (!info.Exists) { MarkFailed(); return; }
            var changed = info.Length < previousSize || info.Length == previousSize && info.LastWriteTimeUtc != previousWrite || creation != default && creation != info.CreationTimeUtc;
            if (!changed && probeCount > 0 && info.Length >= probeOffset + probeCount)
            {
                using var current = new FileStream(Path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
                current.Position = probeOffset;
                Span<byte> bytes = stackalloc byte[64];
                var count = current.Read(bytes[..probeCount]);
                changed = count != probeCount || !bytes[..count].SequenceEqual(probe.AsSpan(0, probeCount));
            }
            if (changed || Failed && file == null) Reset();
            creation = info.CreationTimeUtc;
            if (Failed) return;
            if (file == null) Open();
            var grew = info.Length != previousSize || info.LastWriteTimeUtc != previousWrite;
            previousSize = info.Length;
            previousWrite = info.LastWriteTimeUtc;
            using var counted = new CountedStream(decoder!);
            bool reachedEnd;
            try
            {
                reachedEnd = lines.Drain(counted, line =>
                {
                    if (line == null) return;
                    try
                    {
                        using var doc = JsonDocument.Parse(line);
                        result.AddRange(Parser.Ingest(doc.RootElement));
                        if (Parser is DeepSeekLogParser deep)
                        {
                            preflight = deep.NeedsInheritedScan;
                            Ready = deep.HasSupportedHeader && !preflight;
                        }
                    }
                    catch (JsonException) { }
                }, Math.Min(4 * 1024 * 1024, budget));
            }
            finally { budget -= counted.ReadBytes; }
            if (preflight && reachedEnd && Parser is DeepSeekLogParser deep && deep.InheritedCut is long cut)
            {
                // Find the last tagged inherited boundary before admitting any ancestor telemetry.
                Dispose();
                lines = new();
                Parser = new DeepSeekLogParser(cut, expectedVersion);
                preflight = false;
                Ready = false;
                probeCount = 0;
                Open();
            }
            CaptureProbe();
            if (grew) LastGrowth = new DateTimeOffset(info.LastWriteTimeUtc, TimeSpan.Zero);
            if (Ready && reachedEnd && !grew) result.AddRange(Parser.Flush(now - LastGrowth));
        }
        private void CaptureProbe()
        {
            if (file == null || file.Position == 0) return;
            probeCount = (int)Math.Min(probe.Length, file.Position);
            probeOffset = file.Position - probeCount;
            using var current = new FileStream(Path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            current.Position = probeOffset;
            probeCount = current.Read(probe, 0, probeCount);
        }
        public void Dispose()
        {
            if (decoder != file) decoder?.Dispose();
            file?.Dispose();
            decoder = null;
            file = null;
        }
        private sealed class CountedStream(Stream source) : Stream
        {
            public int ReadBytes { get; private set; }
            public override int Read(byte[] buffer, int offset, int count)
            { var read = source.Read(buffer, offset, count); ReadBytes += read; return read; }
            public override bool CanRead => true;
            public override bool CanSeek => false;
            public override bool CanWrite => false;
            public override long Length => throw new NotSupportedException();
            public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
            public override void Flush() => throw new NotSupportedException();
            public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
            public override void SetLength(long value) => throw new NotSupportedException();
            public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        }
    }
}
