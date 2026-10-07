using System.Text.Json;
using SpeedTracker.Core;

internal static class FlowRegression
{
    public static void Run(Action<bool, string, string?> check, DateTimeOffset now)
    {
        var root = Path.Combine(Path.GetTempPath(), "speedtracker-flow-regression-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        void Expect(bool condition, string label, string? detail = null) => check(condition, label, detail);
        void Near(double? actual, double expected, string label) => Expect(actual is double value && Math.Abs(value - expected) < .0001, label, $"expected {expected}, got {actual}");
        try
        {
            Console.WriteLine("scenario: sparse pings and low-rate responses never become model calls");
            using (var fixture = new Fixture(Path.Combine(root, "sparse"), now))
            {
                fixture.Step(0);
                fixture.Step(.5, sent: 600);
                for (var tick = 2; tick <= 9; tick++)
                {
                    fixture.Step(tick * .5, received: tick is 2 or 5 or 8 ? 600UL : 0);
                    Expect(fixture.Tracker.Active.Count == 0, "sparse receive samples stay silent", $"tick={tick}");
                }
                fixture.Close(5);
                Expect(fixture.History.Read().Count == 0, "sparse pings do not pollute completed history");
                Expect(fixture.Tracker.HeldRate == null, "rejected pings do not replace held speed");
            }
            using (var fixture = new Fixture(Path.Combine(root, "slow"), now))
            {
                fixture.Step(0);
                fixture.Step(.1, sent: 600);
                fixture.Step(.5, received: 300);
                fixture.Step(.8, received: 10);
                fixture.Step(1.1, received: 10);
                Expect(fixture.Tracker.Active.Count == 0, "dense samples below 400 bytes/sec are not generation");
                fixture.Close(2);
                Expect(fixture.History.Read().Count == 0, "low-rate responses are not recorded");
            }
            using (var fixture = new Fixture(Path.Combine(root, "slow-finish"), now))
            {
                fixture.Stream();
                foreach (var time in new[] { 3.5, 5d, 6.5, 8d, 9.5, 11d }) fixture.Step(time, received: 10);
                fixture.Close(11.5);
                Expect(fixture.History.Read().Count == 0, "an initially dense response must still sustain 400 bytes/sec overall to enter history");
            }
            using (var fixture = new Fixture(Path.Combine(root, "waiting"), now))
            {
                fixture.Add(0, 0, "known");
                fixture.Add(0, 0, "unknown", host: "updates.example.test");
                fixture.Poll(0);
                fixture.Add(0, 320, "known");
                fixture.Add(0, 320, "unknown", host: "updates.example.test");
                fixture.Poll(.5);
                foreach (var time in new[] { 1d, 10d, 180d, 601d })
                {
                    fixture.Poll(time);
                    Expect(fixture.Tracker.Active.Count == 0, "tiny uploads never expose unproven network waiting", $"time={time}");
                }
                Expect(fixture.History.Read().Count == 0, "known and unknown housekeeping uploads create no records");
            }
            using (var fixture = new Fixture(Path.Combine(root, "receive-only"), now))
            {
                fixture.Step(0, received: 100000);
                fixture.Step(.5, received: 1200);
                fixture.Step(1, received: 1200);
                fixture.Step(1.5, received: 1200);
                fixture.Close(2);
                Expect(fixture.Tracker.Active.Count == 0 && fixture.History.Read().Count == 0, "an established receive-only stream needs a witnessed request upload");
            }

            Console.WriteLine("scenario: upload control bytes and the 300-ms grace cannot become TTFT");
            using (var fixture = new Fixture(Path.Combine(root, "grace"), now))
            {
                fixture.Step(0);
                fixture.Step(.5, received: 100, sent: 1200);
                fixture.Step(.8, received: 100, sent: 1200);
                fixture.Step(1, received: 1000);
                fixture.Step(1.09, received: 1000);
                fixture.Step(1.11, received: 100);
                fixture.Step(1.31, received: 100);
                fixture.Step(1.51, received: 600);
                fixture.Step(1.91, received: 600);
                Expect(fixture.Tracker.Active.Count == 0, "upload and early replies remain silent until stream density is proven");
                fixture.Step(2.31, received: 600);
                var live = fixture.Tracker.Active.SingleOrDefault();
                Expect(live is { Estimated: true }, "a dense post-upload response becomes an explicit estimate");
                Near(live?.TTFT, 1.01, "first response follows both upload grace and the 300-byte response threshold");
                fixture.Close(2.5);
                var record = fixture.History.Read().SingleOrDefault();
                Near(record?.Ttft, 1.01, "persisted network TTFT excludes upload-time control traffic");
                Near(record?.Generation, .8, "generation starts at the qualified first response, not TLS control bytes");
                Expect(record?.OutputTokens == 91, "estimated output excludes every upload/grace control byte", record?.OutputTokens.ToString());
            }
            using (var fixture = new Fixture(Path.Combine(root, "long-upload"), now))
            {
                fixture.Step(0);
                foreach (var time in new[] { .5, 1.5, 2.5, 3.5 }) fixture.Step(time, received: 100, sent: 1200);
                fixture.Step(4, received: 1200);
                fixture.Step(4.5, received: 1200);
                fixture.Step(5, received: 1200);
                fixture.Close(5.5);
                var record = fixture.History.Read().SingleOrDefault();
                Near(record?.Ttft, 3.5, "a multi-second continuous upload retains its original request start");
                Expect(record?.OutputTokens == 164, "a long upload's control traffic never enters generated-byte estimates");
            }

            Console.WriteLine("scenario: keep-alive requests finish old calls and start the current upload");
            foreach (var secondUpload in new[] { 2.1, 4.1 })
            {
                using var fixture = new Fixture(Path.Combine(root, "reuse-" + secondUpload.ToString(System.Globalization.CultureInfo.InvariantCulture)), now);
                fixture.Stream();
                fixture.Step(secondUpload, received: 1000, sent: 600);
                Expect(fixture.History.Read().Count == 1, "a reused TCP upload finishes the earlier response", $"upload={secondUpload}");
                Expect(fixture.Tracker.Active.Count == 0, "the next unproven request does not inherit the old stream's live state");
                fixture.Step(secondUpload + .4, received: 1200);
                fixture.Step(secondUpload + .9, received: 1200);
                fixture.Step(secondUpload + 1.4, received: 1200);
                var live = fixture.Tracker.Active.SingleOrDefault();
                Near(live?.TTFT, .4, "reused-connection TTFT belongs to the new upload");
                fixture.Close(secondUpload + 1.6);
                var records = fixture.History.Read().OrderBy(r => r.StartedAt).ToArray();
                Expect(records.Length == 2 && records[0].Id != records[1].Id, "two requests on one connection yield two distinct records");
                if (records.Length == 2)
                {
                    Expect(records[1].StartedAt == now.AddSeconds(secondUpload), "the upload arriving with old-call expiry is not lost");
                    Near(records[1].Generation, 1, "the second generation is not merged into the first");
                    Expect(records[0].OutputTokens == records[1].OutputTokens, "new-upload control bytes do not contaminate the second response");
                }
            }
            using (var fixture = new Fixture(Path.Combine(root, "connection-identity"), now))
            {
                fixture.Add(0, 0, "one"); fixture.Add(0, 0, "two"); fixture.Poll(0);
                fixture.Add(0, 600, "one"); fixture.Add(0, 600, "two"); fixture.Poll(.5);
                foreach (var time in new[] { 1d, 1.5, 2d })
                {
                    fixture.Add(1200, 0, "one"); fixture.Add(1200, 0, "two"); fixture.Poll(time);
                }
                Expect(fixture.Tracker.Active.Count == 2, "ConnectionID separates same-PID same-host TCP streams");
                fixture.Sampler.Remove("one"); fixture.Sampler.Remove("two"); fixture.Poll(2.5);
                Expect(fixture.History.Read().Count == 2, "separate connection identities each persist their own request");
            }

            Console.WriteLine("scenario: live speed holds through pauses and targets include observed harnesses");
            using (var fixture = new Fixture(Path.Combine(root, "hold-target"), now))
            {
                foreach (var harness in new[] { "Gemini CLI", "Qwen Code", "Aider", "Goose", "Crush" })
                    Expect(fixture.Tracker.Harnesses.Any(h => h.Name == harness && h.IsInstalled), "installed network-only harness is available in the Live target catalog", harness);
                Expect(!fixture.Tracker.Harnesses.Any(h => h.Name is "Pi" or "OMP"), "a harness that is not installed, logged or running is not in the Live target catalog");
                fixture.Tracker.Target = "Aider";
                using (var reloaded = new TrackerService(new HistoryStore(fixture.History.Home), sources: Array.Empty<LogSource>(), findInstalled: () => new HashSet<string>()))
                    Expect(reloaded.Target == "Aider", "Aider target persists even before the first completed call");
                fixture.Stream();
                var rate = fixture.Tracker.Active.SingleOrDefault()?.Rate;
                fixture.Poll(2.8);
                Near(fixture.Tracker.Active.SingleOrDefault()?.Rate, rate ?? -1, "a short receive pause holds the measured live rate");
                fixture.Poll(3.9);
                Near(fixture.Tracker.Active.SingleOrDefault()?.Rate, rate ?? -1, "held live rate does not decay with wall-clock silence");
                fixture.Poll(4);
                Expect(fixture.Tracker.Active.Count == 0 && fixture.Tracker.HeldRate is > 0, "ended streams leave their accepted speed held while idle");
                fixture.Step(4.5, connection: "observed", harness: "Observed Harness");
                Expect(fixture.Tracker.Harnesses.Any(h => h.Name == "Observed Harness"), "observed sampler harnesses are added to Live targets");
                fixture.Tracker.Target = "Observed Harness";
                using var observedReloaded = new TrackerService(new HistoryStore(fixture.History.Home), sources: Array.Empty<LogSource>(), findInstalled: () => new HashSet<string>());
                Expect(observedReloaded.Target == "Observed Harness" && observedReloaded.Harnesses.Any(h => h.Name == "Observed Harness"), "observed target validation survives restart without a completed record");
                try { observedReloaded.Target = "Not A Harness"; Expect(false, "unobserved unknown targets remain rejected"); }
                catch (ArgumentException) { Expect(true, "unobserved unknown targets remain rejected"); }
            }

            Console.WriteLine("scenario: consumed parser batches survive transient history failures");
            var logRoot = Path.Combine(root, "log-recovery", "logs");
            Directory.CreateDirectory(logRoot);
            using (var fixture = new Fixture(Path.Combine(root, "log-recovery"), now, new[] { new LogSource("OMP", logRoot) }))
            {
                var rows = Enumerable.Range(1, 2).Select(index =>
                {
                    var start = now.AddSeconds(-30 + index * 10);
                    return JsonSerializer.Serialize(new
                    {
                        type = "message", timestamp = Iso(start.AddSeconds(2)),
                        message = new { role = "assistant", model = "claude-opus-4", provider = "anthropic", timestamp = Iso(start), duration = 2000, ttft = 250, stopReason = "stop", responseId = "recover_" + index, usage = new { output = 400, input = 50 } }
                    });
                });
                File.WriteAllText(Path.Combine(logRoot, "session.jsonl"), string.Join('\n', rows) + "\n");
                var blocker = Path.Combine(fixture.History.Home, "history.jsonl");
                Directory.CreateDirectory(blocker);
                try { fixture.Poll(0); Expect(false, "history persistence failures are surfaced"); }
                catch (IOException) { Expect(true, "history persistence failures are surfaced"); }
                Directory.Delete(blocker);
                fixture.Poll(.5);
                var recovered = fixture.History.Read();
                Expect(recovered.Count == 2 && recovered.Any(r => r.SourceKey == "omp:recover_1") && recovered.Any(r => r.SourceKey == "omp:recover_2"), "the failed record and remaining consumed parser batch are retried");
                fixture.Poll(1);
                Expect(fixture.History.Read().Count == 2, "recovered batches are committed exactly once");
            }
            using (var fixture = new Fixture(Path.Combine(root, "network-recovery"), now))
            {
                fixture.Stream();
                var blocker = Path.Combine(fixture.History.Home, "history.jsonl");
                Directory.CreateDirectory(blocker);
                try { fixture.Close(2.5); Expect(false, "network history failures are surfaced"); }
                catch (IOException) { Expect(true, "network history failures are surfaced"); }
                Expect(fixture.Tracker.Active.Count == 0, "failed persistence does not leave an ended network call live");
                Directory.Delete(blocker);
                fixture.Poll(3);
                Expect(fixture.History.Read().Count == 1 && fixture.Tracker.HeldRate is > 0, "finished network calls survive persistence failure and restore held metrics");
                fixture.Poll(3.5);
                Expect(fixture.History.Read().Count == 1, "recovered network calls are not duplicated");
            }

            Console.WriteLine("scenario: explicit proxy live updates merge with passive collection and retry history");
            using (var fixture = new Fixture(Path.Combine(root, "proxy"), now))
            {
                fixture.Stream();
                var changed = 0;
                fixture.Tracker.Changed += () => changed++;
                var call = new LiveCall("proxy-regression", "Aider", "exact-model", "anthropic", "Waiting", now, now, null, null, null, false);
                fixture.Tracker.UpdateProxy(call);
                Expect(changed > 0 && fixture.Tracker.Active.Count == 2, "proxy updates publish immediately without replacing passive live calls");
                fixture.Poll(2.1);
                Expect(fixture.Tracker.Active.Any(c => c.Id == call.Id) && fixture.Tracker.Active.Any(c => c.Id.StartsWith("network:", StringComparison.Ordinal)), "proxy live state remains merged across passive polls");
                fixture.Tracker.UpdateProxy(call with { Phase = "Streaming", TTFT = .5, Rate = 50, OutputTokens = 100 });
                Near(fixture.Tracker.Active.Single(c => c.Id == call.Id).Rate, 50, "proxy updates expose exact current speed immediately");
                var record = new RequestRecord { StartedAt = now, Harness = "Aider", Model = "exact-model", UpstreamHost = "api.anthropic.com", Streamed = true, Ttft = .5, Generation = 2, Total = 2.5, OutputTokens = 100, Tps = 50, Source = "proxy", SourceKey = call.Id };
                var blocker = Path.Combine(fixture.History.Home, "history.jsonl");
                Directory.CreateDirectory(blocker);
                try { fixture.Tracker.FinishProxy(call.Id, record); Expect(false, "proxy history failures are surfaced"); }
                catch (IOException) { Expect(true, "proxy history failures are surfaced"); }
                Expect(!fixture.Tracker.Active.Any(c => c.Id == call.Id), "proxy finish publishes the removed call even if history is temporarily blocked");
                Directory.Delete(blocker);
                fixture.Poll(2.2);
                Expect(fixture.History.Read().Count(r => r.SourceKey == call.Id) == 1, "proxy completion uses the same recoverable persistence queue");
                Near(fixture.Tracker.HeldRate, 50, "accepted exact proxy metrics become held history");
                fixture.Tracker.FinishProxy("cancelled-proxy", null);
                Expect(fixture.History.Read().Count(r => r.SourceKey == call.Id) == 1, "proxy cancellation without an accepted record invents no completion");
            }
        }
        finally
        {
            try { Directory.Delete(root, true); } catch (IOException) { }
        }
    }

    private static string Iso(DateTimeOffset time) => time.UtcDateTime.ToString("yyyy-MM-ddTHH:mm:ss.fffZ");

    private sealed class Fixture : IDisposable
    {
        private readonly DateTimeOffset now;
        public HistoryStore History { get; }
        public TrackerService Tracker { get; }
        public FakeSampler Sampler { get; } = new();
        public Fixture(string root, DateTimeOffset now, IEnumerable<LogSource>? sources = null)
        {
            this.now = now;
            History = new(Path.Combine(root, "history"));
            // A fixed machine: these harnesses are installed and have no session logs, whatever the real one has.
            Tracker = new(History, Sampler, sources ?? Array.Empty<LogSource>(), Path.Combine(root, "user"), () => new HashSet<string> { "Gemini CLI", "Qwen Code", "Aider", "Goose", "Crush" });
            Tracker.SetEnhancedNetwork(true);
        }
        public void Add(ulong received, ulong sent, string connection = "conn", string harness = "Aider", string host = "api.anthropic.com")
        {
            var previous = Sampler.Get(connection);
            Sampler.Push(new(9001, harness, host, (previous?.Received ?? 0) + received, (previous?.Sent ?? 0) + sent, connection));
        }
        public void Step(double time, ulong received = 0, ulong sent = 0, string connection = "conn", string harness = "Aider", string host = "api.anthropic.com")
        {
            Add(received, sent, connection, harness, host);
            Poll(time);
        }
        public void Poll(double time) => Tracker.Poll(now.AddSeconds(time));
        public void Close(double time) { Sampler.Remove("conn"); Poll(time); }
        public void Stream()
        {
            Step(0); Step(.5, sent: 600); Step(1, received: 1200); Step(1.5, received: 1200); Step(2, received: 1200);
        }
        public void Dispose() => Tracker.Dispose();
    }

    private sealed class FakeSampler : IFlowSampler
    {
        private readonly Dictionary<string, FlowSample> samples = new(StringComparer.Ordinal);
        public bool Available => true;
        public string Status => "Deterministic flow regression sampler.";
        public FlowSample? Get(string connection) => samples.GetValueOrDefault(connection);
        public void Push(FlowSample sample) => samples[sample.ConnectionID!] = sample;
        public void Remove(string connection) => samples.Remove(connection);
        public IReadOnlyList<FlowSample> Sample() => samples.Values.ToArray();
        public void Dispose() { }
    }
}
