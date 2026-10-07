using System.Text;
using System.Text.Json;
using Microsoft.Data.Sqlite;
using SpeedTracker.Core;
using ZstdSharp;

internal static class Program
{
    private static int checks;
    private static int failures;
    private static readonly List<string> failed = new();

    private static void Check(bool ok, string label, string? detail = null)
    {
        checks++;
        if (ok) return;
        failures++;
        failed.Add(detail == null ? label : $"{label} — {detail}");
        Console.WriteLine($"  FAIL {label}{(detail == null ? "" : ": " + detail)}");
    }
    private static void Near(double? actual, double expected, string label, double tolerance = 0.02)
        => Check(actual.HasValue && Math.Abs(actual.Value - expected) <= tolerance, label, $"expected ≈{expected}, got {(actual?.ToString() ?? "null")}");
    private static void Phase(string name) => Console.WriteLine($"scenario: {name}");

    private static string Js(params object[] rows) => string.Join('\n', rows.Select(row => JsonSerializer.Serialize(row))) + "\n";
    private static string Iso(DateTimeOffset time) => time.UtcDateTime.ToString("yyyy-MM-ddTHH:mm:ss.fffZ");
    private static long Ms(DateTimeOffset time) => time.ToUnixTimeMilliseconds();
    private static void Append(string path, string text)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        File.AppendAllText(path, text);
    }
    private static void WriteZstd(string path, params string[] frames)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        using var file = new FileStream(path, FileMode.Create, FileAccess.Write);
        foreach (var frame in frames)
        {
            using var compressor = new CompressionStream(file, leaveOpen: true);
            var bytes = Encoding.UTF8.GetBytes(frame);
            compressor.Write(bytes, 0, bytes.Length);
        }
    }
    private static RequestRecord? ByKey(IEnumerable<RequestRecord> records, string key) => records.FirstOrDefault(r => r.SourceKey == key);

    private static int Main()
    {
        var root = Path.Combine(Path.GetTempPath(), "speedtracker-smoke-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        var home = Path.Combine(root, "home");
        var logs = Path.Combine(root, "logs");
        var now = DateTimeOffset.UtcNow;
        var sampler = new FakeSampler();
        var history = new HistoryStore(home);
        var sources = new[]
        {
            new LogSource("Claude Code", Path.Combine(logs, "claude", "projects")),
            new LogSource("Codex", Path.Combine(logs, "codex", "sessions")),
            new LogSource("OMP", Path.Combine(logs, "omp", "agent", "sessions")),
            new LogSource("opencode", Path.Combine(logs, "opencode")),
            new LogSource("DeepSeek CLI", Path.Combine(logs, "dsh", "sessions")),
        };
        try
        {
            // ---- Discovery ---------------------------------------------------------------
            Phase("passive Windows-path discovery is additive and credential-free");
            var discovered = LogDiscovery.DefaultSources(Path.Combine(root, "fakeuser"));
            Check(discovered.Any(s => s.Harness == "Claude Code" && s.Root.EndsWith(Path.Combine(".claude", "projects"))), "Claude Code default root discovered");
            Check(discovered.Any(s => s.Harness == "DeepSeek CLI" && s.Root.EndsWith(Path.Combine(".dsh", "sessions"))), "DeepSeek CLI default root discovered");
            Check(discovered.Any(s => s.Harness == "opencode" && s.Root.Contains(Path.Combine(".local", "share", "opencode"))), "XDG-like opencode root discovered");
            Check(discovered.Any(s => s.Harness == "OMP" && s.Root.EndsWith(Path.Combine("agent", "sessions"))), "OMP root discovered");
            Check(discovered.Any(s => s.Harness == "Gemini CLI" && s.Root.EndsWith(Path.Combine(".gemini", "tmp"))), "Gemini CLI default root discovered");
            Check(discovered.Any(s => s.Harness == "Antigravity" && s.Root.EndsWith(Path.Combine(".gemini", "antigravity", "conversations"))), "Antigravity default root discovered");
            Check(HarnessNames.Classify("/opt/homebrew/bin/node", "node /opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js -p secret") == "Gemini CLI", "Gemini CLI script classified without prompt arguments");
            Check(HarnessNames.Classify("/Applications/Antigravity.app/Contents/Resources/bin/language_server") == "Antigravity", "Antigravity language server classified inside its editor bundle");
            Check(HarnessNames.Classify("C:/Users/x/AppData/Local/Programs/Antigravity/resources/app/extensions/antigravity/bin/language_server_windows_x64.exe") == "Antigravity", "Antigravity Windows language server classified");
            Check(HarnessNames.Classify("/Users/x/.local/bin/agy") == "Antigravity", "Antigravity terminal agent classified");
            Check(HarnessNames.Classify("/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper.app/Contents/MacOS/Antigravity Helper") == null, "Antigravity editor helper not treated as a harness");
            Check(HarnessNames.Classify("/Applications/Other.app/Contents/Resources/bin/language_server") == null, "another editor's language server not treated as Antigravity");
            var machine = Path.Combine(root, "presence");
            Directory.CreateDirectory(machine);
            Check(HarnessPresence.Installed(new[] { machine }, "").Count == 0, "an empty machine has no installed harnesses");
            void Put(string relative) { var path = Path.Combine(machine, relative.Replace('/', Path.DirectorySeparatorChar)); Directory.CreateDirectory(Path.GetDirectoryName(path)!); File.WriteAllText(path, ""); }
            Put(".local/bin/claude"); Put("AppData/Roaming/npm/gemini.cmd"); Put(".bun/bin/omp.exe"); Put(".nvm/versions/node/v22.1.0/bin/codex");
            Directory.CreateDirectory(Path.Combine(machine, "AppData", "Local", "Programs", "Antigravity"));
            var present = HarnessPresence.Installed(new[] { machine }, "");
            Check(present.SetEquals(new[] { "Claude Code", "Gemini CLI", "OMP", "Codex", "Antigravity" }), "installed harnesses found by command, launcher script, version-manager folder and app folder", string.Join(',', present.Order()));
            var tools = Path.Combine(root, "presence-tools"); Directory.CreateDirectory(tools); File.WriteAllText(Path.Combine(tools, "dsh"), "");
            Check(HarnessPresence.Installed(new[] { Path.Combine(root, "nobody") }, tools).SetEquals(new[] { "DeepSeek CLI" }), "the search path is honoured for harnesses installed outside the home folder");
            Check(HarnessPresence.Commands.All(pair => HarnessNames.Classify("/usr/local/bin/" + pair.Command) == pair.Harness), "every harness the scan can report is named the same by the process classifier", string.Join(',', HarnessPresence.Commands.Where(pair => HarnessNames.Classify("/usr/local/bin/" + pair.Command) != pair.Harness).Select(pair => pair.Command)));
            Check(LogDiscovery.DefaultSources(Path.Combine(root, "fakeuser")).All(s => !s.Root.Contains("opencode.db")), "discovery returns directories only, never database or credential files");
            Check(HarnessNames.Classify("/usr/local/bin/omp") == "OMP", "native harness executable classified");
            Check(HarnessNames.Classify("/opt/homebrew/bin/node", "node /usr/lib/node_modules/opencode-ai/bin/opencode --prompt secret") == "opencode", "interpreter script path classified without prompt arguments");
            Check(HarnessNames.Classify("/usr/bin/python3", "python3 -c 'print(1)'") == null, "unrelated interpreter not classified");
            Check(HarnessNames.Classify("C:/Users/x/AppData/Local/dsh/dsh.exe") == "DeepSeek CLI", "official dsh classified as DeepSeek CLI");
            Check(HarnessNames.Classify("/Applications/Claude.app/Contents/MacOS/Claude") == null, "desktop host app not treated as a harness");

            // ---- Claude Code ------------------------------------------------------------
            Phase("Claude Code session log produces one attributed call with real TTFT");
            var claudeStart = now.AddSeconds(-120);
            var claudeFile = Path.Combine(logs, "claude", "projects", "p1", "session.jsonl");
            Append(claudeFile, Js(
                new { type = "user", timestamp = Iso(claudeStart), message = new { role = "user" } },
                new { type = "assistant", timestamp = Iso(claudeStart.AddSeconds(5)), thinkingDurationMs = 2000, message = new { id = "msg_claude_1", model = "claude-sonnet-4", usage = new { output_tokens = 3, input_tokens = 900, cache_read_input_tokens = 0 }, content = new object[] { new { type = "thinking" } } } },
                new { type = "assistant", timestamp = Iso(claudeStart.AddSeconds(6)), message = new { id = "msg_claude_1", model = "claude-sonnet-4", usage = new { output_tokens = 300, input_tokens = 900, cache_read_input_tokens = 100 }, content = new object[] { new { type = "text" } } } },
                new { type = "system", timestamp = Iso(claudeStart.AddSeconds(6)) }));

            // The machine in this test has Pi installed and nothing else, whatever the real one has.
            IReadOnlySet<string> OnlyPi() => new HashSet<string> { "Pi" };
            using var tracker = new TrackerService(history, sampler, sources, Path.Combine(root, "fakeuser"), OnlyPi);
            Check(tracker.Harnesses.Select(h => h.Name).SequenceEqual(new[] { "Pi" }) && tracker.Harnesses[0].IsInstalled, "at start, before any log is read, only installed harnesses are offered", string.Join(',', tracker.Harnesses.Select(h => h.Name)));
            var changes = 0;
            tracker.Changed += () => changes++;
            tracker.Poll(now);
            var records = history.Read();
            var claude = ByKey(records, "claude:msg_claude_1");
            Check(claude != null, "Claude Code call recorded from session log");
            if (claude != null)
            {
                Check(claude.Harness == "Claude Code" && claude.Model == "claude-sonnet-4", "harness and model attributed", $"{claude.Harness}/{claude.Model}");
                Check(claude.Source == "log" && claude.Format == "unknown", "source marked as log without invented wire format");
                Near(claude.Ttft, 3, "TTFT includes the thinking block duration");
                Near(claude.Generation, 3, "generation window measured from first token");
                Near(claude.Tps, 100, "throughput derived from the generation window");
                Check(claude.InputTokens == 1000 && claude.CachedInputTokens == 100, "input and cached input tokens aggregated", $"{claude.InputTokens}/{claude.CachedInputTokens}");
            }

            // ---- Codex live transitions --------------------------------------------------
            Phase("Codex live phase waits, thinks, streams, then finalizes exactly once");
            now = now.AddSeconds(5); // Newly created session files are discovered on the four-second scan cadence.
            var codexStart = now.AddSeconds(-90);
            var codexFile = Path.Combine(logs, "codex", "sessions", "2026", "10", "08", "rollout-x.jsonl");
            Append(codexFile, Js(
                new { type = "session_meta", timestamp = Iso(codexStart), payload = new { session_id = "codex_sess", model_provider = "openai" } },
                new { type = "turn_context", timestamp = Iso(codexStart), payload = new { model = "gpt-5-codex" } },
                new { type = "response_item", timestamp = Iso(codexStart), payload = new { type = "message", role = "user" } }));
            tracker.Poll(now.AddSeconds(1));
            Check(tracker.Active.Any(c => c.Harness == "Codex" && c.Phase == "Waiting"), "Codex reported waiting before the first token", string.Join(',', tracker.Active.Select(c => c.Harness + ":" + c.Phase)));
            Append(codexFile, Js(new { type = "event_msg", timestamp = Iso(codexStart.AddMilliseconds(400)), payload = new { type = "item_completed", started_at_ms = Ms(codexStart.AddMilliseconds(400)), item = new { type = "reasoning" } } }));
            tracker.Poll(now.AddSeconds(2));
            Check(tracker.Active.Any(c => c.Harness == "Codex" && c.Phase == "Thinking"), "Codex reported thinking once reasoning started", string.Join(',', tracker.Active.Select(c => c.Harness + ":" + c.Phase)));
            Append(codexFile, Js(new { type = "event_msg", timestamp = Iso(codexStart.AddMilliseconds(900)), payload = new { type = "item_completed", started_at_ms = Ms(codexStart.AddMilliseconds(900)), item = new { type = "agentMessage" } } }));
            tracker.Poll(now.AddSeconds(3));
            Check(tracker.Active.Any(c => c.Harness == "Codex" && c.Phase == "Streaming"), "Codex reported streaming once visible output started");
            Append(codexFile, Js(new { type = "token_usage_record", timestamp = Iso(codexStart.AddSeconds(2)), payload = new { response_id = "resp_1", usage = new { input_tokens = 1000, cached_input_tokens = 800, output_tokens = 120, reasoning_output_tokens = 40 } } }));
            tracker.Poll(now.AddSeconds(4));
            records = history.Read();
            var codex = ByKey(records, "codex:resp_1");
            Check(codex != null, "Codex call finalized from usage record");
            if (codex != null)
            {
                Near(codex.Ttft, 0.4, "Codex TTFT from reasoning item start");
                Near(codex.FirstVisible, 0.9, "Codex first-visible from agent message start");
                Near(codex.Tps, 75, "Codex throughput over its generation window");
                Check(codex.ReasoningTokens == 40 && codex.CachedInputTokens == 800, "Codex reasoning and cached token detail preserved");
            }
            Check(!tracker.Active.Any(c => c.Harness == "Codex"), "Codex cleared from live once the response completed");

            // ---- OMP (Pi shares the parser) ----------------------------------------------
            Phase("OMP self-reported timing is used verbatim");
            now = now.AddSeconds(10);
            var ompStart = now.AddSeconds(-80);
            Append(Path.Combine(logs, "omp", "agent", "sessions", "x.jsonl"), Js(
                new { type = "model_change", model = "anthropic/claude-opus-4" },
                new { type = "message", timestamp = Iso(ompStart.AddSeconds(2)), message = new { role = "assistant", model = "claude-opus-4", provider = "anthropic", timestamp = Iso(ompStart), duration = 2000, ttft = 250, stopReason = "stop", responseId = "omp_1", usage = new { output = 400, input = 50, cacheRead = 10, reasoningTokens = 5 } } }));
            tracker.Poll(now.AddSeconds(5));
            records = history.Read();
            var omp = ByKey(records, "omp:omp_1");
            Check(omp != null, "OMP call recorded");
            if (omp != null)
            {
                Near(omp.Ttft, 0.25, "OMP TTFT from log-reported ttft");
                Near(omp.Tps, 400 / 1.75, "OMP throughput excludes its first-token wait");
                Check(omp.InputTokens == 60, "OMP input includes cache read", omp.InputTokens?.ToString());
            }

            // ---- DeepSeek (official dsh), zstd frames + generation selection -------------
            Phase("DeepSeek zstd concatenated frames, generation selection and cross-source dedup");
            now = now.AddSeconds(15);
            var dsA = now.AddSeconds(-60);
            string DsEvent(long seq, DateTimeOffset time, object data, object? chunk = null) => Js(chunk == null
                ? new { type = "step/start", seq, time = Ms(time), data }
                : new { type = "chunk", seq, time = Ms(time), data, chunk });
            var header = Js(new { type = "session", version = 3, id = "ds_sess_a", createdAt = Ms(dsA), cwd = "/tmp", isSeeded = false });
            WriteZstd(Path.Combine(logs, "dsh", "sessions", "proj", "a", "session.v3.jsonl.zstd"),
                header,
                DsEvent(1, dsA, new { turn = 1, step = 1 }),
                Js(new { type = "assistant/message", seq = 4, time = Ms(dsA.AddMilliseconds(2000)), data = new { turn = 1, step = 1, message = new { id = "ds-1", role = "assistant", source = new { kind = "model", provider = "deepseek", model = "deepseek-v4" } }, usage = new { inputTokens = 100, outputTokens = 50, cacheReadTokens = 20, reasoningTokens = 10 }, stream = new object[] { new { type = "chunk", time = Ms(dsA.AddMilliseconds(500)), chunk = new { type = "block-start", index = 0, blockType = "reasoning" } }, new { type = "chunk", time = Ms(dsA.AddSeconds(2)), chunk = new { type = "finish", reason = new { kind = "stop" } } } } } }) +
                Js(new { type = "step/end", seq = 5, time = Ms(dsA.AddMilliseconds(2700)), data = new { turn = 1, step = 1 } }));
            // A lower generation in the same directory must never be counted.
            Append(Path.Combine(logs, "dsh", "sessions", "proj", "a", "session.v2.jsonl"),
                Js(new { type = "session", version = 2, id = "ds_sess_a", createdAt = Ms(dsA), isSeeded = false }) +
                Js(new { type = "assistant/message", seq = 4, time = Ms(dsA.AddMilliseconds(2000)), data = new { turn = 1, step = 1, message = new { id = "ds-1", role = "assistant", source = new { kind = "model", provider = "deepseek", model = "deepseek-v4" } }, usage = new { inputTokens = 100, outputTokens = 999 } } }));
            // An identical call re-published under a different project must dedup on source key.
            Append(Path.Combine(logs, "dsh", "sessions", "proj", "b", "session.v3.jsonl"),
                Js(new { type = "session", version = 3, id = "ds_sess_b", createdAt = Ms(dsA), isSeeded = false }) +
                DsEvent(1, dsA, new { turn = 1, step = 1 }) +
                Js(new { type = "assistant/message", seq = 4, time = Ms(dsA.AddMilliseconds(2000)), data = new { turn = 1, step = 1, message = new { id = "ds-1", role = "assistant", source = new { kind = "model", provider = "deepseek", model = "deepseek-v4" } }, usage = new { inputTokens = 100, outputTokens = 50, cacheReadTokens = 20, reasoningTokens = 10 }, stream = new object[] { new { type = "chunk", time = Ms(dsA.AddMilliseconds(500)), chunk = new { type = "block-start", index = 0, blockType = "reasoning" } }, new { type = "chunk", time = Ms(dsA.AddSeconds(2)), chunk = new { type = "finish", reason = new { kind = "stop" } } } } } }));
            tracker.Poll(now.AddSeconds(6));   // metadata pass: resolves the inherited cut
            tracker.Poll(now.AddSeconds(7));   // response pass
            records = history.Read();
            var deep = records.Where(r => r.SourceKey == "deepseek:ds-1").ToArray();
            Check(deep.Length == 1, "exactly one DeepSeek record across highest generation and duplicate source", $"found {deep.Length}");
            if (deep.Length == 1)
            {
                Near(deep[0].Ttft, 0.5, "DeepSeek TTFT from first generated metadata time");
                Near(deep[0].Generation, 1.5, "DeepSeek generation ends at stream finish, not step/end");
                Near(deep[0].Tps, 50 / 1.5, "DeepSeek throughput from reported usage");
                Check(deep[0].InputTokens == 120 && deep[0].CachedInputTokens == 20 && deep[0].ReasoningTokens == 10, "DeepSeek input aggregates cache read, reasoning kept", $"{deep[0].InputTokens}/{deep[0].CachedInputTokens}/{deep[0].ReasoningTokens}");
                Check(deep[0].Harness == "DeepSeek CLI" && deep[0].Model == "deepseek-v4", "DeepSeek harness name stable and model read from evidence");
                Check(deep[0].Aborted == false, "completed DeepSeek turn not marked aborted");
            }

            Phase("DeepSeek live phases follow actual request evidence, and stale activity expires");
            now = now.AddSeconds(10);
            var dsC = now.AddSeconds(-10);
            var liveFile = Path.Combine(logs, "dsh", "sessions", "proj", "c", "session.v3.jsonl");
            Append(liveFile, Js(new { type = "session", version = 3, id = "ds_sess_c", createdAt = Ms(dsC), isSeeded = false }) + DsEvent(1, dsC, new { turn = 1, step = 1 }));
            tracker.Poll(now.AddSeconds(8));
            tracker.Poll(now.AddSeconds(9));
            Check(tracker.Active.Any(c => c.Harness == "DeepSeek CLI" && c.Phase == "Waiting"), "DeepSeek waiting after request start", string.Join(',', tracker.Active.Select(c => c.Harness + ":" + c.Phase)));
            tracker.Poll(now.AddSeconds(11));
            Check(tracker.Active.Any(c => c.Harness == "DeepSeek CLI" && c.Phase == "Waiting" && c.Rate == null), "DeepSeek v3 keeps unreported live token timing unavailable");
            Append(liveFile, Js(new { type = "assistant/message", seq = 4, time = Ms(dsC.AddSeconds(2)), data = new { turn = 1, step = 1, message = new { id = "ds-2", role = "assistant", source = new { kind = "model", provider = "deepseek", model = "deepseek-v4" } }, usage = new { inputTokens = 10, outputTokens = 20 }, stream = new object[] { new { type = "chunk", time = Ms(dsC.AddSeconds(2)), chunk = new { type = "finish", reason = new { kind = "stop" } } } } } }));
            tracker.Poll(now.AddSeconds(12));
            Check(!tracker.Active.Any(c => c.Harness == "DeepSeek CLI"), "DeepSeek live cleared once the assistant response was logged");
            Check(ByKey(history.Read(), "deepseek:ds-2") != null, "DeepSeek live call landed in history exactly once");

            // ---- OpenCode ---------------------------------------------------------------
            Phase("OpenCode reads v2 SQLite, legacy SQLite and legacy JSON without prompts");
            var ocRoot = Path.Combine(logs, "opencode");
            var v2Created = now.AddSeconds(-4);
            var legacyCreated = now.AddSeconds(-6);
            CreateOpenCodeDatabase(Path.Combine(ocRoot, "opencode.db"), v2Created, legacyCreated);
            CreateOpenCodeJson(ocRoot, now.AddSeconds(-8));
            tracker.Poll(now.AddSeconds(13));
            tracker.Poll(now.AddSeconds(14));
            tracker.Poll(now.AddSeconds(18)); // Allow the independent four-second legacy JSON discovery cadence.
            records = history.Read();
            var oc1 = ByKey(records, "opencode:msg_oc1");
            var oc3 = ByKey(records, "opencode:msg_oc3");
            var oc4 = ByKey(records, "opencode:msg_oc4");
            Check(oc1 != null && oc3 != null && oc4 != null, "OpenCode v2, legacy SQLite and legacy JSON telemetry all read", $"v2={oc1 != null} legacy={oc3 != null} json={oc4 != null}");
            Check(records.Count(r => r.SourceKey == "opencode:msg_oc1") == 1, "duplicate OpenCode metadata across tables is deduplicated");
            if (oc1 != null)
            {
                Check(oc1.Ttft == null, "OpenCode v2 leaves request-relative TTFT unknown");
                Near(oc1.Generation, 3, "OpenCode v2 retains independently timestamped generation");
                Near(oc1.Tps, 250 / 3d, "OpenCode v2 speed uses measured generation window");
                Check(Math.Abs((oc1.StartedAt - v2Created).TotalSeconds) < 0.02, "OpenCode v2 start uses observed metadata time", oc1.StartedAt.ToString("O"));
                Check(oc1.OutputTokens == 250 && oc1.InputTokens == 140 && oc1.CachedInputTokens == 30 && oc1.ReasoningTokens == 50, "OpenCode v2 tokens aggregate reasoning and cache", $"{oc1.OutputTokens}/{oc1.InputTokens}/{oc1.CachedInputTokens}");
                Check(oc1.Model == "gpt-5" && oc1.Route == "openai", "OpenCode v2 model and provider from metadata");
            }
            if (oc3 != null)
            {
                Near(oc3.Ttft, 0.1, "OpenCode legacy TTFT from reasoning part start");
                Near(oc3.FirstVisible, 0.5, "OpenCode legacy first-visible from text part start");
                Near(oc3.Generation, 3.1, "OpenCode legacy generation ends at step finish, before tool waits");
                Check(oc3.OutputTokens == 100, "OpenCode legacy output includes reasoning", oc3.OutputTokens.ToString());
            }
            Check(!records.Any(r => r.SourceKey == "opencode:msg_oc2"), "unresolved OpenCode assistant row is not recorded early");
            Check(tracker.Active.Any(c => c.Harness == "opencode" && c.Estimated == false), "OpenCode shows honest live phase while the row is unresolved");

            // ---- History queries --------------------------------------------------------
            Phase("history queries, malformed lines and held metrics behave for consumers");
            Append(Path.Combine(home, "history.jsonl"), "{\"broken\": true}\nnot json at all\n");
            var openCodeRecords = history.Read(new DashboardFilter { Harness = "opencode" });
            Check(openCodeRecords.Count == 3, "harness filter returns exactly the OpenCode calls", openCodeRecords.Count.ToString());
            Check(history.SkippedLines >= 2, "malformed history lines are counted, not hidden", history.SkippedLines.ToString());
            Check(history.Read().Count >= 8, "valid records survive malformed neighbour lines", history.Read().Count.ToString());

            var report = DashboardReport.Create(history.Read());
            Check(report.Groups.Any(g => g.Provider.Name == "Anthropic" && g.Harness == "Claude Code"), "provider × harness grouping attributes Anthropic to Claude Code", string.Join(';', report.Groups.Select(g => g.Provider.Name + "/" + g.Harness)));
            Check(report.Groups.Any(g => g.Provider.Key == "deepseek" && g.Harness == "DeepSeek CLI"), "DeepSeek endpoint group present");
            Check(report.Groups.Any(g => g.Provider.Key == "openai" && g.Harness == "Codex"), "OpenAI group present for Codex");
            Check(report.Providers.Any(p => p.Key == "anthropic" && p.Unverified && p.Evidence.Contains("Reported provider label")), "label-only attribution stays explicitly unverified");
            Check(report.Summary.Count == report.Records.Count && report.Summary.OutputTokens > 0, "summary covers the filtered records with real token totals");
            Check(report.Summary.MedianTPS.HasValue && report.Summary.LatencyCount >= 4 && report.Summary.SpeedCount >= 4, "median speed and latency sample counts populated", $"{report.Summary.MedianTPS}/{report.Summary.LatencyCount}/{report.Summary.SpeedCount}");
            Check(report.Trends.Count >= 1, "trend buckets produced");
            var filtered = DashboardReport.Create(history.Read(), new DashboardFilter { From = now.AddSeconds(-30), Through = now.AddDays(1) });
            Check(filtered.Records.All(r => r.StartedAt >= now.AddSeconds(-30)), "date filter excludes older calls");
            RequestRecord Facet(string harness, string host, string model, int key) => new() { StartedAt = now, Harness = harness, Route = host, UpstreamHost = host, Model = model, OutputTokens = 10, Source = "log", SourceKey = "facet:" + key };
            var facets = new[] { Facet("OMP", "api.openai.com", "gpt-6-astra", 1), Facet("Codex", "api.openai.com", "gpt-6-astra", 2), Facet("Codex", "api.openai.com", "gpt-6.1-sol", 3), Facet("OMP", "api.deepseek.com", "deepseek-flash", 4), Facet("Claude Code", "api.anthropic.com", "claude-opus-5-5", 5) };
            var byModel = DashboardReport.Create(facets, new DashboardFilter { Model = "gpt-6-astra" });
            Check(byModel.HarnessOptions.SequenceEqual(new[] { "Codex", "OMP" }) && byModel.ProviderOptions.Select(p => p.Key).SequenceEqual(new[] { "openai" }) && byModel.ModelOptions.Count == 4, "choosing a model offers only the harnesses and providers that used it", string.Join(',', byModel.HarnessOptions) + "|" + string.Join(',', byModel.ProviderOptions.Select(p => p.Key)));
            var byHarness = DashboardReport.Create(facets, new DashboardFilter { Harness = "OMP" });
            Check(byHarness.ModelOptions.SequenceEqual(new[] { "deepseek-flash", "gpt-6-astra" }) && byHarness.ProviderOptions.Select(p => p.Key).Order(StringComparer.Ordinal).SequenceEqual(new[] { "deepseek", "openai" }) && byHarness.HarnessOptions.Count == 3, "choosing a harness offers only its models and providers", string.Join(',', byHarness.ModelOptions));
            var byProvider = DashboardReport.Create(facets, new DashboardFilter { Provider = "openai" });
            Check(byProvider.HarnessOptions.SequenceEqual(new[] { "Codex", "OMP" }) && byProvider.ModelOptions.SequenceEqual(new[] { "gpt-6-astra", "gpt-6.1-sol" }), "choosing a provider offers only its harnesses and models", string.Join(',', byProvider.ModelOptions));
            var byTwo = DashboardReport.Create(facets, new DashboardFilter { Harness = "OMP", Provider = "openai" });
            Check(byTwo.ModelOptions.SequenceEqual(new[] { "gpt-6-astra" }) && byTwo.Harnesses.Count == 3 && byTwo.Models.Count == 4, "two filters narrow the third while the unfiltered lists keep the whole range", string.Join(',', byTwo.ModelOptions));
            Check(report.Records.Count > 0 && report.Records[0].StartedAt >= report.Records[^1].StartedAt, "report records are newest-first and non-empty", report.Records.Count.ToString());
            Check(history.Append(oc1!) == false, "re-appending a known source key is rejected");

            tracker.Target = "Claude Code";
            Near(tracker.HeldRate, 100, "held rate honors the selected target");
            Near(tracker.HeldTTFT, 3, "held TTFT honors the selected target");
            tracker.Target = "OMP";
            Near(tracker.HeldRate, 400 / 1.75, "target switch changes the held rate");
            tracker.Target = "Codex";
            Near(tracker.HeldRate, 75, "codex target rate held");
            tracker.Target = "opencode";
            Near(tracker.HeldRate, 250 / 3d, "OpenCode holds the newest independently measured generation rate");
            Near(tracker.HeldTTFT, 0.1, "OpenCode holds its last valid latency");
            Check(tracker.HeldRecord?.SourceKey == "opencode:msg_oc1", "held record exposes the newest accepted target call");
            Check(tracker.GetRecent(2, "opencode").Count == 2 && tracker.GetRecent(2, "opencode")[0].SourceKey == "opencode:msg_oc1", "recent harness-filtered history returns newest first");
            using (var reloaded = new TrackerService(new HistoryStore(home), null, sources, Path.Combine(root, "fakeuser"), OnlyPi))
            {
                Check(reloaded.Target == "opencode", "live target persisted across restart", reloaded.Target);
                Check(reloaded.EnhancedNetworkAvailable == false, "enhanced network unavailable without a sampler");
                reloaded.SetEnhancedNetwork(true);
                Check(!reloaded.EnhancedNetworkEnabled, "enhanced network cannot enable without a sampler");
                Check(reloaded.NetworkStatus.Contains("Standard-user log-first"), "network status explains the log-first default", reloaded.NetworkStatus);
                try { reloaded.Target = "Not A Harness"; Check(false, "unknown target rejected"); }
                catch (ArgumentException) { Check(true, "unknown target rejected"); }
            }
            tracker.Target = null;
            Check(tracker.Harnesses.Any(h => h.Name == "Claude Code" && h.HasLogs), "Claude Code reported as having readable telemetry");
            Check(tracker.Harnesses.Any(h => h.Name == "opencode" && h.HasLogs), "opencode reported as having readable telemetry");
            Check(tracker.Harnesses.Any(h => h.Name == "DeepSeek CLI" && h.HasLogs), "DeepSeek CLI reported as having readable telemetry");
            Check(tracker.Harnesses.Any(h => h.Name == "Pi" && h.IsInstalled && !h.HasLogs && h.Limitation!.Contains("Processes alone")), "installed harness without logs is offered and honest about the missing source");
            Check(!tracker.Harnesses.Any(h => h.Name is "Crush" or "Goose" or "Qwen Code"), "harnesses that are not on this machine are not offered", string.Join(',', tracker.Harnesses.Select(h => h.Name)));
            Check(changes > 0, "Changed event raised for consumers");

            // ---- Optional network sampler ----------------------------------------------
            Phase("optional sampler adds estimated live timing only where logs are absent");
            tracker.SetEnhancedNetwork(true);
            Check(tracker.EnhancedNetworkEnabled, "enhanced network enabled when the sampler is available");
            sampler.Push(new FlowSample(4242, "Aider", "api.anthropic.com", 0, 0, "conn-1"));
            tracker.Poll(now.AddSeconds(21));
            sampler.Push(new FlowSample(4242, "Aider", "api.anthropic.com", 0, 600, "conn-1"));
            tracker.Poll(now.AddSeconds(21.5));
            for (var step = 1; step <= 5; step++)
            {
                sampler.Push(new FlowSample(4242, "Aider", "api.anthropic.com", (ulong)(1200 * step), 600, "conn-1"));
                tracker.Poll(now.AddSeconds(21.5 + step * .5));
            }
            var live = tracker.Active.FirstOrDefault(c => c.Harness == "Aider");
            Check(live != null && live.Estimated, "network-only harness shows a live estimate, never a false exact metric");
            sampler.Remove("conn-1");
            tracker.Poll(now.AddSeconds(30));
            var network = history.Read().Where(r => r.Source == "network").ToArray();
            Check(network.Length == 1 && network[0].Harness == "Aider" && network[0].TokensEstimated, "network flow finished into one estimated record", $"count={network.Length}");
            Check(network.Length == 1 && network[0].Tps is > 0, "estimated throughput non-zero from real bytes");
            sampler.Push(new FlowSample(4242, "Aider", "api.anthropic.com", 0, 0, "conn-2"));
            tracker.Poll(now.AddSeconds(40));
            sampler.Push(new FlowSample(4242, "Aider", "api.anthropic.com", 0, 600, "conn-2"));
            tracker.Poll(now.AddSeconds(40.5));
            for (var step = 1; step <= 5; step++)
            {
                sampler.Push(new FlowSample(4242, "Aider", "api.anthropic.com", (ulong)(900 * step), 600, "conn-2"));
                tracker.Poll(now.AddSeconds(40.5 + step * .5));
            }
            sampler.Remove("conn-2");
            tracker.Poll(now.AddSeconds(50));
            Check(history.Read().Count(r => r.Source == "network") == 2, "a new connection identity is tracked separately instead of merging counters", history.Read().Count(r => r.Source == "network").ToString());
            Check(!tracker.Active.Any(c => c.Harness == "Aider"), "network live call cleared after the flow ended");
            tracker.Poll(now.AddMinutes(11));
            Check(tracker.Active.Count == 0, "stale in-flight activity expires instead of showing false activity", string.Join(',', tracker.Active.Select(c => c.Id)));
            FlowRegression.Run(Check, now);
            Check(HarnessRegression.Run() == 0, "harness parity regression passes", null);
            ProxyRegression.Run(Check).GetAwaiter().GetResult();
        }
        catch (Exception ex)
        {
            failures++;
            failed.Add("unhandled: " + ex);
            Console.WriteLine("  FAIL unhandled exception: " + ex);
        }
        finally
        {
            try { Directory.Delete(root, true); } catch (IOException) { }
        }
        Console.WriteLine($"\n{checks - failures}/{checks} checks passed");
        Console.WriteLine(failures == 0 ? "SMOKE PASS" : "SMOKE FAIL");
        return failures == 0 ? 0 : 1;
    }

    private static void CreateOpenCodeDatabase(string path, DateTimeOffset v2Created, DateTimeOffset legacyCreated)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        using var connection = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = path }.ToString());
        connection.Open();
        using (var schema = connection.CreateCommand())
        {
            schema.CommandText = """
                CREATE TABLE session(id TEXT PRIMARY KEY, time_updated INTEGER);
                CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);
                CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
                CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
                CREATE INDEX session_message_time ON session_message(time_created, id);
                CREATE INDEX message_session_time ON message(session_id, time_created, id);
                CREATE INDEX part_message_id_id_idx ON part(message_id,id);
                """;
            schema.ExecuteNonQuery();
        }
        void Run(string sql, params (string Name, object Value)[] parameters)
        {
            using var command = connection.CreateCommand();
            command.CommandText = sql;
            foreach (var parameter in parameters) command.Parameters.AddWithValue(parameter.Name, parameter.Value);
            command.ExecuteNonQuery();
        }
        Run("INSERT INTO session(id, time_updated) VALUES ($id, $time)", ("$id", "ses_1"), ("$time", Ms(v2Created)));
        // v2 completed assistant row: nested model/time/tokens and typed content metadata only.
        Run("INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data) VALUES ($id,$session,'assistant',3,$time,$time,$data)",
            ("$id", "msg_oc1"), ("$session", "ses_1"), ("$time", Ms(v2Created)),
            ("$data", JsonSerializer.Serialize(new
            {
                role = "assistant",
                model = new { id = "gpt-5", providerID = "openai" },
                time = new { created = Ms(v2Created), completed = Ms(v2Created.AddSeconds(3)) },
                tokens = new { input = 100, output = 200, reasoning = 50, cache = new { read = 30, write = 10 } },
                content = new object[]
                {
                    new { type = "reasoning", time = new { created = Ms(v2Created), completed = Ms(v2Created.AddMilliseconds(500)) } },
                    new { type = "text", text = "ignored", time = new { created = Ms(v2Created.AddMilliseconds(600)) } },
                    new { type = "tool", time = new { created = Ms(v2Created.AddMilliseconds(900)) } },
                }
            })));
        // v2 unresolved assistant row: live only, must never be recorded.
        Run("INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data) VALUES ($id,$session,'assistant',4,$time,$time,$data)",
            ("$id", "msg_oc2"), ("$session", "ses_1"), ("$time", Ms(v2Created.AddSeconds(1))),
            ("$data", JsonSerializer.Serialize(new
            {
                role = "assistant",
                model = new { id = "gpt-5", providerID = "openai" },
                time = new { created = Ms(v2Created.AddSeconds(1)) },
                tokens = new { input = 5, output = 0 },
                content = new object[] { new { type = "text", text = "ignored", time = new { created = Ms(v2Created.AddSeconds(1)) } } }
            })));
        // Legacy assistant row; the same id under a different table proves dedup by source key.
        var legacy = JsonSerializer.Serialize(new
        {
            role = "assistant",
            modelID = "claude-sonnet-4",
            providerID = "anthropic",
            time = new { created = Ms(legacyCreated), completed = Ms(legacyCreated.AddSeconds(4)) },
            tokens = new { input = 50, output = 80, reasoning = 20, cache = new { read = 10 } }
        });
        Run("INSERT INTO message(id, session_id, time_created, time_updated, data) VALUES ($id,$session,$time,$time,$data)",
            ("$id", "msg_oc3"), ("$session", "ses_1"), ("$time", Ms(legacyCreated)), ("$data", legacy));
        Run("INSERT INTO message(id, session_id, time_created, time_updated, data) VALUES ($id,$session,$time,$time,$data)",
            ("$id", "msg_oc1"), ("$session", "ses_1"), ("$time", Ms(v2Created)), ("$data", legacy));
        Run("INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) VALUES ($id,$message,$session,$time,$time,$data)",
            ("$id", "part_r"), ("$message", "msg_oc3"), ("$session", "ses_1"), ("$time", Ms(legacyCreated)),
            ("$data", JsonSerializer.Serialize(new { type = "reasoning", time = new { start = Ms(legacyCreated.AddMilliseconds(100)), end = Ms(legacyCreated.AddMilliseconds(400)) } })));
        Run("INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) VALUES ($id,$message,$session,$time,$time,$data)",
            ("$id", "part_t"), ("$message", "msg_oc3"), ("$session", "ses_1"), ("$time", Ms(legacyCreated)),
            ("$data", JsonSerializer.Serialize(new { type = "text", text = "ignored", time = new { start = Ms(legacyCreated.AddMilliseconds(500)), end = Ms(legacyCreated.AddSeconds(3)) } })));
        Run("INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) VALUES ($id,$message,$session,$time,$time,$data)",
            ("$id", "part_f"), ("$message", "msg_oc3"), ("$session", "ses_1"), ("$time", Ms(legacyCreated.AddMilliseconds(3200))),
            ("$data", JsonSerializer.Serialize(new { type = "step-finish", time = new { created = Ms(legacyCreated.AddMilliseconds(3200)) } })));
    }

    private static void CreateOpenCodeJson(string root, DateTimeOffset created)
    {
        var message = new
        {
            id = "msg_oc4",
            sessionID = "ses_json",
            role = "assistant",
            modelID = "gpt-5-mini",
            providerID = "openai",
            time = new { created = Ms(created), completed = Ms(created.AddSeconds(2)) },
            tokens = new { input = 20, output = 60, reasoning = 0, cache = new { read = 5 } }
        };
        Append(Path.Combine(root, "storage", "message", "ses_json", "msg_oc4.json"), JsonSerializer.Serialize(message, new JsonSerializerOptions { WriteIndented = true }));
        Append(Path.Combine(root, "storage", "part", "msg_oc4", "part_1.json"), JsonSerializer.Serialize(new { type = "text", text = "ignored", time = new { start = Ms(created.AddMilliseconds(200)), end = Ms(created.AddSeconds(2)) } }));
    }

    private sealed class FakeSampler : IFlowSampler
    {
        private readonly Dictionary<string, FlowSample> latest = new(StringComparer.Ordinal);
        public void Push(FlowSample sample) => latest[sample.ConnectionID ?? sample.Host] = sample;
        public void Remove(string connectionId) => latest.Remove(connectionId);
        public bool Available => true;
        public string Status => "Elevated connection-table sampling active (test stub).";
        public IReadOnlyList<FlowSample> Sample() => latest.Values.ToArray();
        public void Dispose() { }
    }
}
