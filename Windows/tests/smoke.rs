//! Session logs on disk through the tracker into history, the dashboard report and the held
//! metrics; plus discovery, process classification and harness presence.
//! Ported from Windows/SpeedTracker.Smoke/Program.cs.

mod common;

use common::sqlite::{Db, Param};
use common::{append, frame, iso, lines, ms, sample, FakeSampler, Scratch};
use serde_json::{json, Value};
use speedtracker::discovery::{self, LogSource};
use speedtracker::domain::{DashboardFilter, DashboardReport, FlowSampler, RequestRecord};
use speedtracker::harness;
use speedtracker::history::HistoryStore;
use speedtracker::time::Time;
use speedtracker::tracker::Tracker;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[track_caller]
fn near(actual: Option<f64>, expected: f64, label: &str) {
    common::near(actual, expected, 0.02, label);
}

// ---- discovery, classification, presence -------------------------------------------------------

#[test]
fn default_sources_cover_every_log_backed_harness_and_name_folders_only() {
    let files = Scratch::new("discovery");
    let discovered = discovery::default_sources(Some(&files.path("fakeuser")));
    let has = |harness: &str, tail: &[&str]| discovered.iter().any(|source| source.harness == harness && source.root.ends_with(tail.iter().collect::<PathBuf>()));
    assert!(has("Claude Code", &[".claude", "projects"]), "Claude Code default root discovered");
    assert!(has("DeepSeek CLI", &[".dsh", "sessions"]), "DeepSeek CLI default root discovered");
    assert!(has("opencode", &[".local", "share", "opencode"]), "XDG-like opencode root discovered");
    assert!(has("OMP", &[".omp", "agent", "sessions"]), "OMP root discovered");
    assert!(has("Gemini CLI", &[".gemini", "tmp"]), "Gemini CLI default root discovered");
    assert!(has("Antigravity", &[".gemini", "antigravity", "conversations"]), "Antigravity default root discovered");
    assert!(discovered.iter().all(|source| !source.root.to_string_lossy().contains("opencode.db")), "discovery returns directories only, never database or credential files");
}

#[test]
fn processes_are_classified_by_executable_or_script_never_by_prompt_arguments() {
    let cases: [(&str, Option<&str>, Option<&str>, &str); 12] = [
        ("/opt/homebrew/bin/node", Some("node /opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js -p secret"), Some("Gemini CLI"), "Gemini CLI script classified without prompt arguments"),
        ("/Applications/Antigravity.app/Contents/Resources/bin/language_server", None, Some("Antigravity"), "Antigravity language server classified inside its editor bundle"),
        ("C:/Users/x/AppData/Local/Programs/Antigravity/resources/app/extensions/antigravity/bin/language_server_windows_x64.exe", None, Some("Antigravity"), "Antigravity Windows language server classified"),
        ("/Users/x/.local/bin/agy", None, Some("Antigravity"), "Antigravity terminal agent classified"),
        ("/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper.app/Contents/MacOS/Antigravity Helper", None, None, "Antigravity editor helper not treated as a harness"),
        ("/Applications/Other.app/Contents/Resources/bin/language_server", None, None, "another editor's language server not treated as Antigravity"),
        ("/usr/local/bin/omp", None, Some("OMP"), "native harness executable classified"),
        ("/opt/homebrew/bin/node", Some("node /usr/lib/node_modules/opencode-ai/bin/opencode --prompt secret"), Some("opencode"), "interpreter script path classified without prompt arguments"),
        ("/usr/bin/python3", Some("python3 -c 'print(1)'"), None, "unrelated interpreter not classified"),
        ("C:/Users/x/AppData/Local/dsh/dsh.exe", None, Some("DeepSeek CLI"), "official dsh classified as DeepSeek CLI"),
        ("/Applications/Claude.app/Contents/MacOS/Claude", None, None, "desktop host app not treated as a harness"),
        // A prompt that merely mentions a harness must not name the process.
        ("/usr/bin/node", Some("node /srv/app/server.js --note \"run claude-code later\""), None, "harness names inside later arguments are ignored"),
    ];
    for (executable, command_line, expected, label) in cases {
        assert_eq!(harness::classify(executable, command_line), expected, "{label}");
    }
}

#[test]
fn installed_harnesses_are_found_by_command_launcher_script_version_folder_and_app_folder() {
    let files = Scratch::new("presence");
    let machine = files.path("machine");
    std::fs::create_dir_all(&machine).unwrap();
    assert!(harness::installed(&[machine.clone()], Some("")).is_empty(), "an empty machine has no installed harnesses");
    for command in [".local/bin/claude", "AppData/Roaming/npm/gemini.cmd", ".bun/bin/omp.exe", ".nvm/versions/node/v22.1.0/bin/codex"] {
        files.put(&format!("machine/{command}"), "");
    }
    std::fs::create_dir_all(files.path("machine/AppData/Local/Programs/Antigravity")).unwrap();
    let expected: BTreeSet<String> = ["Antigravity", "Claude Code", "Codex", "Gemini CLI", "OMP"].iter().map(|name| name.to_string()).collect();
    assert_eq!(harness::installed(&[machine], Some("")), expected);
}

#[test]
fn the_search_path_is_honoured_for_harnesses_installed_outside_the_home_folder() {
    let files = Scratch::new("presence-path");
    files.put("tools/dsh", "");
    let found = harness::installed(&[files.path("nobody")], Some(&files.path("tools").to_string_lossy()));
    assert_eq!(found.into_iter().collect::<Vec<_>>(), ["DeepSeek CLI"]);
}

#[test]
fn every_harness_the_scan_can_report_is_named_the_same_by_the_process_classifier() {
    for (name, command) in harness::commands() {
        assert_eq!(harness::classify(&format!("/usr/local/bin/{command}"), None), Some(name), "command {command}");
    }
}

// ---- a tracker over real session files ---------------------------------------------------------

/// A machine with Pi installed and nothing else, whatever the real one has, and one folder of
/// session logs per harness.
struct World {
    files: Scratch,
    now: Time,
    history: Arc<HistoryStore>,
    tracker: Arc<Tracker>,
    changes: Arc<AtomicUsize>,
}

fn only_pi() -> Box<dyn Fn() -> BTreeSet<String> + Send + Sync> {
    Box::new(|| BTreeSet::from(["Pi".to_string()]))
}

fn sources(logs: &Path) -> Vec<LogSource> {
    vec![
        LogSource::new("Claude Code", logs.join("claude").join("projects")),
        LogSource::new("Codex", logs.join("codex").join("sessions")),
        LogSource::new("OMP", logs.join("omp").join("agent").join("sessions")),
        LogSource::new("opencode", logs.join("opencode")),
        LogSource::new("DeepSeek CLI", logs.join("dsh").join("sessions")),
    ]
}

impl World {
    fn new(label: &str) -> World {
        let files = Scratch::new(label);
        let history = Arc::new(HistoryStore::new(Some(&files.path("home"))));
        let tracker = Tracker::new(Arc::clone(&history), None, Some(sources(&files.path("logs"))), Some(files.path("fakeuser")), Some(only_pi())).expect("tracker starts");
        tracker.list_processes_with(Vec::new);
        let changes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        tracker.on_changed(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        World { files, now: Time::now(), history, tracker, changes }
    }
    fn log(&self, relative: &str) -> PathBuf {
        self.files.path(&format!("logs/{relative}"))
    }
    #[track_caller]
    fn poll(&self, seconds: f64) {
        self.tracker.poll(Some(self.now.add_seconds(seconds))).expect("poll succeeds");
    }
    fn records(&self) -> Vec<Arc<RequestRecord>> {
        self.history.read(None).expect("history reads")
    }
    #[track_caller]
    fn record(&self, key: &str) -> Arc<RequestRecord> {
        let matching: Vec<_> = self.records().into_iter().filter(|record| record.source_key.as_deref() == Some(key)).collect();
        assert_eq!(matching.len(), 1, "exactly one record with key {key}");
        matching.into_iter().next().unwrap()
    }
    fn phases(&self, harness: &str) -> Vec<String> {
        self.tracker.active().into_iter().filter(|call| call.harness == harness).map(|call| call.phase).collect()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.tracker.dispose();
    }
}

// A Claude Code turn: a thinking block that took two seconds, then text, then a system line.
fn write_claude(world: &World) {
    let start = world.now.add_seconds(-120.0);
    append(
        &world.log("claude/projects/p1/session.jsonl"),
        lines(&[
            json!({"type": "user", "timestamp": iso(start), "message": {"role": "user"}}),
            json!({"type": "assistant", "timestamp": iso(start.add_seconds(5.0)), "thinkingDurationMs": 2000, "message": {"id": "msg_claude_1", "model": "claude-sonnet-4", "usage": {"output_tokens": 3, "input_tokens": 900, "cache_read_input_tokens": 0}, "content": [{"type": "thinking"}]}}),
            json!({"type": "assistant", "timestamp": iso(start.add_seconds(6.0)), "message": {"id": "msg_claude_1", "model": "claude-sonnet-4", "usage": {"output_tokens": 300, "input_tokens": 900, "cache_read_input_tokens": 100}, "content": [{"type": "text"}]}}),
            json!({"type": "system", "timestamp": iso(start.add_seconds(6.0))}),
        ]),
    );
}

fn codex_file(world: &World) -> PathBuf {
    world.log("codex/sessions/2026/10/08/rollout-x.jsonl")
}

fn codex_request(world: &World, start: Time) {
    append(
        &codex_file(world),
        lines(&[
            json!({"type": "session_meta", "timestamp": iso(start), "payload": {"session_id": "codex_sess", "model_provider": "openai"}}),
            json!({"type": "turn_context", "timestamp": iso(start), "payload": {"model": "gpt-5-codex"}}),
            json!({"type": "response_item", "timestamp": iso(start), "payload": {"type": "message", "role": "user"}}),
        ]),
    );
}

fn codex_item(world: &World, start: Time, after_ms: f64, kind: &str) {
    let at = start.add_ms(after_ms);
    append(&codex_file(world), lines(&[json!({"type": "event_msg", "timestamp": iso(at), "payload": {"type": "item_completed", "started_at_ms": ms(at), "item": {"type": kind}}})]));
}

fn codex_usage(world: &World, start: Time) {
    append(
        &codex_file(world),
        lines(&[json!({"type": "token_usage_record", "timestamp": iso(start.add_seconds(2.0)), "payload": {"response_id": "resp_1", "usage": {"input_tokens": 1000, "cached_input_tokens": 800, "output_tokens": 120, "reasoning_output_tokens": 40}}})]),
    );
}

fn write_omp(world: &World) {
    let start = world.now.add_seconds(-80.0);
    append(
        &world.log("omp/agent/sessions/x.jsonl"),
        lines(&[
            json!({"type": "model_change", "model": "anthropic/claude-opus-4"}),
            json!({"type": "message", "timestamp": iso(start.add_seconds(2.0)), "message": {"role": "assistant", "model": "claude-opus-4", "provider": "anthropic", "timestamp": iso(start), "duration": 2000, "ttft": 250, "stopReason": "stop", "responseId": "omp_1", "usage": {"output": 400, "input": 50, "cacheRead": 10, "reasoningTokens": 5}}}),
        ]),
    );
}

fn deepseek_start(seq: i64, time: Time) -> Value {
    json!({"type": "step/start", "seq": seq, "time": ms(time), "data": {"turn": 1, "step": 1}})
}

// A finished DeepSeek response: reasoning began 500 ms in, the stream finished at two seconds.
fn deepseek_message(id: &str, start: Time, output: i64, with_stream: bool) -> Value {
    let mut data = json!({
        "turn": 1, "step": 1,
        "message": {"id": id, "role": "assistant", "source": {"kind": "model", "provider": "deepseek", "model": "deepseek-v4"}},
        "usage": {"inputTokens": 100, "outputTokens": output, "cacheReadTokens": 20, "reasoningTokens": 10}
    });
    if with_stream {
        data["stream"] = json!([
            {"type": "chunk", "time": ms(start.add_ms(500.0)), "chunk": {"type": "block-start", "index": 0, "blockType": "reasoning"}},
            {"type": "chunk", "time": ms(start.add_seconds(2.0)), "chunk": {"type": "finish", "reason": {"kind": "stop"}}}
        ]);
    }
    json!({"type": "assistant/message", "seq": 4, "time": ms(start.add_seconds(2.0)), "data": data})
}

fn deepseek_header(version: i64, id: &str, start: Time) -> Value {
    json!({"type": "session", "version": version, "id": id, "createdAt": ms(start), "cwd": "/tmp", "isSeeded": false})
}

// The same call three ways: concatenated Zstandard frames, a lower generation beside them that
// must never be counted, and a second project re-publishing it under the same response id.
fn write_deepseek(world: &World) {
    let start = world.now.add_seconds(-60.0);
    let compressed: Vec<u8> = [
        frame(&lines(&[deepseek_header(3, "ds_sess_a", start)])),
        frame(&lines(&[deepseek_start(1, start)])),
        frame(&lines(&[deepseek_message("ds-1", start, 50, true), json!({"type": "step/end", "seq": 5, "time": ms(start.add_ms(2700.0)), "data": {"turn": 1, "step": 1}})])),
    ]
    .concat();
    world.files.put("logs/dsh/sessions/proj/a/session.v3.jsonl.zstd", compressed);
    world.files.put("logs/dsh/sessions/proj/a/session.v2.jsonl", lines(&[deepseek_header(2, "ds_sess_a", start), deepseek_message("ds-1", start, 999, false)]));
    world.files.put("logs/dsh/sessions/proj/b/session.v3.jsonl", lines(&[deepseek_header(3, "ds_sess_b", start), deepseek_start(1, start), deepseek_message("ds-1", start, 50, true)]));
}

// opencode's three stores: the current `session_message` table, the legacy `message`/`part`
// tables, and the older one-file-per-message JSON.
fn write_opencode(world: &World, v2_created: Time, legacy_created: Time, json_created: Time) {
    let db = Db::create(&world.log("opencode/opencode.db"));
    db.run(
        "CREATE TABLE session(id TEXT PRIMARY KEY, time_updated INTEGER);
         CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE INDEX session_message_time ON session_message(time_created, id);
         CREATE INDEX message_session_time ON message(session_id, time_created, id);
         CREATE INDEX part_message_id_id_idx ON part(message_id,id);",
        &[],
    );
    db.run("INSERT INTO session(id, time_updated) VALUES (?1, ?2)", &[Param::Text("ses_1"), Param::Int(ms(v2_created))]);
    let current = |id: &str, seq: i64, created: Time, data: Value| {
        db.run("INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data) VALUES (?1,'ses_1','assistant',?2,?3,?3,?4)", &[Param::Text(id), Param::Int(seq), Param::Int(ms(created)), Param::Text(&data.to_string())]);
    };
    // v2 completed assistant row: nested model/time/tokens and typed content metadata only.
    current(
        "msg_oc1",
        3,
        v2_created,
        json!({
            "role": "assistant", "model": {"id": "gpt-5", "providerID": "openai"},
            "time": {"created": ms(v2_created), "completed": ms(v2_created.add_seconds(3.0))},
            "tokens": {"input": 100, "output": 200, "reasoning": 50, "cache": {"read": 30, "write": 10}},
            "content": [
                {"type": "reasoning", "time": {"created": ms(v2_created), "completed": ms(v2_created.add_ms(500.0))}},
                {"type": "text", "text": "ignored", "time": {"created": ms(v2_created.add_ms(600.0))}},
                {"type": "tool", "time": {"created": ms(v2_created.add_ms(900.0))}}
            ]
        }),
    );
    // v2 unresolved assistant row: live only, must never be recorded.
    let unresolved = v2_created.add_seconds(1.0);
    current("msg_oc2", 4, unresolved, json!({"role": "assistant", "model": {"id": "gpt-5", "providerID": "openai"}, "time": {"created": ms(unresolved)}, "tokens": {"input": 5, "output": 0}, "content": [{"type": "text", "text": "ignored", "time": {"created": ms(unresolved)}}]}));
    // Legacy assistant row; the same id under a different table proves dedup by source key.
    let legacy = json!({"role": "assistant", "modelID": "claude-sonnet-4", "providerID": "anthropic", "time": {"created": ms(legacy_created), "completed": ms(legacy_created.add_seconds(4.0))}, "tokens": {"input": 50, "output": 80, "reasoning": 20, "cache": {"read": 10}}}).to_string();
    for (id, created) in [("msg_oc3", legacy_created), ("msg_oc1", v2_created)] {
        db.run("INSERT INTO message(id, session_id, time_created, time_updated, data) VALUES (?1,'ses_1',?2,?2,?3)", &[Param::Text(id), Param::Int(ms(created)), Param::Text(&legacy)]);
    }
    let part = |id: &str, persisted: Time, data: Value| {
        db.run("INSERT INTO part(id, message_id, session_id, time_created, time_updated, data) VALUES (?1,'msg_oc3','ses_1',?2,?2,?3)", &[Param::Text(id), Param::Int(ms(persisted)), Param::Text(&data.to_string())]);
    };
    part("part_r", legacy_created, json!({"type": "reasoning", "time": {"start": ms(legacy_created.add_ms(100.0)), "end": ms(legacy_created.add_ms(400.0))}}));
    part("part_t", legacy_created, json!({"type": "text", "text": "ignored", "time": {"start": ms(legacy_created.add_ms(500.0)), "end": ms(legacy_created.add_seconds(3.0))}}));
    part("part_f", legacy_created.add_ms(3200.0), json!({"type": "step-finish", "time": {"created": ms(legacy_created.add_ms(3200.0))}}));

    let message = json!({"id": "msg_oc4", "sessionID": "ses_json", "role": "assistant", "modelID": "gpt-5-mini", "providerID": "openai", "time": {"created": ms(json_created), "completed": ms(json_created.add_seconds(2.0))}, "tokens": {"input": 20, "output": 60, "reasoning": 0, "cache": {"read": 5}}});
    world.files.put("logs/opencode/storage/message/ses_json/msg_oc4.json", serde_json::to_string_pretty(&message).unwrap());
    world.files.put("logs/opencode/storage/part/msg_oc4/part_1.json", json!({"type": "text", "text": "ignored", "time": {"start": ms(json_created.add_ms(200.0)), "end": ms(json_created.add_seconds(2.0))}}).to_string());
}

#[test]
fn at_start_before_any_log_is_read_only_installed_harnesses_are_offered() {
    let world = World::new("smoke-start");
    let offered = world.tracker.harnesses();
    assert_eq!(offered.iter().map(|status| status.name.as_str()).collect::<Vec<_>>(), ["Pi"]);
    assert!(offered[0].is_installed);
}

#[test]
fn claude_code_log_yields_one_call_with_ttft_from_its_thinking_block() {
    let world = World::new("smoke-claude");
    write_claude(&world);
    world.poll(0.0);
    let claude = world.record("claude:msg_claude_1");
    assert_eq!((claude.harness.as_str(), claude.model.as_str()), ("Claude Code", "claude-sonnet-4"), "harness and model attributed");
    assert_eq!((claude.source.as_deref(), claude.format.as_str()), (Some("log"), "unknown"), "source marked as log without invented wire format");
    // The thinking block ended at 5 s and took 2 s, so generation began at 3 s and ran to 6 s.
    near(claude.ttft, 3.0, "TTFT includes the thinking block duration");
    near(claude.generation, 3.0, "generation window measured from first token");
    near(claude.tps, 100.0, "throughput derived from the generation window");
    assert_eq!((claude.input_tokens, claude.cached_input_tokens), (Some(1000), Some(100)), "input and cached input tokens aggregated");
}

#[test]
fn codex_live_phase_waits_thinks_streams_then_finalizes_exactly_once() {
    let world = World::new("smoke-codex");
    let start = world.now.add_seconds(-90.0);
    codex_request(&world, start);
    world.poll(1.0);
    assert_eq!(world.phases("Codex"), ["Waiting"], "Codex reported waiting before the first token");
    codex_item(&world, start, 400.0, "reasoning");
    world.poll(2.0);
    assert_eq!(world.phases("Codex"), ["Thinking"], "Codex reported thinking once reasoning started");
    codex_item(&world, start, 900.0, "agentMessage");
    world.poll(3.0);
    assert_eq!(world.phases("Codex"), ["Streaming"], "Codex reported streaming once visible output started");
    codex_usage(&world, start);
    world.poll(4.0);
    let codex = world.record("codex:resp_1");
    near(codex.ttft, 0.4, "Codex TTFT from reasoning item start");
    near(codex.first_visible, 0.9, "Codex first-visible from agent message start");
    // 120 tokens between the first token at 0.4 s and the usage record at 2 s.
    near(codex.tps, 75.0, "Codex throughput over its generation window");
    assert_eq!((codex.reasoning_tokens, codex.cached_input_tokens), (Some(40), Some(800)), "Codex reasoning and cached token detail preserved");
    assert!(world.phases("Codex").is_empty(), "Codex cleared from live once the response completed");
}

#[test]
fn omp_self_reported_timing_is_used_verbatim() {
    let world = World::new("smoke-omp");
    write_omp(&world);
    world.poll(5.0);
    let omp = world.record("omp:omp_1");
    near(omp.ttft, 0.25, "OMP TTFT from log-reported ttft");
    near(omp.tps, 400.0 / 1.75, "OMP throughput excludes its first-token wait");
    assert_eq!(omp.input_tokens, Some(60), "OMP input includes cache read");
}

#[test]
fn deepseek_zstd_frames_generation_selection_and_cross_source_dedup_yield_one_call() {
    let world = World::new("smoke-deepseek");
    write_deepseek(&world);
    world.poll(6.0);
    world.poll(7.0);
    let deep = world.record("deepseek:ds-1");
    near(deep.ttft, 0.5, "DeepSeek TTFT from first generated metadata time");
    near(deep.generation, 1.5, "DeepSeek generation ends at stream finish, not step/end");
    near(deep.tps, 50.0 / 1.5, "DeepSeek throughput from reported usage");
    assert_eq!(deep.output_tokens, 50, "the lower generation's 999 tokens were never counted");
    assert_eq!((deep.input_tokens, deep.cached_input_tokens, deep.reasoning_tokens), (Some(120), Some(20), Some(10)), "DeepSeek input aggregates cache read, reasoning kept");
    assert_eq!((deep.harness.as_str(), deep.model.as_str()), ("DeepSeek CLI", "deepseek-v4"), "DeepSeek harness name stable and model read from evidence");
    assert!(!deep.aborted, "completed DeepSeek turn not marked aborted");
}

#[test]
fn deepseek_live_request_waits_without_a_rate_and_clears_when_the_response_is_logged() {
    let world = World::new("smoke-deepseek-live");
    let start = world.now.add_seconds(-10.0);
    let file = world.log("dsh/sessions/proj/c/session.v3.jsonl");
    append(&file, lines(&[json!({"type": "session", "version": 3, "id": "ds_sess_c", "createdAt": ms(start), "isSeeded": false}), deepseek_start(1, start)]));
    world.poll(8.0);
    world.poll(9.0);
    assert_eq!(world.phases("DeepSeek CLI"), ["Waiting"], "DeepSeek waiting after request start");
    world.poll(11.0);
    let live = world.tracker.active();
    assert!(live.iter().any(|call| call.harness == "DeepSeek CLI" && call.phase == "Waiting" && call.rate.is_none()), "DeepSeek v3 keeps unreported live token timing unavailable");
    append(
        &file,
        lines(&[json!({"type": "assistant/message", "seq": 4, "time": ms(start.add_seconds(2.0)), "data": {"turn": 1, "step": 1, "message": {"id": "ds-2", "role": "assistant", "source": {"kind": "model", "provider": "deepseek", "model": "deepseek-v4"}}, "usage": {"inputTokens": 10, "outputTokens": 20}, "stream": [{"type": "chunk", "time": ms(start.add_seconds(2.0)), "chunk": {"type": "finish", "reason": {"kind": "stop"}}}]}})]),
    );
    world.poll(12.0);
    assert!(world.phases("DeepSeek CLI").is_empty(), "DeepSeek live cleared once the assistant response was logged");
    world.record("deepseek:ds-2");
}

#[test]
fn opencode_reads_current_and_legacy_sqlite_and_legacy_json_and_keeps_unresolved_rows_live_only() {
    let world = World::new("smoke-opencode");
    let v2_created = world.now.add_seconds(-4.0);
    write_opencode(&world, v2_created, world.now.add_seconds(-6.0), world.now.add_seconds(-8.0));
    world.poll(0.0);
    world.poll(1.0);
    world.poll(5.0);
    let (oc1, oc3) = (world.record("opencode:msg_oc1"), world.record("opencode:msg_oc3"));
    world.record("opencode:msg_oc4");
    assert_eq!(oc1.ttft, None, "OpenCode v2 leaves request-relative TTFT unknown");
    near(oc1.generation, 3.0, "OpenCode v2 retains independently timestamped generation");
    near(oc1.tps, 250.0 / 3.0, "OpenCode v2 speed uses measured generation window");
    near(Some(oc1.started_at.since(v2_created)), 0.0, "OpenCode v2 start uses observed metadata time");
    assert_eq!((oc1.output_tokens, oc1.input_tokens, oc1.cached_input_tokens, oc1.reasoning_tokens), (250, Some(140), Some(30), Some(50)), "OpenCode v2 tokens aggregate reasoning and cache");
    assert_eq!((oc1.model.as_str(), oc1.route.as_str()), ("gpt-5", "openai"), "OpenCode v2 model and provider from metadata");
    near(oc3.ttft, 0.1, "OpenCode legacy TTFT from reasoning part start");
    near(oc3.first_visible, 0.5, "OpenCode legacy first-visible from text part start");
    near(oc3.generation, 3.1, "OpenCode legacy generation ends at step finish, before tool waits");
    assert_eq!(oc3.output_tokens, 100, "OpenCode legacy output includes reasoning");
    assert!(!world.records().iter().any(|record| record.source_key.as_deref() == Some("opencode:msg_oc2")), "unresolved OpenCode assistant row is not recorded early");
    assert!(world.tracker.active().iter().any(|call| call.harness == "opencode" && !call.estimated), "OpenCode shows honest live phase while the row is unresolved");
    world.poll(11.0 * 60.0);
    assert!(world.tracker.active().is_empty(), "stale in-flight activity expires instead of showing false activity");
}

// ---- history, the dashboard report and held metrics over everything at once --------------------

// Every harness's finished calls, read into one history: eight records.
fn populated(label: &str) -> World {
    let world = World::new(label);
    write_claude(&world);
    let codex_start = world.now.add_seconds(-90.0);
    codex_request(&world, codex_start);
    codex_item(&world, codex_start, 400.0, "reasoning");
    codex_item(&world, codex_start, 900.0, "agentMessage");
    codex_usage(&world, codex_start);
    write_omp(&world);
    write_deepseek(&world);
    let second = world.now.add_seconds(-10.0);
    world.files.put("logs/dsh/sessions/proj/c/session.v3.jsonl", lines(&[deepseek_header(3, "ds_sess_c", second), deepseek_start(1, second), deepseek_message("ds-2", second, 20, true)]));
    write_opencode(&world, world.now.add_seconds(-4.0), world.now.add_seconds(-6.0), world.now.add_seconds(-8.0));
    for seconds in [0.0, 1.0, 5.0, 6.0] {
        world.poll(seconds);
    }
    world
}

fn keys(records: &[Arc<RequestRecord>]) -> BTreeSet<&str> {
    records.iter().filter_map(|record| record.source_key.as_deref()).collect()
}

const EVERY_CALL: [&str; 8] = ["claude:msg_claude_1", "codex:resp_1", "deepseek:ds-1", "deepseek:ds-2", "omp:omp_1", "opencode:msg_oc1", "opencode:msg_oc3", "opencode:msg_oc4"];

#[test]
fn history_filters_by_harness_and_counts_malformed_lines_without_losing_their_neighbours() {
    let world = populated("smoke-history");
    assert_eq!(keys(&world.records()), BTreeSet::from(EVERY_CALL), "each call recorded exactly once across tables, generations and duplicate sources");
    assert_eq!(world.records().len(), 8);
    append(&world.files.path("home/history.jsonl"), "{\"broken\": true}\nnot json at all\n");
    let opencode = world.history.read(Some(&DashboardFilter { harness: Some("opencode".into()), ..DashboardFilter::default() })).unwrap();
    assert_eq!(opencode.len(), 3, "harness filter returns exactly the OpenCode calls");
    assert_eq!(world.history.skipped_lines(), 2, "malformed history lines are counted, not hidden");
    assert_eq!(world.records().len(), 8, "valid records survive malformed neighbour lines");
    let known = (*world.record("opencode:msg_oc1")).clone();
    assert!(!world.history.append(known).unwrap(), "re-appending a known source key is rejected");
}

#[test]
fn dashboard_report_groups_by_provider_and_harness_with_explicit_provenance() {
    let world = populated("smoke-report");
    let records = world.records();
    let report = DashboardReport::create(&records, &DashboardFilter::default());
    let group = |provider: &str, harness: &str| report.groups.iter().any(|group| group.provider.key == provider && group.harness == harness);
    assert!(report.groups.iter().any(|group| group.provider.name == "Anthropic" && group.harness == "Claude Code"), "provider × harness grouping attributes Anthropic to Claude Code");
    assert!(group("deepseek", "DeepSeek CLI"), "DeepSeek endpoint group present");
    assert!(group("openai", "Codex"), "OpenAI group present for Codex");
    assert!(report.providers.iter().any(|provider| provider.key == "anthropic" && provider.unverified && provider.evidence.contains("Reported provider label")), "label-only attribution stays explicitly unverified");
    assert_eq!(report.summary.count, 8, "summary covers every record");
    assert_eq!(report.records.len(), 8);
    // Claude 300, Codex 120, OMP 400, DeepSeek 50 + 20, opencode 250 + 100 + 60.
    assert_eq!(report.summary.output_tokens, 1300, "real token totals");
    assert!(report.summary.median_tps.is_some() && report.summary.latency_count >= 4 && report.summary.speed_count >= 4, "median speed and latency sample counts populated: {:?}", report.summary);
    assert!(!report.trends.is_empty(), "trend buckets produced");
    assert!(report.records.windows(2).all(|pair| pair[0].started_at >= pair[1].started_at), "report records are newest-first");

    let from = world.now.add_seconds(-30.0);
    let recent = DashboardReport::create(&records, &DashboardFilter { from: Some(from), through: Some(world.now.add_days(1.0)), ..DashboardFilter::default() });
    // Only the second DeepSeek call and the three opencode calls began in the last thirty seconds.
    assert_eq!(keys(&recent.records), BTreeSet::from(["deepseek:ds-2", "opencode:msg_oc1", "opencode:msg_oc3", "opencode:msg_oc4"]), "date filter excludes older calls");
}

#[test]
fn each_dashboard_filter_offers_only_what_the_other_two_have_calls_with() {
    let now = Time::now();
    let facet = |harness: &str, host: &str, model: &str, key: i32| {
        Arc::new(RequestRecord { started_at: now, harness: harness.into(), route: host.into(), upstream_host: host.into(), model: model.into(), output_tokens: 10, source: Some("log".into()), source_key: Some(format!("facet:{key}")), ..RequestRecord::default() })
    };
    let facets = [
        facet("OMP", "api.openai.com", "gpt-6-astra", 1),
        facet("Codex", "api.openai.com", "gpt-6-astra", 2),
        facet("Codex", "api.openai.com", "gpt-6.1-sol", 3),
        facet("OMP", "api.deepseek.com", "deepseek-flash", 4),
        facet("Claude Code", "api.anthropic.com", "claude-opus-5-5", 5),
    ];
    let report = |filter: DashboardFilter| DashboardReport::create(&facets, &filter);
    let providers = |report: &DashboardReport| report.provider_options.iter().map(|provider| provider.key.clone()).collect::<Vec<_>>();

    let by_model = report(DashboardFilter { model: Some("gpt-6-astra".into()), ..DashboardFilter::default() });
    assert_eq!(by_model.harness_options, ["Codex", "OMP"], "choosing a model offers only the harnesses that used it");
    assert_eq!(providers(&by_model), ["openai"], "and only their providers");
    assert_eq!(by_model.model_options.len(), 4, "a dimension's own selection does not narrow its own list");

    let by_harness = report(DashboardFilter { harness: Some("OMP".into()), ..DashboardFilter::default() });
    assert_eq!(by_harness.model_options, ["deepseek-flash", "gpt-6-astra"], "choosing a harness offers only its models");
    assert_eq!(providers(&by_harness).into_iter().collect::<BTreeSet<_>>(), BTreeSet::from(["deepseek".to_string(), "openai".to_string()]), "and its providers");
    assert_eq!(by_harness.harness_options.len(), 3);

    let by_provider = report(DashboardFilter { provider: Some("openai".into()), ..DashboardFilter::default() });
    assert_eq!(by_provider.harness_options, ["Codex", "OMP"], "choosing a provider offers only its harnesses");
    assert_eq!(by_provider.model_options, ["gpt-6-astra", "gpt-6.1-sol"], "and its models");

    let by_two = report(DashboardFilter { harness: Some("OMP".into()), provider: Some("openai".into()), ..DashboardFilter::default() });
    assert_eq!(by_two.model_options, ["gpt-6-astra"], "two filters narrow the third");
    assert_eq!((by_two.harnesses.len(), by_two.models.len()), (3, 4), "while the unfiltered lists keep the whole range");
}

#[test]
fn held_metrics_follow_the_live_target_which_persists_across_restart() {
    let world = populated("smoke-held");
    let tracker = &world.tracker;
    tracker.set_target(Some("Claude Code")).unwrap();
    near(tracker.held_rate(), 100.0, "held rate honors the selected target");
    near(tracker.held_ttft(), 3.0, "held TTFT honors the selected target");
    tracker.set_target(Some("OMP")).unwrap();
    near(tracker.held_rate(), 400.0 / 1.75, "target switch changes the held rate");
    tracker.set_target(Some("Codex")).unwrap();
    near(tracker.held_rate(), 75.0, "codex target rate held");
    tracker.set_target(Some("opencode")).unwrap();
    near(tracker.held_rate(), 250.0 / 3.0, "OpenCode holds the newest independently measured generation rate");
    near(tracker.held_ttft(), 0.1, "OpenCode holds its last valid latency");
    assert_eq!(tracker.held_record().and_then(|record| record.source_key.clone()).as_deref(), Some("opencode:msg_oc1"), "held record exposes the newest accepted target call");
    let recent = tracker.recent(2, Some("opencode"));
    assert_eq!(recent.len(), 2, "recent history is limited to the count asked for");
    assert_eq!(recent[0].source_key.as_deref(), Some("opencode:msg_oc1"), "recent harness-filtered history returns newest first");

    let reloaded = Tracker::new(Arc::new(HistoryStore::new(Some(world.history.home()))), None, Some(sources(&world.files.path("logs"))), Some(world.files.path("fakeuser")), Some(only_pi())).expect("tracker restarts");
    assert_eq!(reloaded.target().as_deref(), Some("opencode"), "live target persisted across restart");
    assert!(!reloaded.enhanced_network_available(), "enhanced network unavailable without a sampler");
    reloaded.set_enhanced_network(true);
    assert!(!reloaded.enhanced_network_enabled(), "enhanced network cannot enable without a sampler");
    assert!(reloaded.network_status().contains("Standard-user log-first"), "network status explains the log-first default: {}", reloaded.network_status());
    assert!(reloaded.set_target(Some("Not A Harness")).is_err(), "unknown target rejected");
}

#[test]
fn harness_list_holds_what_has_logs_or_is_installed_and_nothing_else() {
    let world = populated("smoke-statuses");
    let statuses = world.tracker.harnesses();
    for harness in ["Claude Code", "opencode", "DeepSeek CLI"] {
        assert!(statuses.iter().any(|status| status.name == harness && status.has_logs), "{harness} reported as having readable telemetry");
    }
    let pi = statuses.iter().find(|status| status.name == "Pi").expect("the installed harness is offered");
    assert!(pi.is_installed && !pi.has_logs, "installed harness without logs is offered");
    assert!(pi.limitation.as_deref().is_some_and(|limitation| limitation.contains("Processes alone")), "and honest about the missing source");
    // None of these is installed in this fixture, has logs here, or is a process name on a build machine.
    assert!(!statuses.iter().any(|status| matches!(status.name.as_str(), "Crush" | "Goose" | "Qwen Code")), "harnesses that are not on this machine are not offered: {:?}", statuses.iter().map(|status| &status.name).collect::<Vec<_>>());
    assert!(world.changes.load(Ordering::Relaxed) > 0, "Changed event raised for consumers");
}

// ---- the optional network sampler --------------------------------------------------------------

#[test]
fn network_only_harness_shows_an_estimate_and_each_connection_becomes_its_own_estimated_record() {
    let files = Scratch::new("smoke-network");
    let history = Arc::new(HistoryStore::new(Some(&files.path("home"))));
    let sampler = Arc::new(FakeSampler::default());
    let flows: Arc<dyn FlowSampler> = sampler.clone();
    let tracker = Tracker::new(Arc::clone(&history), Some(flows), Some(Vec::new()), Some(files.path("fakeuser")), Some(only_pi())).expect("tracker starts");
    tracker.list_processes_with(Vec::new);
    let now = Time::now();
    let poll = |seconds: f64| tracker.poll(Some(now.add_seconds(seconds))).expect("poll succeeds");
    let network = || history.read(None).unwrap().into_iter().filter(|record| record.source.as_deref() == Some("network")).collect::<Vec<_>>();
    tracker.set_enhanced_network(true);
    assert!(tracker.enhanced_network_enabled(), "enhanced network enabled when the sampler is available");

    // A request, then 1,200 bytes every half second.
    let call = |connection: &str, from: f64, bytes_per_step: u64| {
        sampler.push(sample(4242, "Aider", "api.anthropic.com", 0, 0, connection));
        poll(from);
        sampler.push(sample(4242, "Aider", "api.anthropic.com", 0, 600, connection));
        poll(from + 0.5);
        for step in 1..=5u32 {
            sampler.push(sample(4242, "Aider", "api.anthropic.com", bytes_per_step * u64::from(step), 600, connection));
            poll(from + 0.5 + f64::from(step) * 0.5);
        }
    };
    call("conn-1", 21.0, 1200);
    assert!(tracker.active().iter().any(|live| live.harness == "Aider" && live.estimated), "network-only harness shows a live estimate, never a false exact metric");
    sampler.remove("conn-1");
    poll(30.0);
    let first = network();
    assert_eq!(first.len(), 1, "network flow finished into one record");
    assert!(first[0].harness == "Aider" && first[0].tokens_estimated, "and it is marked estimated");
    assert!(first[0].tps.is_some_and(|rate| rate > 0.0), "estimated throughput non-zero from real bytes");

    call("conn-2", 40.0, 900);
    sampler.remove("conn-2");
    poll(50.0);
    assert_eq!(network().len(), 2, "a new connection identity is tracked separately instead of merging counters");
    assert!(!tracker.active().iter().any(|live| live.harness == "Aider"), "network live call cleared after the flow ended");
    tracker.dispose();
}

// ---- history written by the .NET version -------------------------------------------------------

#[test]
fn history_line_written_by_the_dotnet_version_reads_unchanged_and_is_kept_by_later_appends() {
    let files = Scratch::new("history-dotnet");
    // As System.Text.Json wrote a RequestRecord: camelCase, nulls omitted, whole numbers without a
    // decimal point, and the instant with seven fraction digits and whatever offset it was built with.
    let line = r#"{"id":"3f2504e0-4f89-11d3-9a0c-0305e82c3301","startedAt":"2026-10-08T19:34:56.1234567+07:00","harness":"Claude Code","route":"anthropic","upstreamHost":"anthropic","format":"unknown","model":"claude-opus-5-5","streamed":true,"status":200,"ttft":1.5,"generation":4,"total":5.5,"inputTokens":1000,"outputTokens":400,"tokensEstimated":false,"tps":100,"aborted":false,"source":"log","sourceKey":"claude:msg_1"}"#;
    files.put("history.jsonl", format!("{line}\n"));
    let history = HistoryStore::new(Some(&files.root));
    let records = history.read(None).unwrap();
    assert_eq!((records.len(), history.skipped_lines()), (1, 0), "the old line is a valid record");
    let old = &records[0];
    assert_eq!(old.id.to_string(), "3f2504e0-4f89-11d3-9a0c-0305e82c3301");
    // 19:34:56.1234567 at +07:00 is 12:34:56.1234567 UTC, 1,791,462,896 s after the epoch; a tick is 100 ns.
    let started = Time(1_791_462_896 * 10_000_000 + 1_234_567);
    assert_eq!(old.started_at, started, "the instant is read to the tick, offset applied");
    assert_eq!((old.harness.as_str(), old.model.as_str(), old.status, old.streamed), ("Claude Code", "claude-opus-5-5", 200, true));
    assert_eq!((old.ttft, old.generation, old.total, old.tps), (Some(1.5), Some(4.0), 5.5, Some(100.0)));
    assert_eq!((old.input_tokens, old.output_tokens), (Some(1000), 400));
    assert_eq!((old.first_visible, old.ttfb, old.cached_input_tokens, old.reasoning_tokens), (None, None, None, None), "fields the old line omitted stay unknown, never zero");

    let newer = RequestRecord { started_at: Time(started.0 + 10_000_000), harness: "Codex".into(), source: Some("log".into()), source_key: Some("codex:resp_9".into()), ..RequestRecord::default() };
    assert!(history.append(newer).unwrap());
    let text = std::fs::read_to_string(files.path("history.jsonl")).unwrap();
    let written: Vec<&str> = text.lines().collect();
    assert_eq!(written.len(), 2, "history is append-only: one line per record");
    assert_eq!(written[0], line, "the existing line is untouched, byte for byte");
    // One second later, written the way .NET reads it back: ISO 8601 with an explicit offset.
    assert!(written[1].contains(r#""startedAt":"2026-10-08T12:34:57.1234567+00:00""#), "{}", written[1]);
    assert!(!written[1].contains("null"), "absent fields are omitted, not written as null: {}", written[1]);
    let reread = HistoryStore::new(Some(&files.root)).read(None).unwrap();
    assert_eq!(reread.iter().map(|record| record.started_at).collect::<Vec<_>>(), [started, Time(started.0 + 10_000_000)], "both lines read back");
}

// ---- a harness that is open but idle -----------------------------------------------------------

#[test]
fn a_running_harness_is_offered_but_a_process_alone_is_never_activity() {
    let world = World::new("smoke-running");
    world.tracker.list_processes_with(|| vec!["claude".to_string(), "notepad".to_string(), "Claude Helper".to_string()]);
    world.poll(0.0);
    let statuses = world.tracker.harnesses();
    assert_eq!(statuses.iter().map(|status| status.name.as_str()).collect::<Vec<_>>(), ["Claude Code", "Pi"], "the running harness joins the installed one; other processes add nothing");
    let claude = &statuses[0];
    assert!(claude.is_running && !claude.has_logs && !claude.is_installed, "it is listed because it is running: {claude:?}");
    assert!(claude.limitation.as_deref().is_some_and(|limitation| limitation.contains("Processes alone")), "and says a process is not telemetry");
    assert!(world.tracker.active().is_empty(), "an open harness is not a model call in progress");
    assert!(world.records().is_empty(), "and records nothing");

    // Processes are rescanned every four seconds; once it has exited it is no longer offered.
    world.tracker.list_processes_with(Vec::new);
    world.poll(5.0);
    assert_eq!(world.tracker.harnesses().iter().map(|status| status.name.clone()).collect::<Vec<_>>(), ["Pi"]);
}

// ---- Claude Code: when a reply is due, and when it is finished ---------------------------------

fn claude_line(time: Time, extra: serde_json::Value, content: serde_json::Value) -> serde_json::Value {
    let mut line = json!({"type": "user", "timestamp": iso(time), "message": {"role": "user", "content": content}});
    for (name, value) in extra.as_object().into_iter().flatten() {
        line[name] = value.clone();
    }
    line
}

#[test]
fn claude_code_shows_a_reply_in_progress_until_it_lands_and_records_it_after_a_second_of_silence() {
    let world = World::new("smoke-claude-live");
    let file = world.log("claude/projects/p1/live.jsonl");
    // The log keeps milliseconds, so the prompt is dated on a whole one.
    let asked = Time::from_unix_ms(ms(world.now.add_seconds(-4.0)));
    append(&file, lines(&[claude_line(asked, json!({}), json!("hello"))]));
    world.poll(0.0);
    let due = world.tracker.active();
    assert_eq!(due.len(), 1, "a prompt means a reply is due");
    assert_eq!((due[0].harness.as_str(), due[0].phase.as_str(), due[0].started_at, due[0].rate), ("Claude Code", "Waiting", asked, None), "timed from the prompt, with no speed invented while it streams");

    // Claude Code writes every line of a reply in one go when the reply is over.
    append(
        &file,
        lines(&[
            json!({"type": "assistant", "timestamp": iso(asked.add_seconds(3.0)), "thinkingDurationMs": 1000, "message": {"id": "msg_live_1", "model": "claude-opus-5-5", "usage": {"output_tokens": 200, "input_tokens": 50}, "content": [{"type": "thinking"}]}}),
            json!({"type": "assistant", "timestamp": iso(asked.add_seconds(4.0)), "message": {"id": "msg_live_1", "model": "claude-opus-5-5", "usage": {"output_tokens": 200, "input_tokens": 50}, "content": [{"type": "text"}]}}),
        ]),
    );
    // The file's own time says when it last grew; put that on the test's clock, at 0.1 s.
    let landed = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms(world.now.add_seconds(0.1)) as u64);
    std::fs::File::options().write(true).open(&file).unwrap().set_modified(landed).unwrap();
    world.poll(0.2);
    assert!(world.tracker.active().is_empty(), "once the reply has landed nothing is due");
    world.poll(0.9);
    assert!(world.records().is_empty(), "under a second of quiet is not yet the end of the message");
    world.poll(1.3);
    let reply = world.record("claude:msg_live_1");
    // Thinking ended at 3 s having taken 1 s, so the first token came at 2 s; the last block ended at 4 s.
    near(reply.ttft, 2.0, "first token dated from the thinking block");
    near(reply.generation, 2.0, "generation runs from first token to last block");
    near(reply.tps, 100.0, "200 tokens over 2 s");
    near(world.tracker.held_rate(), 100.0, "and that speed is what Live holds");
}

#[test]
fn claude_code_lines_that_no_reply_follows_never_show_a_call_in_progress() {
    let cases = [
        ("what a local command printed", json!({}), json!("<local-command-stdout>Compacted </local-command-stdout>")),
        ("a notice that the user interrupted", json!({}), json!([{"type": "text", "text": "[Request interrupted by user]"}])),
        ("a tool stopped by the user", json!({}), json!([{"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": "rejected"}, {"type": "text", "text": "[Request interrupted by user for tool use]"}])),
        ("the summary a compaction leaves behind", json!({"isCompactSummary": true}), json!("This session is being continued from a previous conversation.")),
        ("text Claude Code adds beside a command", json!({"isMeta": true}), json!("<local-command-caveat>Caveat: the messages below were generated by the user.</local-command-caveat>")),
    ];
    for (label, extra, content) in cases {
        let world = World::new("smoke-claude-quiet");
        append(&world.log("claude/projects/p1/quiet.jsonl"), lines(&[claude_line(world.now.add_seconds(-3.0), extra, content)]));
        world.poll(0.0);
        assert!(world.tracker.active().is_empty(), "{label} is not a request");
        assert!(world.tracker.harnesses().iter().any(|status| status.name == "Claude Code" && status.has_logs), "{label}: the session is still recognised as Claude Code's");
    }
}

#[test]
fn claude_code_prompts_and_tool_results_are_requests_and_an_interruption_ends_the_wait() {
    let world = World::new("smoke-claude-requests");
    let file = world.log("claude/projects/p1/requests.jsonl");
    let asked = Time::from_unix_ms(ms(world.now.add_seconds(-9.0)));
    append(&file, lines(&[claude_line(asked, json!({}), json!([{"type": "text", "text": "fix the bug"}]))]));
    world.poll(0.0);
    assert_eq!(world.tracker.active().iter().map(|call| call.started_at).collect::<Vec<_>>(), [asked], "a prompt is a request");

    append(&file, lines(&[claude_line(asked.add_seconds(1.0), json!({"isMeta": true}), json!([{"type": "text", "text": "skill instructions"}]))]));
    world.poll(0.5);
    assert_eq!(world.tracker.active().iter().map(|call| call.started_at).collect::<Vec<_>>(), [asked], "added text neither ends nor restarts the wait");

    append(&file, lines(&[claude_line(asked.add_seconds(2.0), json!({}), json!("[Request interrupted by user]"))]));
    world.poll(1.0);
    assert!(world.tracker.active().is_empty(), "an interrupted request is no longer due");

    let returned = asked.add_seconds(5.0);
    append(&file, lines(&[claude_line(returned, json!({}), json!([{"type": "tool_result", "tool_use_id": "toolu_1", "content": "<local-command-stdout>looks like command output</local-command-stdout>"}]))]));
    world.poll(1.5);
    assert_eq!(world.tracker.active().iter().map(|call| call.started_at).collect::<Vec<_>>(), [returned], "a tool result sends the next request, whatever its text looks like");
}

// ---- a desktop app that shares a harness's file name -------------------------------------------

#[test]
fn desktop_apps_are_told_from_the_harnesses_they_host_by_their_folder() {
    let cases = [
        (r"C:\Program Files\WindowsApps\Claude_2.31226.0.0_x64__pzs8sxrjxfjjc\app\Claude.exe", None, "the Claude desktop app on Windows is claude.exe too, and is not Claude Code"),
        (r"C:\Users\x\AppData\Local\AnthropicClaude\app-1.0.0\claude.exe", None, "nor is its older installer's copy"),
        (r"C:\Users\x\AppData\Roaming\Claude\claude-code\2.1.293\83cb0bd7fed4\claude.exe", Some("Claude Code"), "Claude Code as the desktop app installs it is the harness"),
        (r"C:\Users\x\.local\bin\claude.exe", Some("Claude Code"), "and so is the standalone command"),
        (r"C:\Program Files\WindowsApps\OpenAI.Codex_26.1.0.0_x64__2p2nqsd0c76g0\app\Codex.exe", None, "the Codex desktop app is not the Codex harness"),
        (r"C:\Users\x\AppData\Local\OpenAI\Codex\bin\9691020b546a15b2\codex.exe", Some("Codex"), "the command it runs is"),
    ];
    for (executable, expected, label) in cases {
        assert_eq!(harness::classify(executable, None), expected, "{label}");
        assert_eq!(harness::is_host_app(executable), expected.is_none(), "{label}");
    }

    let world = World::new("smoke-desktop-app");
    world.tracker.list_processes_with(|| vec![r"C:\Program Files\WindowsApps\Claude_2.31226.0.0_x64__pzs8sxrjxfjjc\app\Claude.exe".to_string()]);
    world.poll(0.0);
    assert_eq!(world.tracker.harnesses().iter().map(|status| status.name.clone()).collect::<Vec<_>>(), ["Pi"], "the desktop app running alone does not offer Claude Code");
}
