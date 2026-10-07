using System.Reflection;
using System.Text;
using System.Text.Json;
using Microsoft.Data.Sqlite;
using SpeedTracker.Core;
using ZstdSharp;

// Independent entry point: return the number of failed assertions for the main smoke runner.
public static class HarnessRegression
{
    public static int Run()
    {
        var suite = new Suite();
        suite.Scenario("DeepSeek released stream formats", suite.DeepSeekStreams);
        suite.Scenario("DeepSeek lifecycle and malformed telemetry", suite.DeepSeekLifecycle);
        suite.Scenario("DeepSeek inherited boundaries and generation selection", suite.DeepSeekSources);
        suite.Scenario("DeepSeek torn frames, corruption and shared budget", suite.DeepSeekTailSafety);
        suite.Scenario("OpenCode current telemetry and delayed usage", suite.OpenCodeCurrent);
        suite.Scenario("OpenCode legacy timings and counter validation", suite.OpenCodeLegacy);
        suite.Scenario("OpenCode bounded database refresh and schema discovery", suite.OpenCodePaging);
        suite.Scenario("OpenCode resumable JSON and independent part updates", suite.OpenCodeJson);
        suite.Scenario("Gemini CLI rewritten messages, thoughts and tool follow-ups", suite.GeminiCli);
        suite.Scenario("Antigravity call decoding and speed rules", suite.AntigravityCalls);
        suite.Scenario("Antigravity conversation files, in-flight calls and abandoned rows", suite.AntigravityFiles);
        Console.WriteLine($"Harness parity regression: {suite.Checks} checks, {suite.Failures} failures.");
        return suite.Failures;
    }

    private sealed class Suite
    {
        public int Checks { get; private set; }
        public int Failures { get; private set; }
        private readonly DateTimeOffset epoch = DateTimeOffset.FromUnixTimeMilliseconds(DateTimeOffset.UtcNow.AddSeconds(-60).ToUnixTimeMilliseconds());
        private DateTimeOffset Now => epoch.AddSeconds(60);
        private long At(int milliseconds = 0) => epoch.ToUnixTimeMilliseconds() + milliseconds;
        public void Scenario(string label, Action action)
        {
            try { action(); }
            catch (Exception ex)
            {
                if (ex is TargetInvocationException invocation && invocation.InnerException != null) ex = invocation.InnerException;
                Check(false, label + ": " + ex.GetType().Name + " " + ex.Message);
            }
        }
        private void Check(bool condition, string label)
        {
            Checks++;
            if (condition) return;
            Failures++;
            Console.WriteLine("  FAIL parity: " + label);
        }
        private void Near(double? value, double expected, string label) => Check(value != null && Math.Abs(value.Value - expected) < 0.0001, label);
        private static string Json(object value) => JsonSerializer.Serialize(value);
        private static string Lines(params object[] rows) => string.Join('\n', rows.Select(Json)) + "\n";
        private object Header(int version = 3, bool seeded = false, int? seedLength = null) => version >= 2
            ? new { type = "session", version, id = "session-fixture", createdAt = At(), isSeeded = seeded }
            : new Dictionary<string, object?> { ["type"] = "session", ["version"] = version, ["id"] = "session-fixture", ["createdAt"] = At() }.WithSeed(seedLength);
        private object Event(string type, int seq, int at, object data) => new { type, seq, time = At(at), data };
        private object Start(int seq = 0, int at = 0, int step = 1) => Event("step/start", seq, at, new { turn = 1, step });
        private object Chunk(string type, int at, object? fields = null)
        {
            var value = fields == null ? new Dictionary<string, object?>() : JsonSerializer.Deserialize<Dictionary<string, object?>>(Json(fields))!;
            value["type"] = type;
            return new { type = "chunk", time = At(at), chunk = value };
        }
        private object Message(int seq = 1, int at = 6000, string id = "response", int step = 1, object? usage = null, object[]? stream = null, string? surfaceOp = null) =>
            new Dictionary<string, object?>
            {
                ["type"] = "assistant/message", ["seq"] = seq, ["time"] = At(at),
                ["data"] = new { turn = 1, step, message = new { id, role = "assistant", source = new { kind = "model", provider = "opencode-go", model = "deepseek-v4.1-flash" } },
                    usage = usage ?? new { inputTokens = 100, outputTokens = 80, reasoningTokens = 20, cacheReadTokens = 40, cacheWriteTokens = 10 }, stream = stream ?? Array.Empty<object>() }
            }.WithSurface(surfaceOp);
        private static IReadOnlyList<LogRecord> Ingest(DeepSeekLogParser parser, object row) => parser.Ingest(Json(row));

        public void DeepSeekStreams()
        {
            foreach (var version in new[] { 2, 3, 4 })
            {
                var parser = new DeepSeekLogParser();
                Ingest(parser, Header(version)); Ingest(parser, Start());
                Check(parser.AwaitingResponse, $"v{version} request evidence is awaiting");
                var stream = new[]
                {
                    Chunk("block-start", 1999, new { blockType = "reasoning", index = 0 }),
                    new { type = "reasoning-chunks", time0 = At(2000), index = 0, dt = new[] { 10, 10 }, texts = new[] { "", "thought", "" } },
                    Chunk("block-start", 2999, new { blockType = "text", index = 1 }),
                    new { type = "text-chunks", time0 = At(3000), index = 1, dt = new[] { 20, -5 }, texts = new[] { " ", "", "answer" } },
                    Chunk("finish", 5998, new { reason = new { kind = "stop" } })
                };
                var record = Ingest(parser, Message(stream: stream)).Single();
                Near((record.FirstToken - epoch)?.TotalSeconds, 2.01, "actual delta replaces earlier block-start");
                Near((record.FirstVisible - epoch)?.TotalSeconds, 3.015, "signed packed gaps and whitespace preserve visible timing");
                Near((record.End - epoch).TotalSeconds, 5.998, "generation ends before persisted message and tools");
                Check(record.OutputTokens == 80 && record.InputTokens == 150 && record.CachedInputTokens == 40 && record.ReasoningTokens == 20, "DeepSeek exact disjoint usage is not double-counted");
                Check(!parser.AwaitingResponse, "assistant response clears DeepSeek activity before tools");
            }
            var whitespace = new DeepSeekLogParser(); Ingest(whitespace, Header()); Ingest(whitespace, Start());
            var result = Ingest(whitespace, Message(stream: new[] { Chunk("text-delta", 1000, new { text = " \n" }), Chunk("tool-call-delta", 1200, new { argumentsDelta = "{}" }) })).Single();
            Check(result.FirstToken == epoch.AddSeconds(1) && result.FirstVisible == null, "whitespace/tool arguments generate tokens but not visible answer text");
            foreach (var version in new[] { 0, 1 })
            {
                var parser = new DeepSeekLogParser(); Ingest(parser, Header(version)); Ingest(parser, Start());
                Ingest(parser, new { type = "reasoning-chunks", seq0 = 1, time0 = At(1000), data = new { turn = 1, step = 1, index = 0, dt = new[] { 20 }, texts = new[] { "", "thought" } } });
                Ingest(parser, Event("assistant/chunk", 3, 2000, new { turn = 1, step = 1, chunk = new { type = "text-delta", text = "answer" } }));
                Ingest(parser, Event("assistant/chunk", 4, 3000, new { turn = 1, step = 1, chunk = new { type = "usage", usage = new { outputTokens = 30, inputTokens = 10 } } }));
                Ingest(parser, Event("assistant/chunk", 5, 3100, new { turn = 1, step = 1, chunk = new { type = "finish", reason = new { kind = "stop" } } }));
                var message = version == 0 ? Event("assistant/message", 6, 3200, new { turn = 1, step = 1, provenance = new { model = "legacy-model", provider = "legacy-provider" }, content = Array.Empty<object>() }) :
                    Event("assistant/message", 6, 3200, new { turn = 1, step = 1, message = new { role = "assistant", id = "legacy", source = new { kind = "model", model = "legacy-model", provider = "legacy-provider" } } });
                var record = Ingest(parser, message).Single();
                Near((record.FirstToken - epoch)?.TotalSeconds, 1.02, "legacy seq0 packed rows preserve their actual first delta");
                Near((record.FirstVisible - epoch)?.TotalSeconds, 2, "legacy assistant/chunk uses outer timestamp");
                Near((record.End - epoch).TotalSeconds, 3.1, "legacy finish chunk excludes persistence delay");
                Check(record.OutputTokens == 30, "legacy stream usage remains available without message usage");
            }
        }
        public void DeepSeekLifecycle()
        {
            foreach (var type in new[] { "assistant/attempt", "llm/retry", "step/end", "turn/end", "session/end-seed" })
            {
                var parser = new DeepSeekLogParser(); Ingest(parser, Header()); Ingest(parser, Start());
                Ingest(parser, Event(type, 1, 1000, new { turn = 1, step = 1 }));
                Check(!parser.AwaitingResponse, type + " clears pending activity");
                var record = Ingest(parser, Message(seq: 2, stream: new[] { Chunk("reasoning-delta", 2000, new { text = "thought" }) })).Single();
                Check(record.RequestStart == null && record.MakeRecord().Ttft == null, type + " does not reuse obsolete dispatch timing");
                Near(record.MakeRecord().Generation, 4, "generation remains measurable without request timing");
                Near(record.MakeRecord().Tps, 20, "generation-only rate is retained");
            }
            var malformed = new DeepSeekLogParser(); malformed.Ingest("{\"type\":\"session\"}"); Ingest(malformed, Start());
            Check(!malformed.Supported && !malformed.AwaitingResponse && malformed.Limitation != null, "malformed header cannot fabricate a v0 call");
            var mismatch = new DeepSeekLogParser(expectedVersion: 3); Ingest(mismatch, Header(2));
            Check(!mismatch.HasSupportedHeader && mismatch.Limitation != null, "filename and header generations must agree");
            var noStep = new DeepSeekLogParser(); Ingest(noStep, Header()); Ingest(noStep, Event("step/start", 0, 0, new { turn = 1 }));
            Check(!noStep.AwaitingResponse, "missing step identity is not a request");
            foreach (var invalid in new object[] { -1, true, 1.5 })
            {
                var parser = new DeepSeekLogParser(); Ingest(parser, Header()); Ingest(parser, Start());
                Check(Ingest(parser, Message(usage: new { outputTokens = invalid, inputTokens = 10 })).Count == 0, "invalid required DeepSeek output counter is rejected");
                Check(!parser.AwaitingResponse, "invalid final usage still closes response activity");
            }
            var zero = new DeepSeekLogParser(); Ingest(zero, Header()); Ingest(zero, Start());
            Check(Ingest(zero, Message(usage: new { inputTokens = 5, outputTokens = 0, totalTokens = 999 })).Single().OutputTokens == 0, "zero exact output is not inferred from totalTokens");
            var surface = new DeepSeekLogParser(); Ingest(surface, Header()); Ingest(surface, Start());
            Check(Ingest(surface, Message(surfaceOp: "replace")).Count == 0 && !surface.AwaitingResponse, "surface rewrites are not provider calls");
            var failed = new DeepSeekLogParser(); Ingest(failed, Header()); Ingest(failed, Start());
            Check(Ingest(failed, Message(stream: new[] { Chunk("finish", 5000, new { reason = new { kind = "error" } }) })).Single().Aborted, "failed stream finish preserves interruption");
            var future = new DeepSeekLogParser(); Ingest(future, Header()); Ingest(future, Start());
            var recordFuture = Ingest(future, Message(stream: new[] { Chunk("text-delta", 7000, new { text = "late" }), Chunk("finish", 8000, new { reason = new { kind = "stop" } }) })).Single();
            Check(recordFuture.End == epoch.AddSeconds(6) && recordFuture.FirstToken == null, "future stream stamps do not create negative timing windows");
        }
        public void DeepSeekSources()
        {
            using var fixture = new DirectoryFixture();
            var missing = fixture.Put("missing/session.v3.jsonl", Lines(Header(seeded: true), Start(), Message(id: "ancestor")));
            using var watcher = new SessionWatcher(fixture.Root);
            Check(watcher.Poll(Now).Count == 0 && !watcher.HasLogs && watcher.Active(Now).Count == 0, "unresolved inherited cut is unavailable, not live");
            Check(watcher.Errors.Values.Any(v => v.Contains("inherited boundary", StringComparison.Ordinal)), "missing inherited boundary is surfaced");
            File.AppendAllText(missing, Lines(Event("session/end-seed", 2, 6001, new { inherited = true }), Start(3, 10000, 2), Message(4, 16000, "ancestor-two", 2),
                Event("session/end-seed", 5, 16001, new { inherited = true }), Start(6, 20000, 3), Message(7, 26000, "owned", 3)));
            Check(watcher.Poll(Now.AddSeconds(1)).Count == 0, "seed metadata pass admits no ancestor calls");
            Check(watcher.Poll(Now.AddSeconds(2)).Select(r => r.Key).SequenceEqual(new[] { "deepseek:owned" }), "last inherited cut excludes nested ancestors");
            foreach (var version in new[] { 0, 1 })
            {
                using var historical = new DirectoryFixture();
                historical.Put($"session.v{version}.jsonl", Lines(Header(version, seedLength: 2), Start(), Message(id: "inherited"), Start(2, 10000, 2), Message(3, 16000, "child", 2)));
                // V0's canonical generation name has no .v0 suffix.
                if (version == 0) File.Move(Path.Combine(historical.Root, "session.v0.jsonl"), Path.Combine(historical.Root, "session.jsonl"));
                using var reader = new SessionWatcher(historical.Root);
                Check(reader.Poll(Now).Select(r => r.Key).SequenceEqual(new[] { "deepseek:child" }), "historical seedLength suppresses inherited calls");
            }
            using var generations = new DirectoryFixture();
            generations.Put("session.v1.jsonl", Lines(Header(1), Start(), Message(id: "old")));
            generations.Put("session.v3.jsonl", Lines(Header(), Start(), Message(id: "current")));
            using var selected = new SessionWatcher(generations.Root);
            Check(selected.Poll(Now).Select(r => r.Key).SequenceEqual(new[] { "deepseek:current" }), "highest numeric generation wins");
            generations.Put("session.v5.jsonl", Lines(Header(5), Start(), Message(id: "unsupported")));
            Check(selected.Poll(Now.AddSeconds(5)).Count == 0 && !selected.HasLogs && selected.Active(Now.AddSeconds(5)).Count == 0, "future generation never falls back to older files");
            using var ambiguous = new DirectoryFixture();
            ambiguous.Put("session.v3.jsonl", Lines(Header(), Start()));
            File.WriteAllBytes(Path.Combine(ambiguous.Root, "session.v3.jsonl.zstd"), Frame(Lines(Header(), Start())));
            using var mixed = new SessionWatcher(ambiguous.Root);
            Check(mixed.Poll(Now).Count == 0 && !mixed.HasLogs && mixed.Errors.Values.Any(v => v.Contains("Ambiguous", StringComparison.Ordinal)), "ambiguous raw/compressed generation is refused");
        }
        public void DeepSeekTailSafety()
        {
            using var fixture = new DirectoryFixture();
            var path = fixture.Put("session.v3.jsonl", Lines(Header(), Start()));
            using var watcher = new SessionWatcher(fixture.Root);
            watcher.Poll(Now);
            Check(watcher.HasLogs && watcher.Active(Now).Count == 1 && watcher.Active(Now.AddMinutes(11)).Count == 0, "DeepSeek live request expires without new response");
            var response = Lines(Message());
            File.AppendAllText(path, response[..(response.Length / 2)]);
            Check(watcher.Poll(Now.AddSeconds(1)).Count == 0, "torn raw row is not interpreted prematurely");
            File.AppendAllText(path, response[(response.Length / 2)..]);
            Check(watcher.Poll(Now.AddSeconds(2)).Count == 1 && watcher.Active(Now.AddSeconds(2)).Count == 0, "completed raw row clears activity and emits once");
            File.WriteAllText(path, Lines(Header(), Start(), Message()));
            Check(watcher.Poll(Now.AddSeconds(3)).Count == 0, "replacement replay keeps stable response-key dedup");
            using var compressed = new DirectoryFixture();
            var compressedPath = Path.Combine(compressed.Root, "session.v3.jsonl.zstd");
            File.WriteAllBytes(compressedPath, Frame(Lines(Header(), Start())));
            using var reader = new SessionWatcher(compressed.Root);
            reader.Poll(Now);
            var frame = Frame(Lines(Message()));
            var records = new List<LogRecord>();
            for (var offset = 0; offset < frame.Length; offset += 7)
            {
                using var append = new FileStream(compressedPath, FileMode.Append, FileAccess.Write, FileShare.ReadWrite);
                append.Write(frame, offset, Math.Min(7, frame.Length - offset)); append.Flush();
                records.AddRange(reader.Poll(Now.AddSeconds(1)));
            }
            Check(records.Count == 1 && reader.Active(Now.AddSeconds(1)).Count == 0 && reader.Errors.Count == 0, "concatenated zstd frame survives arbitrary append boundaries");
            using var corrupt = new DirectoryFixture();
            var corruptPath = Path.Combine(corrupt.Root, "session.v3.jsonl.zstd");
            File.WriteAllBytes(corruptPath, Frame(Lines(Header(), Start())));
            using var broken = new SessionWatcher(corrupt.Root); broken.Poll(Now);
            var invalid = Frame(Lines(Message())); invalid[0] ^= 0xff;
            using (var append = new FileStream(corruptPath, FileMode.Append, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete)) append.Write(invalid);
            broken.Poll(Now.AddSeconds(1));
            Check(!broken.HasLogs && broken.Active(Now.AddSeconds(1)).Count == 0 && broken.Errors.Count > 0, "a corrupt later frame clears availability and active state");
            File.WriteAllBytes(corruptPath, Frame(Lines(Header(), Start(), Message(id: "repaired"))));
            Check(broken.Poll(Now.AddSeconds(2)).Any(r => r.Key == "deepseek:repaired") && broken.HasLogs && broken.Errors.Count == 0, "repair resets failed decoder without suppressing valid recovered usage");
            using var bounded = new DirectoryFixture();
            var oversized = new string('x', 4 * 1024 * 1024 + 100);
            for (var index = 0; index < 5; index++) bounded.Put($"{index}/session.v3.jsonl", Lines(Header(), Start(), Event("tool/result", 1, 1000, new { turn = 1, step = 1, output = oversized }), Message(2, id: "budget-" + index)));
            using var budgeted = new SessionWatcher(bounded.Root);
            var first = budgeted.Poll(Now); var keys = first.Select(r => r.Key).ToHashSet();
            Check(first.Count < 5, "decoded-byte budget is shared across tails");
            for (var index = 0; index < 8; index++) keys.UnionWith(budgeted.Poll(Now.AddSeconds(index + 1)).Select(r => r.Key));
            Check(keys.Count == 5, "bounded tail polling resumes all remaining sessions");
        }

        private Dictionary<string, object?> Assistant(bool current = true, bool terminal = true, int created = 0, object[]? content = null, bool usage = true)
        {
            var data = new Dictionary<string, object?> { ["time"] = terminal ? new { created = At(created), completed = At(created + 20000) } : new { created = At(created) } };
            if (terminal) data["finish"] = "stop";
            if (usage) data["tokens"] = new { input = 100, output = 80, reasoning = 20, cache = new { read = 40, write = 10 } };
            if (current) { data["model"] = new { id = "fixture-model", providerID = "fixture-provider" }; data["content"] = content ?? Array.Empty<object>(); }
            else { data["role"] = "assistant"; data["modelID"] = "fixture-model"; data["providerID"] = "fixture-provider"; }
            return data;
        }
        public void OpenCodeCurrent()
        {
            using var fixture = new SqliteFixture(); fixture.Schema(current: true, legacy: false);
            fixture.Message("text-first", "text", At(), Assistant(content: new object[] { new { type = "text", text = "unretained content" }, new { type = "reasoning", time = new { created = At(2000) } } }));
            fixture.Message("reasoning-first", "reasoning", At(), Assistant(content: new object[] { new { type = "reasoning", time = new { created = At(2000) } }, new { type = "text" } }));
            fixture.Message("tool-paused", "tool", At(), Assistant(terminal: false, usage: false, content: new[] { new { type = "tool", time = new { created = At(2000) }, state = new { status = "running", input = "unretained arguments" } } }));
            fixture.Message("delayed", "delay", At(), Assistant(usage: false));
            fixture.Message("superseded", "same-session", At(), Assistant(terminal: false, usage: false));
            fixture.Message("latest", "same-session", At(21000), Assistant(created: 21000));
            var summary = Assistant(terminal: false, usage: false); summary["summary"] = true;
            fixture.Message("summary", "summary", At(), summary);
            using var watcher = new OpenCodeLogWatcher(new[] { fixture.Database });
            var records = watcher.Poll(Now);
            var text = records.Single(r => r.Key == "opencode:text-first");
            Check(text.FirstToken == null && text.FirstVisible == null && text.MakeRecord().Tps == null, "v2 text-first never borrows a later reasoning timestamp");
            var reasoning = records.Single(r => r.Key == "opencode:reasoning-first").MakeRecord();
            Check(reasoning.Ttft == null && reasoning.FirstVisible == null, "v2 request-relative timing remains unknown");
            Near(reasoning.Generation, 18, "v2 independently measured generation survives absent request timing");
            Near(reasoning.Tps, 100.0 / 18, "v2 exact generation-only TPS reaches the record");
            Check(watcher.HasLogs && watcher.Active(Now).Count == 0, "tool pause, summary, finished missing usage and superseded rows are not active");
            var final = Assistant(); final["finish"] = "error"; final["error"] = new { type = "unknown", message = "unretained diagnostic" };
            fixture.Update("delayed", final);
            var delayed = watcher.Poll(Now.AddSeconds(1));
            Check(delayed.Count == 1 && delayed[0].Key == "opencode:delayed" && delayed[0].Aborted, "terminal missing usage remains refreshable until the actual usage arrives");
            Check(watcher.Poll(Now.AddSeconds(2)).Count == 0, "unchanged database does not rescan emitted calls");
            fixture.Message("pending", "new-session", At(25000), Assistant(terminal: false, created: 25000, usage: false));
            watcher.Poll(Now.AddSeconds(3));
            Check(watcher.Active(Now.AddSeconds(3)).Count == 1 && watcher.Active(Now.AddMinutes(11)).Count == 0, "current incomplete telemetry has source-based activity and expiry");
            fixture.Update("pending", Assistant(created: 25000));
            Check(watcher.Poll(Now.AddSeconds(4)).Count == 1 && watcher.Active(Now.AddSeconds(4)).Count == 0, "WAL in-place completion refreshes and clears pending");
        }
        public void OpenCodeLegacy()
        {
            using var fixture = new SqliteFixture(); fixture.Schema(current: false, legacy: true);
            fixture.Message("legacy", "legacy-session", At(), Assistant(current: false), current: false);
            fixture.Part("reason", "legacy", "reasoning", At(10000), new { start = At(1000), end = At(8000) });
            fixture.Part("synthetic", "legacy", "text", At(1000), new { start = At(10) }, synthetic: true);
            fixture.Part("ignored", "legacy", "text", At(1000), new { start = At(20) }, ignored: true);
            fixture.Part("text", "legacy", "text", At(12000), new { start = At(2000) });
            fixture.Part("finish", "legacy", "step-finish", At(11000), new { end = At(19000) });
            var invalid = Assistant(current: false); invalid["tokens"] = new { output = -1, reasoning = 2, input = 1 };
            fixture.Message("negative", "negative-session", At(), invalid, current: false);
            var missing = Assistant(current: false); missing["tokens"] = new { reasoning = 2, input = 1 };
            fixture.Message("missing", "missing-session", At(), missing, current: false);
            var overflow = Assistant(current: false); overflow["tokens"] = new { output = int.MaxValue, reasoning = 1 };
            fixture.Message("overflow", "overflow-session", At(), overflow, current: false);
            fixture.Message("paused", "paused-session", At(), Assistant(current: false, terminal: false), current: false);
            fixture.Part("paused-tool", "paused", "tool", At(1000), null, status: "completed");
            using var watcher = new OpenCodeLogWatcher(new[] { fixture.Root });
            var record = watcher.Poll(Now).Single();
            Check(record.Key == "opencode:legacy" && record.OutputTokens == 100 && record.InputTokens == 150, "negative/missing/overflowing output cannot become exact OpenCode calls");
            Near(record.MakeRecord().Ttft, 1, "synthetic and ignored part timings are excluded");
            Near(record.MakeRecord().FirstVisible, 2, "legacy visible timestamp is from actual text");
            Near(record.MakeRecord().Generation, 10, "legacy generation ends at step-finish persistence, not time.end/tool cleanup");
            Check(watcher.Active(Now).Count == 0, "legacy tool statuses suppress model activity");
        }
        public void OpenCodePaging()
        {
            using var fixture = new SqliteFixture(); fixture.Schema(current: true, legacy: false);
            for (var index = 0; index < 300; index++) fixture.Message("pending-" + index.ToString("D3"), "session-" + index, At(index), Assistant(terminal: false, created: index, usage: false));
            using var watcher = new OpenCodeLogWatcher(new[] { fixture.Root });
            for (var index = 0; index < 5; index++) Check(watcher.Poll(Now).Count == 0, "bounded history pages do not fabricate incomplete calls");
            fixture.UpdateAll(Assistant());
            var keys = new HashSet<string>();
            for (var index = 0; index < 5; index++) keys.UnionWith(watcher.Poll(Now.AddSeconds(index + 1)).Select(r => r.Key));
            Check(keys.Count == 300 && watcher.Active(Now.AddSeconds(5)).Count == 0, "refresh queue continues past 128 pending IDs even after data_version stops changing");
            Check(watcher.Poll(Now.AddSeconds(6)).Count == 0, "completed bounded backfill does not repeat unchanged DB work");
            using var unindexed = new SqliteFixture(); unindexed.Schema(current: true, legacy: false, indexed: false);
            for (var index = 0; index < 300; index++) unindexed.Message("fallback-" + index, "fallback-session-" + index, At(index), Assistant(created: index));
            using var fallback = new OpenCodeLogWatcher(new[] { unindexed.Root });
            var fallbackKeys = new HashSet<string>();
            for (var index = 0; index < 5; index++) fallbackKeys.UnionWith(fallback.Poll(Now).Select(r => r.Key));
            Check(fallbackKeys.Count == 300, "unindexed schema walks bounded rowid pages instead of abandoning history or sorting the DB");
            using var schema = new SqliteFixture(); using var discover = new OpenCodeLogWatcher(new[] { schema.Root });
            Check(discover.Poll(Now).Count == 0 && !discover.HasLogs, "empty supported root is not readable telemetry");
            schema.Schema(current: true, legacy: false); schema.Message("new-schema", "schema", At(), Assistant());
            Check(discover.Poll(Now.AddSeconds(1)).Count == 1 && discover.HasLogs, "schema added after opening is discovered without restart");
            using var inserted = new SqliteFixture(); inserted.Schema(current: true, legacy: false);
            using var insertionReader = new OpenCodeLogWatcher(new[] { inserted.Root }); insertionReader.Poll(Now);
            for (var index = 0; index < 300; index++) inserted.Message("inserted-" + index, "inserted-session-" + index, At(index), Assistant(created: index));
            var insertionFirst = insertionReader.Poll(Now.AddSeconds(1));
            var insertedKeys = insertionFirst.Select(r => r.Key).ToHashSet();
            Check(insertionFirst.Count <= 128, "new insertion catch-up obeys the per-table chunk ceiling");
            for (var index = 0; index < 5; index++) insertedKeys.UnionWith(insertionReader.Poll(Now.AddSeconds(index + 2)).Select(r => r.Key));
            Check(insertedKeys.Count == 300, "insertion cursor resumes after the database becomes unchanged");
        }
        public void OpenCodeJson()
        {
            using var fixture = new DirectoryFixture();
            for (var index = 0; index < 270; index++)
            {
                var data = Assistant(current: false, terminal: false, created: index, usage: false);
                data["id"] = "pending-" + index; data["sessionID"] = "session-" + index;
                fixture.Put($"storage/message/session-{index}/pending-{index}.json", Json(data));
            }
            var completed = Assistant(current: false); completed["id"] = "json-complete"; completed["sessionID"] = "json-complete-session";
            fixture.Put("storage/message/completed/json-complete.json", Json(completed));
            using var watcher = new OpenCodeLogWatcher(new[] { fixture.Root });
            var keys = new HashSet<string>();
            for (var index = 0; index < 4; index++) keys.UnionWith(watcher.Poll(Now.AddSeconds(index)).Select(r => r.Key));
            Check(keys.Contains("opencode:json-complete"), "unresolved JSON files cannot starve later completed files");
            var pending = Assistant(current: false, terminal: false, usage: false); pending["id"] = "json-parts"; pending["sessionID"] = "json-parts-session";
            var file = fixture.Put("storage/message/parts/json-parts.json", Json(pending));
            watcher.Poll(Now.AddSeconds(5)); watcher.Poll(Now.AddSeconds(6));
            pending["finish"] = "tool-calls"; pending["tokens"] = new { input = 10, output = 5, reasoning = 1 };
            pending["time"] = new { created = At(), completed = At(20000) };
            File.WriteAllText(file, Json(pending));
            var first = watcher.Poll(Now.AddSeconds(7));
            Check(first.Any(r => r.Key == "opencode:json-parts") && !watcher.Active(Now.AddSeconds(7)).Any(c => c.Id.EndsWith("json-parts", StringComparison.Ordinal)), "legacy JSON in-place finalization remains refreshable");
            var paused = Assistant(current: false, terminal: false, created: 30000); paused["id"] = "json-tool"; paused["sessionID"] = "json-tool-session";
            fixture.Put("storage/message/tools/json-tool.json", Json(paused));
            watcher.Poll(Now.AddSeconds(9)); watcher.Poll(Now.AddSeconds(10));
            fixture.Put("storage/part/json-tool/tool.json", Json(new { type = "tool", state = new { status = "running", input = "unretained" } }));
            watcher.Poll(Now.AddSeconds(11));
            Check(!watcher.Active(Now.AddSeconds(11)).Any(c => c.Id.EndsWith("json-tool", StringComparison.Ordinal)), "independent tool-part updates clear JSON activity without message mtime growth");
            Check(watcher.Poll(Now.AddSeconds(12)).Count == 0, "JSON completed response keys are not emitted twice");
        }

        private string Iso(int milliseconds) => epoch.AddMilliseconds(milliseconds).UtcDateTime.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", System.Globalization.CultureInfo.InvariantCulture);

        // Lines shaped as Gemini CLI 0.46's ChatRecordingService writes them. Mirrors GeminiCLILogParserTests.
        public void GeminiCli()
        {
            var parser = new GeminiCliLogParser();
            Check(parser.Ingest(Json(new { sessionId = "s1", projectHash = "abc", startTime = Iso(0), lastUpdated = Iso(0), kind = "main" })).Count == 0 && parser.Supported, "Gemini session metadata recognised without a record");
            parser.Ingest(Json(new { id = "u1", timestamp = Iso(0), type = "user", content = new[] { new { text = "private prompt" } } }));
            Check(parser.AwaitingResponse && parser.LastRequestAt == epoch, "Gemini user message opens a request");
            var reply = new Dictionary<string, object?>
            {
                ["id"] = "g1", ["timestamp"] = Iso(6000), ["type"] = "gemini", ["content"] = "",
                ["thoughts"] = new[] { new { subject = "s", description = "d", timestamp = Iso(1500) }, new { subject = "s", description = "d", timestamp = Iso(3000) } },
                ["tokens"] = new { input = 1000, output = 200, cached = 400, thoughts = 100, tool = 0, total = 1300 }, ["model"] = "gemini-3-pro"
            };
            var first = parser.Ingest(Json(reply));
            Check(first.Count == 1 && first[0].Key == "gemini:g1" && !parser.AwaitingResponse, "Gemini reply recorded once its stream has ended");
            var stored = first[0].MakeRecord();
            Check(stored.Harness == "Gemini CLI" && stored.Model == "gemini-3-pro" && stored.OutputTokens == 300 && stored.ReasoningTokens == 100, "Gemini output counts thinking with the reply");
            Check(stored.InputTokens == 1000 && stored.CachedInputTokens == 400, "Gemini input and cached tokens carried over");
            Near(stored.Ttft, 1.5, "Gemini first thought is the first token");
            Near(stored.Tps, 300 / 4.5, "Gemini speed spans first thought to stream end");
            reply["toolCalls"] = new[] { new { id = "t1", name = "read_file", status = "success", timestamp = Iso(9000) } };
            Check(parser.Ingest(Json(reply)).Count == 0 && parser.AwaitingResponse && parser.LastRequestAt == epoch.AddMilliseconds(9000), "Gemini rewritten message adds no record and dates the tool follow-up request");
            var second = parser.Ingest(Json(new { id = "g2", timestamp = Iso(12000), type = "gemini", content = "done", thoughts = Array.Empty<object>(), tokens = new { input = 1200, output = 60, cached = 0, thoughts = 0, tool = 0, total = 1260 }, model = "gemini-3-pro" }));
            Check(second.Count == 1 && second[0].RequestStart == epoch.AddMilliseconds(9000) && second[0].MakeRecord().Ttft == null && second[0].MakeRecord().ReasoningTokens == null, "Gemini reply without thoughts has no first-token time");
            Near(second[0].MakeRecord().Tps, 20, "Gemini reply without thoughts falls back to the whole request");
            var third = parser.Ingest(Json(new { id = "g3", timestamp = Iso(30000), type = "gemini", content = "b", tokens = new { output = 40, thoughts = 0 }, model = "gemini-3-pro" }));
            Check(third.Count == 1 && third[0].RequestStart == null && third[0].MakeRecord().Ttft == null && third[0].MakeRecord().Tps == null, "Gemini reply with no new request never borrows the old start");

            var late = new GeminiCliLogParser();
            late.Ingest(Json(new { id = "u1", timestamp = Iso(0), type = "user", content = "x" }));
            var pending = new Dictionary<string, object?> { ["id"] = "g1", ["timestamp"] = Iso(5000), ["type"] = "gemini", ["content"] = "a", ["tokens"] = null, ["model"] = "m" };
            Check(late.Ingest(Json(pending)).Count == 0, "Gemini message without tokens waits");
            pending["tokens"] = new { output = 100, thoughts = 0 };
            var arrived = late.Ingest(Json(pending));
            Check(arrived.Count == 1 && late.Ingest(Json(pending)).Count == 0, "Gemini tokens on a later write produce exactly one record");
            Near(arrived[0].MakeRecord().Tps, 20, "Gemini late tokens keep the message's own end time");

            var nested = new GeminiCliLogParser();
            nested.Ingest("{\"$set\":{\"messages\":[{\"id\":\"u1\",\"timestamp\":\"" + Iso(0) + "\",\"type\":\"user\",\"content\":[{\"text\":\"x\"}]}],\"lastUpdated\":\"" + Iso(0) + "\"}}");
            Check(nested.AwaitingResponse && nested.LastRequestAt == epoch, "Gemini messages inside a metadata update count");
            Check(nested.Ingest(Json(new { id = "i1", timestamp = Iso(1000), type = "info", content = "note" })).Count == 0 && nested.AwaitingResponse, "Gemini status lines are not model calls");
            nested.Ingest(Json(new { id = "e1", timestamp = Iso(2000), type = "error", content = "quota" }));
            Check(!nested.AwaitingResponse, "Gemini error ends the wait");
        }

        // Protobuf laid out as Antigravity's rows are. The prompt fields are present, as in real rows.
        private static byte[] Varint(ulong value)
        {
            var bytes = new List<byte>();
            do { var b = (byte)(value & 0x7F); value >>= 7; bytes.Add(value == 0 ? b : (byte)(b | 0x80)); } while (value != 0);
            return bytes.ToArray();
        }
        private static byte[] Number(int field, ulong value) => Varint((ulong)(field << 3)).Concat(Varint(value)).ToArray();
        private static byte[] Bytes(int field, byte[] value) => Varint((ulong)(field << 3 | 2)).Concat(Varint((ulong)value.Length)).Concat(value).ToArray();
        private static byte[] Text(int field, string value) => Bytes(field, Encoding.UTF8.GetBytes(value));
        private static byte[] Seconds(double value)
        {
            var whole = Math.Floor(value);
            return Number(1, (ulong)whole).Concat(Number(2, (ulong)Math.Round((value - whole) * 1_000_000_000))).ToArray();
        }
        private static byte[] Generation(ulong[] steps, ulong? output, ulong thinking = 0, double? ttft = null, double? streaming = null, string model = "gemini-3.8-flash-n")
        {
            var chat = Text(1, "SYSTEM PROMPT: a secret that must never be read").Concat(Bytes(2, Text(1, "user said something private"))).Concat(Number(3, 1318));
            if (output is ulong tokens) chat = chat.Concat(Bytes(4, Number(1, 1318).Concat(Number(2, 4000)).Concat(Number(3, tokens)).Concat(Number(5, 20_000)).Concat(Number(6, 24)).Concat(Number(9, thinking)).Concat(Number(10, tokens - thinking)).ToArray()));
            if (ttft is double first) chat = chat.Concat(Bytes(11, Seconds(first)));
            if (streaming is double length) chat = chat.Concat(Bytes(12, Seconds(length)));
            chat = chat.Concat(Text(19, model));
            return Bytes(1, chat.ToArray()).Concat(Bytes(2, steps.SelectMany(Varint).ToArray())).Concat(Text(4, "exec-1")).ToArray();
        }

        public void AntigravityCalls()
        {
            var burst = AntigravityGeneration.Parse(Generation(new ulong[] { 1, 2 }, 212, 148, 5.256, 0.302))!;
            Check(burst.StepIndices.SequenceEqual(new long[] { 1, 2 }) && burst.ExecutionId == "exec-1" && burst.Model == "gemini-3.8-flash-n", "Antigravity ids, steps and model decoded");
            Check(burst.OutputTokens == 212 && burst.ThinkingTokens == 148 && burst.InputTokens == 4000 && burst.CacheReadTokens == 20_000 && burst.IsComplete, "Antigravity usage decoded");
            Near(burst.TimeToFirstToken, 5.256, "Antigravity time to first token decoded");
            Near(burst.StreamingDuration, 0.302, "Antigravity streaming duration decoded");
            Check(Protobuf.Identifier(Encoding.UTF8.GetBytes("user said something private")) == null && Protobuf.Identifier(Encoding.UTF8.GetBytes("gemini-3.8-flash-n")) == "gemini-3.8-flash-n", "Antigravity free text is never accepted as an identifier");
            var stored = burst.Record("c", 0, epoch)!.MakeRecord();
            Near(stored.Ttft, 5.256, "Antigravity TTFT is the harness's own");
            Near(stored.Generation, 0.302, "Antigravity generation is the streaming time");
            Near(stored.Total, 5.558, "Antigravity total spans request to end");
            Near(stored.Tps, 212 / 5.558, "Antigravity burst is timed over the whole call, not the burst");
            Check(stored.OutputTokens == 212 && stored.ReasoningTokens == 148 && stored.InputTokens == 24_000 && stored.CachedInputTokens == 20_000 && stored.Harness == "Antigravity" && stored.Source == "log", "Antigravity record fields");
            var longer = AntigravityGeneration.Parse(Generation(new ulong[] { 39 }, 3900, 2556, 17.926, 31.942))!.Record("c", 19, epoch)!;
            Check(longer.Key == "antigravity:c:exec-1:19", "Antigravity key is stable per conversation, turn and call");
            Near(longer.MakeRecord().Tps, 1344 / 31.942, "Antigravity long stream is timed on its visible tokens");
            Check(AntigravityGeneration.Parse(new byte[] { 0xFF, 0xFF, 0xFF }) == null && Protobuf.Fields(new byte[] { 0x0A, 0x05, 0x01 }) == null, "Antigravity malformed bytes rejected");
        }

        public void AntigravityFiles()
        {
            using var fixture = new AntigravityFixture(epoch);
            fixture.Step(1, 0.019, 5.577); fixture.Generation(0, Generation(new ulong[] { 1, 2 }, 212, 148, 5.256, 0.302));
            fixture.Step(3, 5.593, 10.633); fixture.Generation(1, Generation(new ulong[] { 3, 4 }, 154, 76, 4.978, 0.062));
            using var watcher = new AntigravityLogWatcher(new[] { fixture.Root });
            var records = watcher.Poll(Now);
            Check(watcher.HasLogs && records.Count == 2 && records[0].RequestStart == epoch.AddMilliseconds(19), "Antigravity calls read from a conversation file");
            Check(watcher.LatestModel == "gemini-3.8-flash-n" && watcher.Active(Now).Count == 0 && watcher.LastRequestAt == epoch.AddMilliseconds(5593), "Antigravity finished calls are not active");
            Check(watcher.Poll(Now.AddSeconds(10)).Count == 0 && Directory.GetFiles(fixture.Root).Length == 1, "Antigravity unchanged file reports nothing twice and reading leaves no files behind");

            using var flight = new AntigravityFixture(epoch);
            flight.Step(1, 0, null); flight.Generation(0, Generation(new ulong[] { 1 }, null));
            using var live = new AntigravityLogWatcher(new[] { flight.Root });
            Check(live.Poll(Now).Count == 0 && live.Active(Now).Count == 1 && live.Active(Now)[0].Phase == "Waiting", "Antigravity call in flight is awaited, not recorded");
            flight.Step(1, 0, 8); flight.Generation(0, Generation(new ulong[] { 1 }, 400, 100, 3, 5));
            var done = live.Poll(Now.AddSeconds(10));
            Check(done.Count == 1 && live.Active(Now.AddSeconds(10)).Count == 0, "Antigravity call is reported once it completes");
            Near(done[0].MakeRecord().Tps, 60, "Antigravity completed call speed");
            flight.Step(5, 20, null);
            live.Poll(Now.AddSeconds(20));
            Check(live.Active(Now.AddSeconds(20)).Count == 1 && live.Active(Now.AddMinutes(20)).Count == 0, "Antigravity request that never completes stops counting as in flight");

            using var abandoned = new AntigravityFixture(epoch);
            abandoned.Step(1, 0, null); abandoned.Generation(0, Generation(new ulong[] { 1 }, null));
            abandoned.Generation(1, new byte[] { 0x0A, 0x7F, 0x00 });
            abandoned.Step(2, 10, 16); abandoned.Generation(2, Generation(new ulong[] { 2 }, 300, 0, 2, 4));
            using var skipping = new AntigravityLogWatcher(new[] { abandoned.Root });
            var kept = skipping.Poll(Now);
            Check(kept.Count == 1 && kept[0].Key.EndsWith(":exec-1:2", StringComparison.Ordinal), "Antigravity abandoned and malformed rows do not hold up later calls");
        }
    }

    private static Dictionary<string, object?> WithSeed(this Dictionary<string, object?> value, int? seed)
    { if (seed != null) value["seedLength"] = seed; return value; }
    private static Dictionary<string, object?> WithSurface(this Dictionary<string, object?> value, string? surface)
    { if (surface != null) value["surfaceOp"] = surface; return value; }
    private static byte[] Frame(string lines)
    {
        using var memory = new MemoryStream();
        using (var encoder = new CompressionStream(memory, leaveOpen: true)) encoder.Write(Encoding.UTF8.GetBytes(lines));
        return memory.ToArray();
    }

    private sealed class DirectoryFixture : IDisposable
    {
        public string Root { get; } = Path.Combine(Path.GetTempPath(), "speedtracker-parity-" + Guid.NewGuid().ToString("N"));
        public DirectoryFixture() { Directory.CreateDirectory(Root); }
        public string Put(string relative, string content)
        {
            var path = Path.Combine(Root, relative.Replace('/', Path.DirectorySeparatorChar));
            Directory.CreateDirectory(Path.GetDirectoryName(path)!); File.WriteAllText(path, content); return path;
        }
        public void Dispose() { Directory.Delete(Root, recursive: true); }
    }
    private sealed class SessionWatcher : IDisposable
    {
        private static readonly Type Type = typeof(SessionLogParser).Assembly.GetType("SpeedTracker.Core.SessionLogWatcher", throwOnError: true)!;
        private readonly object watcher;
        public SessionWatcher(string root) { watcher = Activator.CreateInstance(Type, new object[] { new[] { new LogSource("DeepSeek CLI", root) } })!; }
        public IReadOnlyList<LogRecord> Poll(DateTimeOffset now) => (IReadOnlyList<LogRecord>)Type.GetMethod("Poll")!.Invoke(watcher, new object[] { now })!;
        public IReadOnlyList<LiveCall> Active(DateTimeOffset now) => (IReadOnlyList<LiveCall>)Type.GetMethod("Active")!.Invoke(watcher, new object[] { now })!;
        public bool HasLogs => (bool)Type.GetMethod("HasLogs")!.Invoke(watcher, new object[] { "DeepSeek CLI" })!;
        public IReadOnlyDictionary<string, string> Errors => (IReadOnlyDictionary<string, string>)Type.GetProperty("Errors")!.GetValue(watcher)!;
        public void Dispose() => ((IDisposable)watcher).Dispose();
    }
    private sealed class SqliteFixture : IDisposable
    {
        private readonly DirectoryFixture files = new();
        public string Root => files.Root;
        public string Database => Path.Combine(Root, "opencode.db");
        private readonly SqliteConnection writer;
        private int sequence;
        public SqliteFixture()
        {
            writer = new(new SqliteConnectionStringBuilder { DataSource = Database, Pooling = false }.ToString()); writer.Open(); Sql("PRAGMA journal_mode=WAL");
        }
        public void Schema(bool current, bool legacy, bool indexed = true)
        {
            if (current)
            {
                Sql("CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,type TEXT NOT NULL,seq INTEGER NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL)");
                if (indexed) Sql("CREATE INDEX session_message_created ON session_message(time_created)");
                Sql("CREATE UNIQUE INDEX session_message_sequence ON session_message(session_id,seq)");
            }
            if (legacy)
            {
                Sql("CREATE TABLE session(id TEXT PRIMARY KEY,time_updated INTEGER NOT NULL); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL); CREATE INDEX message_session_created ON message(session_id,time_created,id); CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT NOT NULL,session_id TEXT NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL); CREATE INDEX part_message ON part(message_id,id)");
            }
        }
        private void Sql(string sql) { using var command = writer.CreateCommand(); command.CommandText = sql; command.ExecuteNonQuery(); }
        public void Message(string id, string session, long created, object data, bool current = true)
        {
            if (!current)
            {
                using var sessionCommand = writer.CreateCommand(); sessionCommand.CommandText = "INSERT OR REPLACE INTO session VALUES($session,$time)";
                sessionCommand.Parameters.AddWithValue("$session", session); sessionCommand.Parameters.AddWithValue("$time", created + 30000); sessionCommand.ExecuteNonQuery();
            }
            using var command = writer.CreateCommand();
            command.CommandText = current ? "INSERT INTO session_message VALUES($id,$session,'assistant',$seq,$time,$time,$data)" : "INSERT INTO message VALUES($id,$session,$time,$time,$data)";
            command.Parameters.AddWithValue("$id", id); command.Parameters.AddWithValue("$session", session); command.Parameters.AddWithValue("$time", created);
            if (current) command.Parameters.AddWithValue("$seq", sequence++);
            command.Parameters.AddWithValue("$data", JsonSerializer.Serialize(data)); command.ExecuteNonQuery();
        }
        public void Update(string id, object data)
        {
            using var command = writer.CreateCommand(); command.CommandText = "UPDATE session_message SET data=$data,time_updated=time_updated+1 WHERE id=$id";
            command.Parameters.AddWithValue("$data", JsonSerializer.Serialize(data)); command.Parameters.AddWithValue("$id", id); command.ExecuteNonQuery();
        }
        public void UpdateAll(object data)
        {
            using var command = writer.CreateCommand(); command.CommandText = "UPDATE session_message SET data=$data,time_updated=time_updated+1";
            command.Parameters.AddWithValue("$data", JsonSerializer.Serialize(data)); command.ExecuteNonQuery();
        }
        public void Part(string id, string message, string type, long persisted, object? time, bool synthetic = false, bool ignored = false, string? status = null)
        {
            using var command = writer.CreateCommand(); command.CommandText = "INSERT INTO part VALUES($id,$message,'fixture-session',$time,$time,$data)";
            command.Parameters.AddWithValue("$id", id); command.Parameters.AddWithValue("$message", message); command.Parameters.AddWithValue("$time", persisted);
            command.Parameters.AddWithValue("$data", JsonSerializer.Serialize(new { type, time, synthetic, ignored, state = new { status } })); command.ExecuteNonQuery();
        }
        public void Dispose() { writer.Dispose(); files.Dispose(); }
    }

    private sealed class AntigravityFixture : IDisposable
    {
        private readonly DirectoryFixture files = new();
        private readonly SqliteConnection writer;
        private readonly DateTimeOffset epoch;
        public string Root => files.Root;
        public AntigravityFixture(DateTimeOffset epoch)
        {
            this.epoch = epoch;
            writer = new(new SqliteConnectionStringBuilder { DataSource = Path.Combine(Root, "11111111-0000-4000-8000-000000000001.db"), Pooling = false }.ToString()); writer.Open();
            Run("CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY (idx))");
            Run("CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, status integer NOT NULL DEFAULT 0, metadata blob, step_payload blob, PRIMARY KEY (idx))");
        }
        private static byte[] Stamp(DateTimeOffset time)
        {
            var seconds = (ulong)time.ToUnixTimeSeconds(); var nanos = (ulong)(time.Ticks % TimeSpan.TicksPerSecond * 100);
            return Field(1, seconds).Concat(Field(2, nanos)).ToArray();
        }
        private static byte[] Var(ulong value)
        {
            var bytes = new List<byte>();
            do { var b = (byte)(value & 0x7F); value >>= 7; bytes.Add(value == 0 ? b : (byte)(b | 0x80)); } while (value != 0);
            return bytes.ToArray();
        }
        private static byte[] Field(int field, ulong value) => Var((ulong)(field << 3)).Concat(Var(value)).ToArray();
        private static byte[] Wrap(int field, byte[] value) => Var((ulong)(field << 3 | 2)).Concat(Var((ulong)value.Length)).Concat(value).ToArray();
        public void Step(int index, double created, double? completed)
        {
            var metadata = Wrap(1, Stamp(epoch.AddSeconds(created))).Concat(Field(3, 7));
            if (completed is double end) metadata = metadata.Concat(Wrap(8, Stamp(epoch.AddSeconds(end))));
            Run("INSERT OR REPLACE INTO steps (idx, step_type, status, metadata, step_payload) VALUES ($index, 15, 3, $data, $payload)", index, metadata.ToArray(), Encoding.UTF8.GetBytes("reply text"));
        }
        public void Generation(int index, byte[] data) => Run("INSERT OR REPLACE INTO gen_metadata (idx, data, size) VALUES ($index, $data, 0)", index, data);
        private void Run(string sql, int? index = null, byte[]? data = null, byte[]? payload = null)
        {
            using var command = writer.CreateCommand(); command.CommandText = sql;
            if (index != null) command.Parameters.AddWithValue("$index", index.Value);
            if (data != null) command.Parameters.AddWithValue("$data", data);
            if (payload != null) command.Parameters.AddWithValue("$payload", payload);
            command.ExecuteNonQuery();
        }
        public void Dispose() { writer.Dispose(); files.Dispose(); }
    }
}
