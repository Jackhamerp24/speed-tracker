# AGENTS.md

Guide for an agent picking up this project. `README.md` is the user-facing description; this file is what you need to change the code safely.

## What this is

Speed Tracker is a macOS menu bar app that shows how fast the model behind a coding harness is responding: **TTFT** (time to first token) and **speed** (output tokens per second). It works for Claude Code, Codex, OMP and other harnesses without the user configuring anything. There is also a Windows tray app, written in Rust; its notes are in the last section, "Windows app in Rust".

It is modelled on [CodexBar](https://github.com/steipete/CodexBar): menu bar live status with an optional dashboard window for historical inspection.
## Requirements the owner has stated

These came from direct corrections. Treat them as fixed unless the owner says otherwise.

1. **Zero setup.** The app must detect harnesses and models on its own. Never make it depend on the user setting a base URL, an environment variable or a config file. The first version was a proxy that needed a base URL; that was rejected.
2. **Never modify a harness's configuration.** Detection is passive: read logs, observe traffic.
3. **Any harness, any model.** Claude Code, Codex, OMP and a DeepSeek harness were named; assume others.
4. **No false activity.** It must not show a harness as active when no model call is running.
5. **The displayed speed must not keep dropping to zero.** Pauses hold the last value.
6. **The target is chosen in the Live tab**: `Auto` (follow whichever harness is active) or one specific harness. Per-harness on/off switches in Settings were built first and rejected as a misreading; do not bring them back.
7. **Menu bar live status plus an optional dashboard window**; the dashboard is historical and does not replace the Live tab.

## How it works

Two passive sources are combined in `AutoDetector`:

1. **Session logs** (`SessionLogWatcher` + parsers in `SessionLogs.swift`). Each harness already writes every response to disk. These give the model name, exact token counts and timestamps. Finished-call records come from here.
2. **Network flow sampling** (`Nettop` + `FlowTracker` in `NetworkFlows.swift`). The app samples each harness process's TCP byte counters. An upload burst is a request, the first reply bytes are the first byte, steady data is text streaming. This drives the live view, fills in TTFT where a log lacks it, and is the only source for harnesses with no readable log.

A third, optional source is the local proxy (`ProxyServer`, port 4141), which measures a stream exactly if a harness's base URL is pointed at it. Nothing depends on it. Keep it working, keep it optional.

```
harness logs ─┐
              ├─> AutoDetector ─> TrackerEvent ─> MetricsStore ─> menu bar item + popover
nettop ───────┘                                        │
proxy (optional) ──────────────> TrackerEvent ─────────┘
                                                       └─> history.jsonl
```

`TrackerEvent` is `.started(LiveSnapshot)`, `.updated(LiveSnapshot)`, `.finished(UUID, RequestRecord?)`. A nil record means "the live call ended; another source records it, or it was not a generation".

## Layout

```
Sources/SpeedTrackerCore        UI-free detection, history queries and dashboard analytics
  Models.swift                  RequestRecord (the history schema), LiveSnapshot, TrackerEvent
  Dashboard.swift               provider identity, history queries, aggregations, trends and distributions
  SessionLogs.swift              LogRecord, ClaudeCodeLogParser, CodexLogParser, OMPLogParser, GeminiCLILogParser
  SessionLogWatcher.swift        finds and tails session files; bounded memory
  OpenCodeLogs.swift             read-only SQLite/current and legacy JSON telemetry
  DeepSeekLogs.swift             versioned dsh JSONL/Zstandard parser and watcher
  AntigravityLogs.swift          read-only conversation SQLite + minimal protobuf reader
  NetworkFlows.swift             Nettop sampler, FlowCall, FlowTracker state machine
  ProcessCatalog.swift           ProcessInspector, HarnessCatalog, HostCatalog
  AutoDetector.swift             orchestration, sampling cadence, live-view rules, log/flow join
  HistoryStore.swift              append-only history.jsonl and bounded historical reads
  ProxyServer.swift              optional proxy on NWListener
  StreamMeter.swift              SSE/NDJSON parsing
  ProxyConfig.swift              routes, config.json, AppPaths
  HarnessDetector.swift          User-Agent -> harness, proxy only
  HTTPParsing.swift              request-head and chunked-body parsing
Sources/SpeedTracker             menu-bar app plus dashboard window
  AppDelegate.swift              wires detector, status item and dashboard
  MetricsStore.swift             live state, target filter, accepted-record publisher
  StatusItemController.swift     NSStatusItem + NSPopover
  PopoverView.swift              Live, Models, Settings and dashboard entry
  DashboardStore.swift           historical filters, pagination and live updates
  DashboardView.swift            overview, trends, calls and inspector
  DashboardCharts.swift          Swift Charts visualizations
  DashboardWindowController.swift resizable dashboard lifecycle
  SetupTab.swift                 Settings tab
  Theme.swift                    HarnessStyle, semantic tokens, cards and charts
  Snapshot.swift                 offscreen PNG renders for popover/dashboard
  Diagnose.swift                 detection diagnostics
Tests/SpeedTrackerCoreTests      117 Core tests
Scripts/                         build, test, icon and proxy smoke scripts
Windows/Cargo.toml, src, tests   the Windows app, in Rust; see "Windows app in Rust" at the end
Windows/SpeedTracker.*           the earlier C# version: no longer built or shipped, awaiting removal
```

Two things are easy to confuse: `HarnessCatalog` classifies a **process** (automatic detection); `HarnessDetector` classifies a **User-Agent** (proxy only).

## Build, test, run

This Mac has the Xcode **Command Line Tools only, no Xcode**. That shapes several things below.

```bash
./Scripts/build_app.sh      # release build -> "Speed Tracker.app", ad-hoc signed
./Scripts/test.sh           # unit tests (adds the testing-macro plugin path CLT needs)
./Scripts/smoke_test.sh     # optional proxy, end to end against a mock provider
open "Speed Tracker.app"
```

After changing code, quit the running app before relaunching, or you will be looking at the old build:

```bash
pkill -f "Speed Tracker.app/Contents/MacOS/SpeedTracker"
```

### Constraints from having no Xcode

- **Do not write `@State`.** In this SDK it is a macro whose plugin ships only with Xcode, and the build fails. Store `State(initialValue:)` as a `let` and use `.wrappedValue` / `.projectedValue`. `@Published`, `@EnvironmentObject`, `@AppStorage` and `@ViewBuilder` are fine. `Binding` is passed explicitly, not with `@Binding`.
- **Tests use Swift Testing**, not XCTest (unavailable). Run them through `Scripts/test.sh`.
- The app bundle is assembled by script; there is no `.xcodeproj`.
- The app target has no unit tests. UI logic in `MetricsStore` is checked by snapshot and trace (below).

### Building inside a restricted agent environment

During Phase 1 verification on 2026-10-07, the default module-cache location was not writable. Isolating the caches let the installed Command Line Tools compile the project:

```bash
CLANG_MODULE_CACHE_PATH=/private/tmp/speedtracker-clang-cache \
SWIFT_MODULECACHE_PATH=/private/tmp/speedtracker-swift-cache \
./Scripts/build_app.sh
```

Use the same environment variables with `swift build` or `./Scripts/test.sh` when needed. SwiftPM's manifest sandbox and AppKit snapshot rendering also required execution outside the agent sandbox in that session. The successful build did not require installing or changing the toolchain; do not infer a toolchain mismatch from the initial cache-related diagnostics alone.

## Verifying a change

You cannot easily click the popover, and no agent has yet seen the real menu bar item. Use these instead.

**Render the popover** with demo data and read the PNG:

```bash
"Speed Tracker.app/Contents/MacOS/SpeedTracker" --snapshot out.png [--tab live|models|settings] [--idle] [--empty] [--dark] [--target NAME]
```

It also prints the menu bar title it would show. Check light and dark.

**See what detection sees** on this Mac:

```bash
"Speed Tracker.app/Contents/MacOS/SpeedTracker" --diagnose 30
```

**Trace the live app.** `SPEEDTRACKER_TRACE=1` logs every event and every menu bar title change to stderr:

```bash
open --env SPEEDTRACKER_TRACE=1 --stderr trace.log "Speed Tracker.app"
```

**Keep test runs out of real data** with `SPEEDTRACKER_HOME=<dir>` (config and history go there). The real data is in `~/Library/Application Support/SpeedTracker/`.

A convenient live test subject: if you are running as Claude Code, your own session is a harness. The trace will show your calls as `Claude Code`. That is real activity, not a false positive.

## Facts that were expensive to learn

### Reading network counters

- The kernel socket table (`sysctl net.inet.tcp.pcblist_n`) returns nothing without the Apple-only entitlement `com.apple.private.network.statistics`.
- `netstat -anvb` works from a shell but **returns nothing when its parent is a locally built, ad-hoc signed binary**, including the app launched normally. The cause was not found. Do not switch back to it.
- `nettop -L 1 -x -n -m tcp -J bytes_in,bytes_out [-p pid ...]` works from the app. One shot costs 15 to 30 ms of CPU. Running it continuously (`-L 0`) burns a full core; fractional `-s` is rejected. So the app launches it once per sample and adapts the rate: 10 Hz while a request awaits its first byte, 4 Hz while streaming, 1 Hz while a harness is open, a discovery scan every 3 to 5 s.

### What model traffic looks like (measured on Claude Code)

- A request is a large upload burst. **During and just after it the server sends HTTP/2 flow-control frames**, roughly 1 byte per kilobyte uploaded. `FlowTracker` ignores reply bytes within `uploadGrace` of an upload, otherwise TTFT reads as ~0.2 s.
- During hidden thinking Anthropic sends about 140 bytes every 1.2 s. That is not text. `sustained` requires data in most samples at 400 B/s or more.
- Some models reason silently for 10 s or more with no bytes at all. Such a call goes `dormant` rather than ending.
- First reply byte arrives about 0.2 s before the thinking block starts (1.60 s vs 1.85 s, 1.73 s vs 1.94 s on two calls).

### False positives already fixed; do not reintroduce

- The Claude desktop app's `Claude Helper` process keeps a connection to an Anthropic address and trickles data over it. It was recorded as model calls. Fixed by `HarnessCatalog.isHostAppProcess` (skip internals of Claude.app, ChatGPT.app, Codex.app) and by requiring a real stream (`FlowCall.isStream`) plus a seen request for traffic-only records.
- `/Applications/Claude.app/Contents/MacOS/Claude` must not classify as Claude Code. The harness binary is lowercase `claude`.
- Harnesses make housekeeping calls to the same host as model calls. For a harness with a log, a pre-stream call is shown only while its log says a response is due (`SessionLogWatcher.isAwaiting`).

### Session log formats (verified against real files on this Mac)

- **Claude Code**, `~/.claude/projects/<project>/<session>.jsonl`. One line per content block; the timestamp is when the block **finished**. All lines of a message carry the final `usage` and a non-null `stop_reason`, so the last line cannot be recognised on arrival; the parser finalises a message when the next message id, a `system` line, or 3 s of silence arrives. A thinking block has `thinkingDurationMs`, so `timestamp - thinkingDurationMs` is when generation began. A message that opens with text or a tool call has no first-token time in the log. Tool results can interleave between blocks of the same message.
- **Codex**, `~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`. A response ends with `token_usage_record` (`payload.usage`). `event_msg/item_completed` for `Reasoning` and `AgentMessage` items carry `started_at_ms`; the earliest is the first token. The request time is the preceding user message or `*_output` item. `event_msg/token_count` repeats and is only a fallback.
- **OMP / Pi**, `~/.omp/agent/sessions/<project>/<session>.jsonl`. The assistant entry carries `ttft` and `duration` (ms), `usage.output`, `model`, `provider`, and `message.timestamp` (epoch ms, the request time). Nothing is inferred.
- Session lines can be megabytes (pasted files, tool output). `SessionLogWatcher` reads in chunks inside `autoreleasepool` and replaces any line over 1 MB with an outline of its type and timestamp. Without this, first launch used 750 MB; with it, 25 MB.

### Privacy rules in the code

- `ProcessInspector.arguments` reads argv and stops; the environment after it can hold API keys and is never read.
- `HostCatalog.baseURLHosts` reads only lines that set a base URL.
- History stores timings and token counts, never prompts, replies or credentials.

## Display rules (`MetricsStore`, `StatusItemController`)

- `displayRate` is the one number in the menu bar. It moves only while text flows, holds through waiting, thinking and tool pauses, and settles on the logged figure when a record arrives.
- A new stretch of text starts from the carried value and blends to its own measurement over `settle` (6 s). Before this, each call dipped (118 → 21 → 82 → 118 t/s). After: 137 → 135 → 129 → 124 → 143.
- The menu bar title changes at most every 0.5 s. The bolt is filled while a call is in flight; only the number and that fill change, so the item does not jitter.
- `target` (UserDefaults key `targetHarness`, nil = Auto) filters `active`, `recent`, `primary` and the period stats. `modelStats` and `generatingHarnesses` ignore it on purpose. Every harness is still written to history. `AutoDetector.setTarget` stops sampling non-target harnesses only when their log already records their calls.

## Phase 1 visual refresh (implemented 2026-10-07)

The owner requested three phases: appearance refresh, provider-by-harness dashboard, and Windows 11 port. All three are implemented in source. Windows packages cross-build; native Windows runtime acceptance remains outstanding.

- Changes are in `Theme.swift`, `PopoverView.swift` and `SetupTab.swift`. The detector, measurement logic, history schema and menu bar controller were not changed.
- The popover is now **404 pt wide**, with **16 pt outer padding** and **14 pt card corners** (`Layout`). It retains the Live, Models and Settings tabs.
- `Theme` centralizes the green activity accent, panel fills, borders, dividers and muted text. Surfaces use system-adaptive `Color.primary` opacity and text uses semantic foreground colors. Both light and dark appearance remain supported.
- The header has an app mark, name, subtitle and activity pill. The Live target sits in a “Watching” panel; Auto and individual-harness selection retain their existing semantics. (The chip row was replaced by a click-to-open list on 2026-10-08; see below.)
- Live cards separate phase, harness, current model, speed/TTFT, sparkline and token details. Models and Settings use the shared surface hierarchy; optional proxy instructions remain in a disclosure group.
- Activity, target and phase pulses honor `accessibilityReduceMotion`. Numeric metric animations also honor it. Target scrolling still has its existing animation.
- Target chips no longer suppress the native focus effect. Harness dots are decorative in accessibility, harness badges combine their children, and the sparkline has an accessibility label/value. These changes do not establish complete keyboard or VoiceOver coverage.
- The skill search supported minimal/Swiss hierarchy, compact telemetry layouts and readable metric typography. Its generated landing-page pattern did not fit this utility, and SwiftUI searches returned no verified match after a retry. Native SF typography and project constraints were retained; no generated design-system files, fonts or dependencies were added.

**Verification completed:** `swift build`, `Scripts/build_app.sh`, `codesign --verify --deep --strict`, and all **35 Core tests** passed. Offscreen demo snapshots were rendered and inspected for Live in light/dark, Models in dark, Settings in dark, idle in dark, empty in light, and a pinned Codex target in dark. Snapshot menu titles included live `69 t/s`, idle `68.4 t/s` and pinned Codex `142 t/s`.

## Phase 2 dashboard (implemented 2026-10-08)

- `Dashboard.swift` adds endpoint-first provider identity, source-confidence/evidence labels, complete-history queries beyond the 1,000-record live cache, provider × harness groups, R7 p50/p95 summaries, sparse UTC trends, histograms and source coverage. It does not mutate `RequestRecord` or rewrite old history.
- `DashboardStore` loads history off the main thread, preserves malformed-line counts, handles refresh generations, paginates 25 calls and merges accepted new records while the window is open. Filters never change the Live target.
- `DashboardView` and `DashboardCharts` add Overview, Trends and Calls sections, provider/harness drill-down, model filters, distributions, call inspection and source/estimate/status explanations. Whole-request speed remains separate from generation-window speed.
- `DashboardWindowController` owns one resizable regular window, temporarily switches the accessory app to regular activation while open, then returns it to accessory mode on close. The popover, app menu and ⌘D open it.

**Verification completed:** all **58 Core tests** passed; release build and strict ad-hoc signature verification passed. Rendered dashboard Overview, Trends and Calls views in light/dark, filtered-empty and popover surfaces were inspected. A production-code smoke scenario exercised 1,205 records, drill-down, filters, pagination, call selection, independent live activity, live completion, close/reopen and empty state. Accessibility exercised the actual app process, including harness filtering, section switching, pagination and call inspection. OS screen capture was unavailable; full VoiceOver, reduced motion and real provider credentials remain unverified.

Always stop the running app before launching a rebuilt bundle; the owner explicitly forbids two concurrent Speed Tracker instances. Use `SPEEDTRACKER_HOME` to keep verification history separate. Offscreen snapshots are separate short-lived invocations and must run while the normal app is stopped.

## Phase 3 and added harnesses (2026-10-08)

- Added read-only `OpenCodeLogWatcher` for current `session_message`, legacy `message`/`part` SQLite and entity JSON. Bounded indexed backfill avoids copying the user's multi-gigabyte database. Current v2 assistant creation is first-content time, not dispatch time; TTFT stays unknown, but generation-only TPS can still be measured from supported part timestamps.
- Added `DeepSeekLogWatcher` / `DeepSeekLogParser` for official `dsh`: highest v0–v4 generation, plain or concatenated Zstandard frames, historical seed cuts, retry/attempt closure, packed stream timing and strict usage admission. Zstandard 1.5.7 is linked from the official Swift package; users do not need Homebrew/zstd. SQLite uses the system C library.
- `SessionLogWatcher.hasLogs` / `detectedHarnesses` include supplemental sources. `AutoDetector` uses those APIs for source coverage and target sampling. `HarnessCatalog` recognizes official `dsh`; harness remains `DeepSeek CLI` for history continuity.
- Verification at that point: **96 Swift tests**, **169 Windows smoke checks**, and **108 harness-parity checks** passed (now 110, 177 and 146; see the Gemini CLI and Antigravity section). Real read-only 45-day log scan yielded **2,797 OpenCode** and **2,885 DeepSeek CLI** unique records; neither source falsely reported awaiting activity. Windows x64 and ARM64 self-contained ZIPs cross-published. Optional Windows proxy was exercised end to end against a local streamed provider fixture. macOS release build and signature checks passed.
- Windows native WPF/tray, elevated IP Helper counters, UAC approval, mixed-DPI placement and WSL behavior remain unverified on a Windows machine. Artifacts are unsigned. Do not describe cross-compilation as a Windows runtime test.

### Handoff: next session starts here

The last completed session rebuilt the macOS app and relaunched exactly **one** instance with the dashboard open. Recheck runtime state before launching anything; do not assume that process is still running. Temporary smoke source files were removed. Production fixtures and regression scenarios remain in the test/smoke projects.

**Deliverables:** `Speed Tracker.app`, `dist/SpeedTracker-win-x64.zip` and `dist/SpeedTracker-win-arm64.zip`. The Windows archives were approximately 112/113 MiB at handoff. Extract the whole archive and run `SpeedTracker.exe`; the `collector` subdirectory is required only for optional enhanced networking. Packages include their runtimes and are unsigned.

**Windows decision approved by the owner:** standard-user, zero-setup log collection by default; explicitly enabled elevated network collection is optional. Never elevate the dashboard or silently require a proxy/base URL. The separate collector uses a restricted user/Administrators named-pipe ACL and verifies client/server process IDs, including credential-based UAC. Windows `Program.cs` uses a per-user mutex plus reopen event instead of allowing duplicate main app instances.

**Windows build and smoke commands:**

```bash
# This Mac has .NET8 installed here, although dotnet is not on PATH.
/usr/local/share/dotnet/dotnet run --project Windows/SpeedTracker.Smoke/SpeedTracker.Smoke.csproj -c Release
./Scripts/build_windows.sh win-x64
./Scripts/build_windows.sh win-arm64
```

```powershell
# Windows with the .NET8 SDK; target users need no SDK.
.\Scripts\build_windows.ps1 -Runtime win-x64
dotnet run --project Windows/SpeedTracker.Smoke/SpeedTracker.Smoke.csproj -c Release
```

**Architecture boundary:** Windows is a C# implementation of the same history/measurement contract, not a binary reuse of Swift Core. Keep parser and analytics semantics aligned across languages. `HarnessRegression.cs` compares realistic format edge cases; `FlowRegression.cs` covers traffic false positives, upload grace, connection reuse, target persistence and history-write recovery; `ProxyRegression.cs` exercises real local HTTP forwarding and stream measurement.

**Regressions already fixed—do not reintroduce:**

- Generation duration/TPS can be known without a request-dispatch timestamp; TTFT must remain nil in that case.
- OpenCode finished rows with delayed usage remain refreshable but not active. Tool execution and superseded incomplete rows do not count as model generation. Pending SQL and JSON work must rotate rather than starve behind old rows.
- DeepSeek v0/v1 seed lengths differ from v2+ inherited markers. Unresolved seed boundaries and failed/corrupt tails cannot become active. Retry/attempt/step-end events clear the old request timing. Prefer actual token deltas over block-start fallback.
- Windows network-only activity requires a proven sustained receive window; arbitrary uploads, sparse pings and upload-time control bytes are not model calls. Reused TCP connections must separate requests. Network-only harnesses must be selectable in Live.
- Consumed completed records remain queued until history persistence succeeds; a temporary lock/write error must not lose the parsed batch.

**Priority next work: Windows runtime acceptance.** No Windows host, VM or configured remote was available. On an actual Windows 11 machine, run the smoke suite, then verify tray single/double click, dashboard/filter/call inspection, close-to-tray and second-launch reopen, mixed-DPI flyout placement, light/dark/high contrast, standard-user log paths, UAC approve/cancel/disable and collector shutdown. Compare IPv4/IPv6 byte counters with real traffic and confirm that no idle harness activates. Exercise the optional proxy with a local mock before using credentials. Cross-build success is not proof of any of these OS behaviors.

**Local harness evidence:** OpenCode data exists at `~/.local/share/opencode/opencode.db` (about 2.2 GB); DeepSeek sessions at `~/.dsh/sessions` include `session.v3.jsonl.zstd`. The reported 2,797/2,885 call counts came from an explicit **45-day read-only smoke window**, not normal seven-day startup backfill. Normal diagnostics observed zero recent calls for these sources at that time; absence of recent telemetry is not a failed historical parser. Never dump prompt/reply/stream text or open credential files when investigating.



## Gemini CLI and Antigravity (added 2026-10-08)

Both are log-covered harnesses on macOS and in the Windows core. Swift: `GeminiCLILogParser` in `SessionLogs.swift`, `AntigravityLogs.swift`. C#: `GeminiCliLogParser` in `SessionParsers.cs`, `AntigravityLogs.cs`. Keep the two languages in step; `HarnessRegression.cs` mirrors `GeminiAntigravityTests.swift`.

**Gemini CLI**, `~/.gemini/tmp/<project>/chats/session-*.jsonl` (subagents one folder deeper). Facts read from the CLI's own `ChatRecordingService` (bundle of 0.46):

- Line 1 is session metadata. `{"$set": …}` lines update it and can carry a `messages` array. `{"$rewindTo": id}` drops messages.
- Every other line is a message, and **a message is appended again, whole, each time it changes**. The same `id` appears several times; emit once.
- A `gemini` message is created by `recordMessage` right after the stream loop ends, so its `timestamp` is the end of the response, including for tool-only replies. `tokens` are queued during the stream and attached then; they can be null on a first write.
- `thoughts[].timestamp` is the arrival time of each thought summary. The first is the first token. No thoughts means no first-token time in the log.
- `tokens.output` excludes `tokens.thoughts`. Output here is their sum.
- Requests sent after tool calls are not logged as `user` lines. `toolCalls[].timestamp` is written when a tool completes, and the next request leaves then.
- A reply with no fresh request before it gets no start time (`requestUsed`), never the previous one.
- Legacy whole-file `.json` sessions from older CLI versions are not read.

**Antigravity**, `~/.gemini/antigravity/conversations/<conversation id>.db`, used by the editor's `language_server` and the `agy` terminal agent. Field numbers were read from the protobuf descriptors embedded in `/Applications/Antigravity.app/Contents/Resources/bin/language_server` (2.21.0) and checked against a real conversation:

- `gen_metadata(idx, data)`: one `CortexStepGeneratorMetadata` per model call. `chat_model` = 1, `step_indices` = 2 (packed), `execution_id` = 4. The execution id is per turn, not per call, so the record key is conversation + execution + idx.
- `ChatModelMetadata`: `usage` = 4, `time_to_first_token` = 11, `streaming_duration` = 12 (both `Duration`), `response_model` = 19. Fields 1 and 2 are the system prompt and messages: never decode them.
- `ModelUsageStats`: input = 2, output = 3 (includes thinking), cache_write = 4, cache_read = 5, thinking_output = 9, response_output = 10. Input excludes cache reads.
- `steps(idx, step_type, metadata)`: `CortexStepMetadata` created_at = 1, viewable_at = 6, completed_at = 8. Step type 15 is the model's response. In real data `viewable_at - created_at` equals `time_to_first_token` and `completed_at - viewable_at` equals `streaming_duration`.
- The newest row keeps the whole prompt (megabytes) until the next call replaces it.
- **Speed rule** (`AntigravityGeneration.record`, passed through `LogRecord.speed`): thinking is hidden and over before the first token. Streaming of one second or more: visible tokens ÷ streaming time. Shorter: all output ÷ whole call. Without this, real calls read 700 to 2,500 tok/s because a reply of ~70 tokens lands in ~0.06 s.
- **The files are WAL-mode SQLite.** A plain read-only open creates `-shm`/`-wal` companions that a read-only connection cannot remove. So: with no `-wal` present, open `file:…?immutable=1`; with one present, open normally. A probe during development left two empty companions in the owner's folder; they were removed after confirming nothing had the file open. Do not reintroduce a plain open.
- In-flight detection uses the newest type-15 step lacking `completed_at`. Whether Antigravity writes that step before the reply finishes has not been observed.

`HarnessCatalog` classifies `agy`, and `language_server*` / `agentapi` under a path containing `antigravity`, as Antigravity. Other processes inside `Antigravity.app` are host-app internals and are ignored. Google hosts (`cloudcode-pa`, `daily-cloudcode-pa`, `aiplatform`, `generativelanguage`) are shared addresses, trusted only for known harnesses.

Verification: **110 Swift tests**, **177 Windows smoke checks** and **146 harness-parity checks** pass. On this Mac the app read all 20 calls of a real Antigravity conversation. **Not verified:** a real Gemini CLI model reply (local sessions contain none; fixtures follow the CLI source), either harness live during a call, Antigravity's Windows data path (assumed the same under `%USERPROFILE%`), and the immutable-URI open on a Windows path.

## Target list and linked dashboard filters (2026-10-08)

Two owner requests, both implemented:

- **The Live target is a click-to-open list, not a row of chips.** `TargetHeader` in `PopoverView.swift` is one “Watching” row naming the current choice; clicking it opens `TargetList` (Auto first, each harness with its state), and choosing or clicking elsewhere closes it. Do not go back to a horizontal row. `--snapshot … --target-list` renders it open.
- **The list is an overlay and must not resize the popover.** The first version grew the popover inline. Open, it needed 859 pt on a display with 859 pt available, so AppKit could no longer place it under the status item and moved it to the screen edge (measured: window x 706 → 0), where it stayed. The overlay is sized to the Live tab, so the window does not change. Ordinary resizes that fit, such as switching tabs, keep their anchor on their own; re-calling `popover.show` on every size change was tried as a fix and **broke** them (x → 0), so do not add it. Keep anything that opens inside the popover from making it taller than a laptop screen.
- `--probe-popover` opens the popover, opens and closes the list, switches tabs, and logs the window frame at each step to stderr. Use it for layout jumps that cannot be screenshotted. Windows already used a ComboBox.
- **Dashboard filters narrow one another.** `DashboardReport.harnessOptions`, `providerOptions` and `modelOptions` (C#: `HarnessOptions`, `ProviderOptions`, `ModelOptions`) list what each filter can still be set to given the other two, within the date range. A dimension's own selection does not narrow its own list. The pickers use these; `harnesses`, `providers` and `models` remain the whole-range lists and are still what the matrix and tests of the date window use. A selection left stale by a date-range change stays visible as “… · no calls”.

Verification: **111 Swift tests**, **181 Windows smoke checks**, **146 parity checks**. The picker was checked from offscreen renders, closed and open, in light and dark, and with `--probe-popover` in the running app (window frame unchanged by opening the list; anchored through tab switches). It has not been clicked by hand. The dashboard pickers' open menus cannot be rendered offscreen; the option lists are covered by `eachFilterOffersOnlyWhatTheOtherTwoHaveCallsWith` and the Windows smoke checks.

## Harness presence, repository and CI (2026-10-08)

- **Only harnesses on this machine can be chosen.** Owner requirement: at start, the app works out which harnesses are present and offers only those. `HarnessPresence.installed()` (Swift) and `HarnessPresence.Installed()` (C#) look for each harness's command in the usual per-user and package-manager folders plus `PATH`, and for known app folders. An app launched from the Finder has a bare `PATH`, so the folder list matters more than `PATH` does. `AutoDetector` scans in `start()` before anything else, publishes that list at once, and rescans every minute. Present means installed, or keeping session logs, or running. The Live target list and Settings come from those statuses only; a name that survives only in history is no longer listed, except the pinned target, shown as “Not found on this Mac”. Windows previously seeded the list with every known harness name; it now filters the same way, and a harness seen by the network sampler in the last two minutes counts as running. The dashboard's harness filter is still built from recorded calls.
- Both scans are injectable (`findInstalled`) so tests describe a machine instead of reading the real one. Add a new harness to `HarnessPresence.signatures`, `HarnessCatalog.classify` and the C# equivalents together; a test checks the names agree.
- **Repository:** `Jackhamerp24/speed-tracker` on GitHub. `README.md` is English and `README.vi.md` is Vietnamese; keep them in step. Screenshots in `docs/images` are rendered from demo data with `--snapshot`, never from real history.
- **CI:** `.github/workflows/build.yml` tests and builds macOS (universal, `UNIVERSAL=1 ./Scripts/build_app.sh`) and Windows (x64 and ARM64) on every push, and publishes a release for a `v*` tag. CI is the first place the Windows smoke suite ran on real Windows, and it passed there (188 checks, 146 parity checks). The WPF app itself still has not been run by hand. The first CI run failed on macOS because Swift 6.3 on the runner could not type-check one long test expression that the local 6.4 compiler accepts; keep test closures simple. Release `v0.1.0` was published by the workflow. Plain `git push` hangs on this machine waiting for credentials; push with `git -c credential.helper= -c credential.helper='!gh auth git-credential' push`.
- Verification: **117 Swift tests**, **188 Windows smoke checks**, **146 parity checks**. On this Mac the scan found Antigravity, Claude Code, Codex, Gemini CLI, OMP and opencode.

## Data

`~/Library/Application Support/SpeedTracker/`

- `history.jsonl`: one `RequestRecord` per line. **Keep the schema additive**; the dashboard will read old files. `source` is `log`, `network` or `proxy`. `sourceKey` (for example `claude:<message id>`) prevents duplicates across launches.
- `config.json`: proxy port and extra routes.
- `history.jsonl.bak`: a backup from when 45 false records were removed. The owner may delete it.

`MetricsStore` keeps the newest 1000 records in memory for the popover. The dashboard reads the append-only history directly for its selected range, so its aggregates are not limited to those 1000 records.

## Not verified

Be honest about these when reporting, and verify them if you touch the area.

- The real menu bar item and popover were not manually clicked; dashboard controls were exercised through macOS accessibility. Popover checks remain offscreen renders.
- Live flow tracking was validated on Claude Code. A live Codex call was captured once. OMP has not been exercised live; its past sessions were read correctly.
- No harness without a readable log has been tested. That path produces estimated (`~`) numbers.
- The proxy has not been used with real credentials. The `chatgpt` route is untested. It does not proxy WebSockets.
- Launch at login (`SMAppService`) with an ad-hoc signature has not been tested.

## Known weaknesses

- **OMP speed may read high** for models that reason silently: `ttft` there is time to first content, and reasoning tokens may be counted in a window that starts after them. Unconfirmed. A median of 310 tok/s for `deepseek-flash` is the figure to distrust.
- **TTFT means slightly different things per harness** because each log exposes something different.
- **Claude Code replies that open with text** have no TTFT when read back from old sessions.
- **Live token counts are estimates** from bytes, using a per-harness bytes-per-token ratio learned from finished calls (`UserDefaults` key `bytesPerToken`). Early calls can be well off.
- **Every launch re-reads seven days of logs** (about 6 s of CPU) and relies on `sourceKey` to skip what is stored. The dashboard scans its history file on open and selected-range refresh. Persisting file offsets would avoid the log backfill cost.
- **Sampling costs a process launch per sample.** A cheaper source of per-connection counters would be a real improvement, if one exists without a private entitlement.

## Likely next work

- **Phase 2 follow-ups:** add richer model-level comparison or export only if requested. Provider attribution should remain endpoint/evidence based; do not turn gateway labels into vendor claims. The dashboard currently uses the existing append-only JSONL file rather than a database.
- **Phase 3 Windows port (implemented 2026-10-08):** `Windows/SpeedTracker.Core` is the UI-free .NET8 telemetry/history/analytics service; `Windows/SpeedTracker.Windows` is the tray/flyout/WPF dashboard shell; `Windows/SpeedTracker.Collector` is an optional elevated TCP-counter helper; `Windows/SpeedTracker.Proxy` is an optional loopback reverse proxy. Standard-user log detection is default; elevation is explicit and isolated. `Scripts/build_windows.ps1` and `Scripts/build_windows.sh` produce self-contained x64/arm64 bundles.
- Windows parity follow-ups should remain conservative: test the native tray, mixed-DPI placement, notification-area policy, UAC ACLs, WSL-mounted logs and screen readers on Windows. Never relax endpoint trust or log/source limitations to make a visual state look active.
- Support for more harness logs: add a `SessionLogParser`, register it in `SessionLogWatcher.defaultSources`, add the process pattern to `HarnessCatalog.classify`, and add a colour in `HarnessStyle`.
- Resolve the OMP reasoning-token question above.

## Conventions

- Comments explain why, not what. Match the existing density.
- New Core behaviour gets a test in `Tests/SpeedTrackerCoreTests`, with fixtures shaped like the real log lines or traffic.
- User-visible estimates carry `~`. Do not present an estimate as a measurement.
- Before telling the owner something works, run it: tests, a snapshot, and a trace of the live app where the change is behavioural.

## Windows app in Rust (shipping from 2026-10-10)

Owner request: make the Windows download as light as possible; the C# release zip was about 112 MiB because it carried two .NET runtimes, WPF and ASP.NET Core. The Windows app is now Rust: one `SpeedTracker.exe`, **2.75 MB, 1.38 MB zipped** (x64 release, measured on this Windows 11 PC). CI tests and packages it; `Scripts/build_windows.ps1` builds it.

**Where this replaces older text in this file.** Everything above that describes the Windows version as C#/.NET/WPF (the Layout block, Phase 3, the handoff, the `dotnet` commands, "smoke checks" and "parity checks") is history. The semantics it describes still hold; the code is the Rust port. The C# projects (`Windows/SpeedTracker.*`) and `Scripts/build_windows.sh` are **still in the tree but are no longer built, tested or shipped**: removing them was blocked by the agent's permission system on 2026-10-10 and is left to the owner (`git rm -r Windows/SpeedTracker.Collector Windows/SpeedTracker.Core Windows/SpeedTracker.Proxy Windows/SpeedTracker.Smoke Windows/SpeedTracker.Windows Scripts/build_windows.sh`). Do not edit them, and do not keep them in step with the Rust code.

### What Live can and cannot show on Windows

The owner ran the first Rust build and reported that it "does not actually track your speed at all". Finished calls were being recorded; the Live view just had nothing to show during a call. What was measured while fixing that (2026-10-10, Claude Code 2.1.293 in the Claude desktop app):

- **Claude Code writes a whole reply to its session log in one go, about half a second after the reply ends.** Every block's line, and any tool results that came back while it streamed, land in a single write. A thinking line stamped 06.540 reached the file at 11.456 with the rest of its message. So the log says "a reply is due" (a prompt or tool result with no reply after it) and, once it is over, exactly what it measured. It never says how fast a reply is arriving. This is also why every line carries the final `usage`.
- **Per-process I/O counters do not include socket traffic.** `GetProcessIoCounters` showed 0 read bytes and about 1 KB "other" for a 5 MB download by `curl`. Only operation counts move. There is no unprivileged per-connection byte counter on Windows: `GetPerTcpConnectionEStats` and ETW need an administrator. Do not try this route again.
- So, **without elevation, speed and TTFT appear when each reply completes** (about a second later), and in between Live shows "reply in progress" with a timer and holds the last speed. **Speed during a reply needs the optional elevated collector.**

What was changed:

- `ClaudeCodeLogParser` finalises a message after **one second** of quiet, not three (the batch write makes that safe; the Swift parser still uses three and has network flow for its live view).
- A Claude Code `user` line is a request only if a reply follows it. `starts_request` rules out `isCompactSummary`, and text opening with `<local-command-` or `[Request interrupted` (also when it sits beside a rejected tool result). `isMeta` lines are ignored entirely. Before this, running `/compact` or pressing Esc left "Waiting" on screen for the ten-minute limit: false activity.
- **A log-covered harness's traffic now feeds Live** when the collector is on. Before, it was thrown away, so even with the collector Claude Code and Codex never showed a live speed. `poll_network` runs the same flow state machine, but for a covered harness it writes no record (`NetworkFlow.covered`) and only decorates the call the log says is due: phase `Streaming · network estimate`, TTFT counted from the log's request time, rate and tokens from bytes, `estimated`. A stream with no call due in the log, or one that began before the request, is shown nowhere.
- **The Claude desktop app is `claude.exe` too.** `harness::is_host_app` tells it from Claude Code by folder (`\WindowsApps\Claude_…`, `\AnthropicClaude\`, `\WindowsApps\OpenAI.…`), and `processes::executables` passes full paths for anything named like a harness. The collector never samples a host app.
- The flyout leads with one number: the speed arriving now if it can be seen, otherwise the last reply's, with TTFT, token count, how long ago, and the five most recent replies. **Keep open** stops it closing when focus moves. The tray tooltip leads with the speed.
- **The tray icon is the number** (`icon_text`, `icon_pixels` in `tray.rs`), the counterpart of the macOS menu bar title: the speed rounded to at most three characters in a 4 x 7 pixel font, drawn at the notification area's real size, blue while a call is in flight and grey otherwise. Before any speed is known it is three bars. `SpeedTracker.exe --icon-preview out.png` draws every state at 16, 24 and 32 px; look at that after changing it. It has not been looked at in the real notification area by an agent.

**Not done, and why:** the collector path is still unverified on a real machine, because approving the Windows administrator prompt is the user's act. Its join with the log is covered by tests with a scripted sampler only. The byte-to-token ratios (22 for Anthropic hosts, 180 otherwise) came from the macOS work and are not learned per harness here.

### Tests

`cd Windows && cargo test` (about 5 s after the build; cargo reads `.cargo/config.toml` from the folder it runs in, so run it there). **114 tests, passing on this PC.** The owner's `test-discipline` skill governs test work here: read `~/.claude/skills/test-discipline/SKILL.md` first.

```
tests/harness_parity.rs      40  parser and file-format edge cases; ported from HarnessRegression.cs
tests/flow.rs                16  network-flow state machine, targets, history-write recovery, log/traffic join
tests/smoke.rs               24  logs -> tracker -> history -> dashboard, discovery, presence, Claude Code request rules
tests/proxy.rs                1  real loopback HTTP through the proxy; from ProxyRegression.cs
tests/stream_measurement.rs   7  the proxy's stream meter per provider format
tests/platform.rs             5  Win32 process queries against the test process itself
src/app/** (unit)            21  number and time formatting, flyout placement, tray icon, collector argument checks
tests/common/                    fixtures: scratch folders, a scripted sampler, a writable SQLite connection
tests/fixtures/                  a DeepSeek session compressed by the reference Zstandard library, and its generator
```

- Expected values are the C# suite's hand-worked ones, or are worked out in a comment beside the assertion. Do not replace one with whatever the code returns.
- The big C# scenario was split into independent tests, each with its own tracker and scratch folder. Every C# check is kept or made stricter. One was replaced: "first poll returns fewer than 5" for the byte budget passed with either the 4 MB per-file cap or the 16 MB shared budget removed, so it is now two tests, one per bound.
- Tests beyond the C# suite, each for something the port did itself instead of using a library: a history line as .NET wrote it; reference-encoder Zstandard frames with checksums; a WAL-mode Antigravity database at rest; the proxy's stream formats; the process queries; a harness that is running but idle.
- `Tracker::list_processes_with` replaces the process scan, as `find_installed` replaces the presence scan. Fixtures pass an empty list so a harness running on the build machine cannot change a result.
- The one-second rule is tested on the test's own clock: the fixture sets the log file's modified time, because the watcher dates growth from it.
- Each area was shown able to fail by injecting defects one at a time into a scratch copy. First round: 24 defects, 23 caught at once; removing the per-file cap survived, which exposed the weak budget check above. Second round (the Live work): 19 defects, 18 caught at once; "a tool result is always a request" survived, which exposed that an interrupted tool call was being treated as a pending reply. Both are fixed and caught now.
- **Not covered by any test:** the tray icon and its menu, the two windows beyond their pure helpers, the named pipe and UAC launch, the TCP-counter sampling, WSL home discovery, and TLS upstreams through the proxy. These need a desktop session, elevation or a network.

CI (`.github/workflows/build.yml`, Windows job): `cargo test --locked`, then a check that at least 114 tests passed and none was ignored (lower that number only when tests are removed on purpose), both release builds, a 5 MB limit on each zip, and a start of the x64 exe with a malformed collector launch (exit code 2).

### Layout and decisions

```
Windows/Cargo.toml         one package: library `speedtracker` + binary `SpeedTracker`
Windows/src/lib.rs         UI-free core, a file-for-file port of SpeedTracker.Core and .Proxy
  time.rs json.rs          100 ns UTC instants; lenient JSON readers
  domain.rs                RequestRecord, ProviderIdentity, dashboard report
  parsers.rs               Claude Code, Codex, OMP/Pi, Gemini CLI, DeepSeek parsers
  logs.rs discovery.rs     session-file discovery and tailing, incremental Zstandard
  opencode.rs antigravity.rs sqlite.rs   database-backed harnesses
  tracker.rs               polling, live state, network-flow state machine
  proxy.rs                 optional proxy and stream measurement
Windows/src/app/           Windows only
  tray.rs                  notification icon, tracker, child windows
  collector.rs enhanced.rs elevated TCP-counter helper and its pipe
  ui/                      Live flyout and Dashboard (egui)
```

- **One exe, three roles**, chosen by arguments: tray app (none, or `--dashboard` / `--live`), a window (`--ui live|dashboard`), the elevated collector (`--collector <pipe> <pid>`).
- **Windows are separate short-lived processes.** The tray process holds no graphics; it starts `--ui …` children and talks to them in JSON lines over stdin/stdout (`app/ipc.rs`). The dashboard child reads `history.jsonl` itself. Idle memory measured about 32 MB for the tray process after backfill; a window adds its own process while open.
- **SQLite is the copy Windows ships** (`winsqlite3.dll`, loaded at run time in `sqlite.rs`); nothing is bundled. It was 3.51 with JSON functions on this PC. Older Windows 10 builds are unchecked.
- **Zstandard is `ruzstd`**, decoded block by block so a growing file can be tailed. Inside a frame that has not ended the decoder holds back its window, so text there appears late. Concatenated frames, the documented dsh layout, are unaffected.
- **The proxy** is a small hand-written HTTP/1.1 server with `ureq` and Windows TLS upstream. It answers one request per connection (`Connection: close`) and does not decompress, so it asks upstreams for `identity`.
- **UI is egui with the OpenGL backend.** A machine with no OpenGL 2 driver (some VMs and remote sessions, CI runners) cannot open the windows; the tray and collection still run. Text uses Segoe UI from the system.
- History schema, file locations, harness names and every status string match the C# version, so existing `%LOCALAPPDATA%\SpeedTracker\history.jsonl` files read unchanged.
- The exe has no icon or version resource yet; Explorer shows the generic one.

### Building

`.\Scripts\build_windows.ps1 -Runtime win-x64|win-arm64` writes `dist/SpeedTracker-<runtime>/SpeedTracker.exe` and the zip beside it. With the Visual Studio C++ tools (CI) it uses the MSVC targets with the C runtime linked in, so users need no redistributable. This PC has no Visual Studio: run `Scripts/setup_windows_gnu.ps1` once. It installs Rust's `x86_64-pc-windows-gnullvm` target, supplies the import-library tool and system import libraries that target lacks (under `~/.cargo/gnu-extras`), and writes the uncommitted `Windows/.cargo/config.toml`, which the build script then follows (x64 only). The plain `x86_64-pc-windows-gnu` target links but **every eframe program built with it crashes before `main`**; the cause was not found. Do not go back to it. In Git Bash, `export PATH="$HOME/.cargo/bin:$PATH"` first.

### Verifying without clicking

```bash
# Render a window to a PNG. Standalone windows read SPEEDTRACKER_HOME but have no live state.
SpeedTracker.exe --ui dashboard --tab overview|trends|calls --theme dark|light --screenshot out.png
# The flyout with made-up state, for pictures and layout checks. docs/images/windows-live.png is `--demo waiting`.
SpeedTracker.exe --ui live --demo idle|waiting|streaming --theme dark|light --screenshot out.png
# Have the tray app's own child windows do it, which exercises the tray-to-window link.
SPEEDTRACKER_WINDOW_ARGS="--screenshot|out.png" SpeedTracker.exe --live
# The tray icon's states at the sizes Windows uses, each also enlarged.
SpeedTracker.exe --icon-preview out.png
# Append a line to a file whenever live state changes (timings only). The Windows counterpart of the macOS trace.
SPEEDTRACKER_TRACE=trace.log SpeedTracker.exe
```

If you are running as Claude Code, your own session is the test subject: the trace shows `Claude Code/Waiting` from each tool result until your reply lands, then the held speed changing about a second later. Debug builds keep a console, so a panic is readable; release builds have none. Quit the running app before rebuilding into `dist/`, and never run two instances.

Verified on this PC: the tray process read 1,266 finished calls from real Claude Code, Codex, OMP and DeepSeek CLI logs; a second launch reopened the dashboard through the first; Overview, Trends, Calls and Live rendered correctly from that data in light and dark; the flyout received live state from the tray; `/health` answered on port 4141; and a trace of the release build followed this agent's own Claude Code calls, one record per reply, about a second after each. **Not verified:** clicking any control by hand; flyout placement on mixed-DPI displays; the UAC collector path and TCP counters; proxy forwarding to a real provider over TLS; opencode, Antigravity and Gemini CLI against real data on Windows (fixtures only); high contrast; screen readers; the ARM64 build on ARM hardware.
