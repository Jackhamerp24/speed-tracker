using System.Diagnostics;
using System.Text.Json;

namespace SpeedTracker.Core;

public sealed class TrackerService : IDisposable
{
    private static readonly string[] KnownHarnesses = new[] { "claude", "codex", "omp", "pi", "opencode", "dsh", "gemini", "qwen", "aider", "goose", "crush" }
        .Select(name => HarnessNames.Classify(name)!).Distinct(StringComparer.Ordinal).ToArray();
    private const ulong RequestBytes = 300;
    private const ulong ResponseBytes = 300;
    private const double UploadGrace = .3;
    private const double StreamBytesPerSecond = 400;
    private readonly object gate = new();
    private readonly object pollGate = new();
    private readonly HistoryStore history;
    private readonly IFlowSampler? sampler;
    private readonly SessionLogWatcher logs;
    private readonly OpenCodeLogWatcher openCode;
    private readonly AntigravityLogWatcher antigravity;
    private readonly Dictionary<string, NetworkFlow> flows = new(StringComparer.Ordinal);
    private readonly Dictionary<string, LiveCall> proxyCalls = new(StringComparer.Ordinal);
    private readonly Queue<RequestRecord> pending = new();
    private readonly HashSet<string> knownHarnesses = new(KnownHarnesses, StringComparer.Ordinal);
    private readonly List<RequestRecord> recent;
    private IReadOnlyList<LiveCall> active = Array.Empty<LiveCall>();
    private IReadOnlyList<HarnessStatus> harnesses = Array.Empty<HarnessStatus>();
    private Timer? timer;
    private bool disposed;
    private bool networkEnabled;
    private string? target;
    private string? pollError;
    private DateTimeOffset processScan = DateTimeOffset.MinValue;
    private readonly HashSet<string> running = new(StringComparer.Ordinal);
    // Harnesses whose command or folder exists on this machine. Looked up at start and once a minute.
    private readonly Func<IReadOnlySet<string>> findInstalled;
    private IReadOnlySet<string> installed;
    private DateTimeOffset presenceScan;
    // When the network sampler last saw each harness's process: a harness seen moments ago is on this machine.
    private readonly Dictionary<string, DateTimeOffset> sampled = new(StringComparer.Ordinal);
    public event Action? Changed;
    // findInstalled reports which harnesses are installed; tests replace it.
    public TrackerService(HistoryStore history, IFlowSampler? sampler = null, IEnumerable<LogSource>? sources = null, string? userHome = null, Func<IReadOnlySet<string>>? findInstalled = null)
    {
        this.history = history; this.sampler = sampler;
        this.findInstalled = findInstalled ?? (() => HarnessPresence.Installed(LogDiscovery.Homes(userHome)));
        // Before anything else, see which harnesses this machine has, so the first list offered holds only those.
        installed = this.findInstalled(); presenceScan = DateTimeOffset.UtcNow;
        var discovered = sources?.ToArray() ?? LogDiscovery.DefaultSources(userHome).ToArray();
        logs = new(discovered); openCode = new(discovered.Where(s => s.Harness == "opencode").Select(s => s.Root));
        antigravity = new(discovered.Where(s => s.Harness == "Antigravity").Select(s => s.Root));
        recent = history.Read().OrderByDescending(r => r.StartedAt).ThenBy(r => r.Id).ToList();
        foreach (var name in discovered.Select(s => s.Harness).Concat(recent.Select(r => r.Harness)))
            if (ValidHarnessName(name)) knownHarnesses.Add(name);
        var preference = Path.Combine(history.Home, "live-target.json");
        if (File.Exists(preference))
        {
            try
            {
                var info = new FileInfo(preference);
                if (info.Length <= 16384)
                {
                    using var document = JsonDocument.Parse(File.ReadAllText(preference));
                    if (document.RootElement.ValueKind == JsonValueKind.String) target = document.RootElement.GetString();
                    else if (document.RootElement.ValueKind == JsonValueKind.Object)
                    {
                        var saved = document.RootElement.Deserialize<LiveTargetPreference>(RequestRecord.JsonOptions);
                        if (saved?.Harnesses != null)
                            foreach (var name in saved.Harnesses)
                                if (ValidHarnessName(name)) knownHarnesses.Add(name);
                        target = saved?.Target;
                    }
                }
                if (target != null && !knownHarnesses.Contains(target)) target = null;
            }
            catch (JsonException) { target = null; }
        }
        // Session logs have not been read yet, so the first list is what is installed. A pinned target stays listed so it can be changed.
        harnesses = Array.AsReadOnly(knownHarnesses.Where(h => installed.Contains(h) || h == target).Order(StringComparer.Ordinal).Select(h => new HarnessStatus(h, false, false, "No readable supported session telemetry found. Processes alone do not activate tracking.", installed.Contains(h))).ToArray());
        history.Recorded += OnRecorded;
    }
    public IReadOnlyList<LiveCall> Active { get { lock (gate) return active; } }
    public IReadOnlyList<HarnessStatus> Harnesses { get { lock (gate) return harnesses; } }
    public RequestRecord? HeldRecord { get { lock (gate) return Selected().FirstOrDefault(); } }
    public double? HeldRate { get { lock (gate) return LastSpeed()?.Tps; } }
    public double? HeldTTFT { get { lock (gate) return LastLatency()?.Ttft; } }
    public bool HeldRateEstimated { get { lock (gate) return LastSpeed()?.TokensEstimated ?? false; } }
    public bool HeldTTFTEstimated { get { lock (gate) return LastLatency() is RequestRecord record && (record.TokensEstimated || record.Source == "network"); } }
    public bool EnhancedNetworkAvailable => sampler?.Available ?? false;
    public bool EnhancedNetworkEnabled { get { lock (gate) return networkEnabled && EnhancedNetworkAvailable; } }
    public string NetworkStatus
    {
        get
        {
            lock (gate)
            {
                if (pollError != null) return pollError;
                if (!networkEnabled) return "Standard-user log-first tracking. Live timing is absent where a harness does not publish it. Enhanced network collection is off.";
                return sampler?.Status ?? "Enhanced network collection is unavailable on this platform.";
            }
        }
    }
    public string? Target
    {
        get { lock (gate) return target; }
        set
        {
            if (value != null && !ValidHarnessName(value)) throw new ArgumentException("Unknown live harness target.", nameof(value));
            lock (gate)
            {
                ObjectDisposedException.ThrowIf(disposed, this);
                if (value != null && !knownHarnesses.Contains(value)) throw new ArgumentException("Unknown live harness target.", nameof(value));
                if (target == value) return;
                Directory.CreateDirectory(history.Home);
                var path = Path.Combine(history.Home, "live-target.json"); var temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
                try { File.WriteAllText(temporary, JsonSerializer.Serialize(new LiveTargetPreference(value, knownHarnesses.Order(StringComparer.Ordinal).ToArray()), RequestRecord.JsonOptions)); File.Move(temporary, path, true); }
                finally { if (File.Exists(temporary)) File.Delete(temporary); }
                target = value;
            }
            Changed?.Invoke();
        }
    }
    private static bool ValidHarnessName(string? name) => !string.IsNullOrWhiteSpace(name) && name.Length <= 128 && name == name.Trim() && !name.Any(char.IsControl);
    private sealed record LiveTargetPreference(string? Target, string[]? Harnesses);
    private void ObserveHarness(string name)
    {
        if (ValidHarnessName(name)) lock (gate) knownHarnesses.Add(name);
    }
    public IReadOnlyList<RequestRecord> GetRecent(int count, string? harness = null)
    {
        lock (gate) return Array.AsReadOnly(recent.Where(r => harness == null || r.Harness == harness).Take(Math.Max(0, count)).ToArray());
    }
    private IEnumerable<RequestRecord> Selected() => recent.Where(r => target == null || r.Harness == target);
    private RequestRecord? LastSpeed() => Selected().FirstOrDefault(r => !r.Aborted && r.Status < 400 && r.Tps is double rate && double.IsFinite(rate) && rate >= 0);
    private RequestRecord? LastLatency() => Selected().FirstOrDefault(DashboardSummary.ValidLatency);
    private void OnRecorded(RequestRecord record)
    {
        lock (gate)
        {
            if (disposed) return;
            if (ValidHarnessName(record.Harness)) knownHarnesses.Add(record.Harness);
            var position = recent.FindIndex(r => r.StartedAt < record.StartedAt);
            recent.Insert(position < 0 ? recent.Count : position, record);
        }
        Changed?.Invoke();
    }
    public void Start()
    {
        lock (gate)
        {
            ObjectDisposedException.ThrowIf(disposed, this);
            timer ??= new Timer(_ => TimerPoll(), null, TimeSpan.Zero, TimeSpan.FromMilliseconds(500));
        }
    }
    private void TimerPoll()
    {
        try { Poll(); }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidOperationException or System.Text.Json.JsonException)
        {
            lock (gate) pollError = $"Telemetry/history error: {ex.GetType().Name}. Completed history was not silently discarded.";
            Changed?.Invoke();
        }
    }
    public void SetEnhancedNetwork(bool enabled)
    {
        lock (pollGate)
        {
            lock (gate)
            {
                ObjectDisposedException.ThrowIf(disposed, this);
                networkEnabled = enabled && EnhancedNetworkAvailable;
                if (!networkEnabled) active = Array.AsReadOnly(active.Where(c => !c.Id.StartsWith("network:", StringComparison.Ordinal)).ToArray());
            }
            flows.Clear();
        }
        Changed?.Invoke();
    }
    public void UpdateProxy(LiveCall call)
    {
        lock (pollGate)
        {
            lock (gate)
            {
                ObjectDisposedException.ThrowIf(disposed, this);
                proxyCalls[call.Id] = call;
                if (ValidHarnessName(call.Harness)) knownHarnesses.Add(call.Harness);
                active = Array.AsReadOnly(active.Where(c => c.Id != call.Id).Append(call).OrderByDescending(c => c.StartedAt).ToArray());
            }
        }
        Changed?.Invoke();
    }
    public void FinishProxy(string id, RequestRecord? record)
    {
        var changed = false;
        try
        {
            lock (pollGate)
            {
                lock (gate)
                {
                    ObjectDisposedException.ThrowIf(disposed, this);
                    proxyCalls.Remove(id);
                    active = Array.AsReadOnly(active.Where(c => c.Id != id).ToArray());
                    changed = true;
                }
                if (record != null) pending.Enqueue(record);
                PersistPending();
            }
        }
        finally { if (changed) Changed?.Invoke(); }
    }
    public void Poll(DateTimeOffset? now = null)
    {
        var wall = now ?? DateTimeOffset.UtcNow;
        bool changed;
        lock (pollGate)
        {
            lock (gate) { if (disposed) return; }
            AcceptLogs(logs.Poll(wall), wall);
            AcceptLogs(openCode.Poll(wall), wall);
            AcceptLogs(antigravity.Poll(wall), wall);
            var live = logs.Active(wall).Concat(openCode.Active(wall)).Concat(antigravity.Active(wall)).Concat(proxyCalls.Values).ToList();
            if (EnhancedNetworkEnabled) PollNetwork(wall, live);
            if (wall - processScan >= TimeSpan.FromSeconds(4)) { ScanProcesses(); processScan = wall; }
            // A harness can be installed while the app is open, so look again once a minute.
            if ((wall - presenceScan).Duration() >= TimeSpan.FromMinutes(1)) { installed = findInstalled(); presenceScan = wall; }
            string[] names; string? pinned;
            lock (gate) { names = knownHarnesses.Order(StringComparer.Ordinal).ToArray(); pinned = target; }
            // Only harnesses present on this machine are offered: installed, keeping readable session
            // logs, or running right now. One that is none of these cannot be chosen. The pinned target
            // stays listed so it can be changed.
            var statuses = names.Select(h =>
            {
                var hasLogs = h == "opencode" ? openCode.HasLogs : h == "Antigravity" ? antigravity.HasLogs : logs.HasLogs(h);
                var sourceError = h == "opencode" ? openCode.Limitation : h == "Antigravity" ? antigravity.Limitation : logs.Errors.GetValueOrDefault(h);
                var seen = sampled.TryGetValue(h, out var sampledAt) && (wall - sampledAt).Duration() < TimeSpan.FromMinutes(2);
                return new HarnessStatus(h, hasLogs, running.Contains(h) || seen || live.Any(c => c.Harness == h), sourceError ?? (hasLogs ? "Passive session logs. Metrics not exposed by this source stay unknown, never zero; provider labels are unverified." : "No readable supported session telemetry found. Processes alone do not activate tracking."), installed.Contains(h));
            }).Where(status => status.HasLogs || status.IsRunning || status.IsInstalled || status.Name == pinned).ToArray();
            var snapshot = live.OrderByDescending(c => c.StartedAt).ToArray();
            lock (gate)
            {
                changed = !active.SequenceEqual(snapshot) || !harnesses.SequenceEqual(statuses) || pollError != null;
                active = Array.AsReadOnly(snapshot); harnesses = Array.AsReadOnly(statuses); pollError = null;
            }
            PersistPending();
        }
        if (changed) Changed?.Invoke();
    }
    private void AcceptLogs(IReadOnlyList<LogRecord> records, DateTimeOffset wall)
    {
        foreach (var record in records.OrderBy(r => r.End))
            if (record.End >= wall.AddDays(-7) && record.End <= wall.AddMinutes(1))
            {
                ObserveHarness(record.Harness);
                pending.Enqueue(record.MakeRecord());
            }
    }
    private void PersistPending()
    {
        while (pending.TryPeek(out var record))
        {
            // Parser offsets and flow state already advanced. Keep this item and the
            // rest of the batch until persistence succeeds or confirms a duplicate.
            history.Append(record);
            pending.Dequeue();
        }
    }
    private void ScanProcesses()
    {
        running.Clear();
        foreach (var process in Process.GetProcesses())
        {
            using (process)
            {
                try
                {
                    var harness = HarnessNames.Classify(process.ProcessName);
                    if (harness != null) { running.Add(harness); ObserveHarness(harness); }
                }
                catch (Exception ex) when (ex is InvalidOperationException or System.ComponentModel.Win32Exception) { }
            }
        }
    }
    private void PollNetwork(DateTimeOffset now, List<LiveCall> live)
    {
        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var sample in sampler!.Sample())
        {
            if (!ValidHarnessName(sample.Harness)) continue;
            ObserveHarness(sample.Harness); sampled[sample.Harness] = now;
            var key = sample.ConnectionID ?? $"{sample.Pid}:{sample.Host}"; seen.Add(key);
            if (!flows.TryGetValue(key, out var flow)) { flows[key] = new(sample); continue; }
            if (sample.Received < flow.Previous.Received || sample.Sent < flow.Previous.Sent
                || sample.Pid != flow.Previous.Pid || sample.Host != flow.Previous.Host || sample.Harness != flow.Previous.Harness)
            {
                FinishNetwork(flow, flow.Previous);
                flows[key] = new(sample);
                continue;
            }
            var sent = sample.Sent - flow.Previous.Sent; var received = sample.Received - flow.Previous.Received;
            flow.Previous = sample;
            var covered = sample.Harness == "opencode" ? openCode.HasLogs : sample.Harness == "Antigravity" ? antigravity.HasLogs : logs.HasLogs(sample.Harness);
            if (covered) { flow.Reset(); continue; }

            // Expire the old response before applying this sample's upload, so a
            // keep-alive request arriving after a pause is not consumed by Reset.
            if (flow.Start.HasValue && (flow.Sustained && (now - flow.Last).TotalSeconds >= 2
                || (now - flow.Start.Value).TotalSeconds > 180 && (now - flow.Last).TotalSeconds > 90))
            {
                FinishNetwork(flow, sample);
                flow.Reset();
            }
            var uploading = sent >= RequestBytes;
            if (uploading)
            {
                if (flow.First.HasValue)
                {
                    if (!flow.Sustained && flow.Bytes < 8000 && (now - flow.First.Value).TotalSeconds < 1)
                    {
                        // An early small reply followed by more upload was a handshake.
                        flow.ResetResponse();
                    }
                    else
                    {
                        FinishNetwork(flow, sample);
                        flow.Reset();
                    }
                }
                else if (flow.Start.HasValue && (flow.LastUpload.HasValue && (now - flow.LastUpload.Value).TotalSeconds >= 3
                    || flow.Bytes > 0 && (now - flow.Last).TotalSeconds > 1))
                {
                    flow.Reset();
                }
                if (flow.Start == null) flow.Begin(now);
                flow.LastUpload = now;
            }
            var control = uploading || flow.LastUpload.HasValue && (now - flow.LastUpload.Value).TotalSeconds < UploadGrace;
            if (flow.Start.HasValue && received > 0 && !control)
            {
                flow.Last = now; flow.Bytes += received;
                if (flow.First == null && flow.Bytes >= ResponseBytes) flow.First = now;
            }
            if (flow.First.HasValue && !flow.Sustained) flow.Qualify(now, control ? 0 : received);
            // Network-only traffic is never enough to claim Waiting/Thinking.
            // Show only a proven stream, and keep its last measured rate through pauses.
            if (flow.IsStream)
            {
                var generation = (flow.Last - flow.First!.Value).TotalSeconds;
                var ratio = flow.TokenRatio;
                live.Add(new("network:" + flow.Id, sample.Harness, "unknown", sample.Host, "Streaming · network estimate", flow.Start!.Value, flow.Last,
                    (flow.First.Value - flow.Start.Value).TotalSeconds, flow.Bytes / ratio / generation, (int)Math.Min(int.MaxValue, flow.Bytes / ratio), true));
            }
        }
        foreach (var key in flows.Keys.Where(k => !seen.Contains(k)).ToArray()) { FinishNetwork(flows[key], flows[key].Previous); flows.Remove(key); }
    }
    private static double BytesPerToken(FlowSample sample) => ProviderIdentity.From(new RequestRecord { UpstreamHost = sample.Host }).Key == "anthropic" ? 22d : 180d;
    private void FinishNetwork(NetworkFlow flow, FlowSample sample)
    {
        if (!flow.IsStream) return;
        var generation = (flow.Last - flow.First!.Value).TotalSeconds;
        if (flow.Bytes / generation < StreamBytesPerSecond) return;
        var tokens = (int)Math.Min(int.MaxValue, Math.Round(flow.Bytes / flow.TokenRatio));
        if (tokens < 5) return;
        pending.Enqueue(new RequestRecord { Id = flow.Id, StartedAt = flow.Start!.Value, Harness = sample.Harness, Route = sample.Host, UpstreamHost = sample.Host, Model = "unknown", Streamed = true, Ttft = (flow.First.Value - flow.Start.Value).TotalSeconds, Ttfb = (flow.First.Value - flow.Start.Value).TotalSeconds, Generation = generation, Total = (flow.Last - flow.Start.Value).TotalSeconds, OutputTokens = tokens, TokensEstimated = true, Tps = tokens / generation, Source = "network", SourceKey = "network:" + flow.Id });
    }
    private sealed class NetworkFlow
    {
        public FlowSample Previous;
        public Guid Id;
        public readonly double TokenRatio;
        public DateTimeOffset? Start;
        public DateTimeOffset? First;
        public DateTimeOffset? LastUpload;
        public DateTimeOffset Last;
        public ulong Bytes;
        public bool Sustained;
        private readonly Queue<(DateTimeOffset Time, ulong Bytes)> recent = new();
        public bool IsStream => Start.HasValue && First.HasValue && Sustained && (Last - First.Value).TotalSeconds >= .5;
        public NetworkFlow(FlowSample sample) { Previous = sample; TokenRatio = BytesPerToken(sample); }
        public void Begin(DateTimeOffset now) { Start = now; Id = Guid.NewGuid(); Last = now; }
        public void Qualify(DateTimeOffset now, ulong received)
        {
            recent.Enqueue((now, received));
            while (recent.TryPeek(out var oldest) && (now - oldest.Time).TotalSeconds > 1.3) recent.Dequeue();
            var start = recent.Peek().Time;
            var duration = (now - start).TotalSeconds;
            if (duration < .6) return;
            var hits = 0; ulong bytes = 0; var first = true;
            foreach (var item in recent)
            {
                if (item.Bytes > 0) hits++;
                // The oldest sample's bytes arrived before the window it opens.
                if (!first) bytes += item.Bytes;
                first = false;
            }
            Sustained = hits >= 3 && hits >= .75 * recent.Count && bytes / duration >= StreamBytesPerSecond;
        }
        public void ResetResponse() { First = null; Bytes = 0; Sustained = false; recent.Clear(); }
        public void Reset() { Start = null; LastUpload = null; ResetResponse(); }
    }
    public void Dispose()
    {
        lock (pollGate)
        {
            lock (gate)
            {
                if (disposed) return;
                disposed = true; timer?.Dispose(); timer = null; active = Array.Empty<LiveCall>();
            }
            history.Recorded -= OnRecorded; logs.Dispose(); openCode.Dispose(); antigravity.Dispose(); sampler?.Dispose(); flows.Clear(); proxyCalls.Clear();
        }
    }
}
