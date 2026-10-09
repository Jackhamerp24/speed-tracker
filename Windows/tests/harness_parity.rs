//! Harness parity: the same format edge cases the C# and Swift suites pin down, with the same
//! fixtures and the same expected values. Fixtures are shaped like the real log lines and rows.
//! Ported from Windows/SpeedTracker.Smoke/HarnessRegression.cs; mirrors GeminiAntigravityTests.swift.

mod common;

use common::sqlite::{Db, Param};
use common::{append, frame, lines, Scratch};
use serde_json::{json, Map, Value};
use speedtracker::antigravity::{protobuf, AntigravityGeneration, AntigravityLogWatcher};
use speedtracker::discovery::LogSource;
use speedtracker::domain::LiveCall;
use speedtracker::logs::SessionLogWatcher;
use speedtracker::opencode::OpenCodeLogWatcher;
use speedtracker::parsers::{DeepSeekLogParser, GeminiCliLogParser, LogRecord};
use speedtracker::time::Time;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[track_caller]
fn near(actual: Option<f64>, expected: f64, label: &str) {
    common::near(actual, expected, 0.0001, label);
}

#[track_caller]
fn single(mut records: Vec<LogRecord>, label: &str) -> LogRecord {
    assert_eq!(records.len(), 1, "{label}: expected exactly one record");
    records.remove(0)
}

fn keys(records: &[LogRecord]) -> Vec<&str> {
    records.iter().map(|record| record.key.as_str()).collect()
}

/// Fixture time: a whole millisecond one minute ago, so every log stamp is in the recent past.
struct Clock {
    epoch: Time,
}

struct Msg {
    seq: i64,
    at: i64,
    id: &'static str,
    step: i64,
    usage: Option<Value>,
    stream: Vec<Value>,
    surface_op: Option<&'static str>,
}

impl Default for Msg {
    fn default() -> Self {
        Msg { seq: 1, at: 6000, id: "response", step: 1, usage: None, stream: Vec::new(), surface_op: None }
    }
}

impl Clock {
    fn new() -> Clock {
        Clock { epoch: Time::from_unix_ms(Time::now().add_seconds(-60.0).unix_ms()) }
    }
    fn now(&self) -> Time {
        self.epoch.add_seconds(60.0)
    }
    /// Epoch milliseconds, `milliseconds` after the fixture epoch.
    fn at(&self, milliseconds: i64) -> i64 {
        self.epoch.unix_ms() + milliseconds
    }
    fn iso(&self, milliseconds: i64) -> String {
        common::iso(self.epoch.add_ms(milliseconds as f64))
    }
    fn after(&self, milliseconds: i64) -> Time {
        self.epoch.add_ms(milliseconds as f64)
    }
    fn header(&self, version: i64, seeded: bool, seed_length: Option<i64>) -> Value {
        if version >= 2 {
            return json!({"type": "session", "version": version, "id": "session-fixture", "createdAt": self.at(0), "isSeeded": seeded});
        }
        let mut header = json!({"type": "session", "version": version, "id": "session-fixture", "createdAt": self.at(0)});
        if let Some(length) = seed_length {
            header["seedLength"] = json!(length);
        }
        header
    }
    fn v3(&self) -> Value {
        self.header(3, false, None)
    }
    fn event(&self, kind: &str, seq: i64, at: i64, data: Value) -> Value {
        json!({"type": kind, "seq": seq, "time": self.at(at), "data": data})
    }
    fn start(&self, seq: i64, at: i64, step: i64) -> Value {
        self.event("step/start", seq, at, json!({"turn": 1, "step": step}))
    }
    fn first_start(&self) -> Value {
        self.start(0, 0, 1)
    }
    fn chunk(&self, kind: &str, at: i64, mut fields: Value) -> Value {
        fields["type"] = json!(kind);
        json!({"type": "chunk", "time": self.at(at), "chunk": fields})
    }
    fn message(&self, message: Msg) -> Value {
        let usage = message.usage.unwrap_or_else(|| json!({"inputTokens": 100, "outputTokens": 80, "reasoningTokens": 20, "cacheReadTokens": 40, "cacheWriteTokens": 10}));
        let mut row = json!({
            "type": "assistant/message", "seq": message.seq, "time": self.at(message.at),
            "data": {"turn": 1, "step": message.step, "message": {"id": message.id, "role": "assistant", "source": {"kind": "model", "provider": "opencode-go", "model": "deepseek-v4.1-flash"}}, "usage": usage, "stream": message.stream}
        });
        if let Some(surface) = message.surface_op {
            row["surfaceOp"] = json!(surface);
        }
        row
    }
    /// A parser that has read a v3 header and one request start.
    fn requested(&self) -> DeepSeekLogParser {
        let mut parser = DeepSeekLogParser::new(None, None);
        parser.ingest(&self.v3());
        parser.ingest(&self.first_start());
        parser
    }
}

// ---- DeepSeek released stream formats ---------------------------------------------------------

#[test]
fn deepseek_stream_timing_uses_actual_deltas_and_exact_usage() {
    let clock = Clock::new();
    for version in [2, 3, 4] {
        let mut parser = DeepSeekLogParser::new(None, None);
        parser.ingest(&clock.header(version, false, None));
        parser.ingest(&clock.first_start());
        assert!(parser.state.awaiting_response, "v{version} request evidence is awaiting");
        let stream = vec![
            clock.chunk("block-start", 1999, json!({"blockType": "reasoning", "index": 0})),
            json!({"type": "reasoning-chunks", "time0": clock.at(2000), "index": 0, "dt": [10, 10], "texts": ["", "thought", ""]}),
            clock.chunk("block-start", 2999, json!({"blockType": "text", "index": 1})),
            json!({"type": "text-chunks", "time0": clock.at(3000), "index": 1, "dt": [20, -5], "texts": [" ", "", "answer"]}),
            clock.chunk("finish", 5998, json!({"reason": {"kind": "stop"}})),
        ];
        let record = single(parser.ingest(&clock.message(Msg { stream, ..Msg::default() })), "v2+ message");
        near(record.first_token.map(|time| time.since(clock.epoch)), 2.01, "actual delta replaces earlier block-start");
        near(record.first_visible.map(|time| time.since(clock.epoch)), 3.015, "signed packed gaps and whitespace preserve visible timing");
        near(Some(record.end.since(clock.epoch)), 5.998, "generation ends before persisted message and tools");
        assert_eq!((record.output_tokens, record.input_tokens, record.cached_input_tokens, record.reasoning_tokens), (80, Some(150), Some(40), Some(20)), "DeepSeek exact disjoint usage is not double-counted");
        assert!(!parser.state.awaiting_response, "assistant response clears DeepSeek activity before tools");
    }
}

#[test]
fn deepseek_whitespace_and_tool_arguments_are_tokens_but_not_visible_text() {
    let clock = Clock::new();
    let mut parser = clock.requested();
    let stream = vec![clock.chunk("text-delta", 1000, json!({"text": " \n"})), clock.chunk("tool-call-delta", 1200, json!({"argumentsDelta": "{}"}))];
    let record = single(parser.ingest(&clock.message(Msg { stream, ..Msg::default() })), "whitespace message");
    assert_eq!(record.first_token, Some(clock.after(1000)), "whitespace generates a token");
    assert_eq!(record.first_visible, None, "whitespace and tool arguments are not visible answer text");
}

#[test]
fn deepseek_legacy_generations_keep_outer_timestamps_and_stream_usage() {
    let clock = Clock::new();
    for version in [0, 1] {
        let mut parser = DeepSeekLogParser::new(None, None);
        parser.ingest(&clock.header(version, false, None));
        parser.ingest(&clock.first_start());
        parser.ingest(&json!({"type": "reasoning-chunks", "seq0": 1, "time0": clock.at(1000), "data": {"turn": 1, "step": 1, "index": 0, "dt": [20], "texts": ["", "thought"]}}));
        parser.ingest(&clock.event("assistant/chunk", 3, 2000, json!({"turn": 1, "step": 1, "chunk": {"type": "text-delta", "text": "answer"}})));
        parser.ingest(&clock.event("assistant/chunk", 4, 3000, json!({"turn": 1, "step": 1, "chunk": {"type": "usage", "usage": {"outputTokens": 30, "inputTokens": 10}}})));
        parser.ingest(&clock.event("assistant/chunk", 5, 3100, json!({"turn": 1, "step": 1, "chunk": {"type": "finish", "reason": {"kind": "stop"}}})));
        let message = if version == 0 {
            clock.event("assistant/message", 6, 3200, json!({"turn": 1, "step": 1, "provenance": {"model": "legacy-model", "provider": "legacy-provider"}, "content": []}))
        } else {
            clock.event("assistant/message", 6, 3200, json!({"turn": 1, "step": 1, "message": {"role": "assistant", "id": "legacy", "source": {"kind": "model", "model": "legacy-model", "provider": "legacy-provider"}}}))
        };
        let record = single(parser.ingest(&message), "legacy message");
        near(record.first_token.map(|time| time.since(clock.epoch)), 1.02, "legacy seq0 packed rows preserve their actual first delta");
        near(record.first_visible.map(|time| time.since(clock.epoch)), 2.0, "legacy assistant/chunk uses outer timestamp");
        near(Some(record.end.since(clock.epoch)), 3.1, "legacy finish chunk excludes persistence delay");
        assert_eq!(record.output_tokens, 30, "legacy stream usage remains available without message usage (v{version})");
    }
}

// ---- DeepSeek lifecycle and malformed telemetry -----------------------------------------------

#[test]
fn deepseek_retry_and_end_events_discard_the_old_request_timing() {
    let clock = Clock::new();
    for kind in ["assistant/attempt", "llm/retry", "step/end", "turn/end", "session/end-seed"] {
        let mut parser = clock.requested();
        parser.ingest(&clock.event(kind, 1, 1000, json!({"turn": 1, "step": 1})));
        assert!(!parser.state.awaiting_response, "{kind} clears pending activity");
        let stream = vec![clock.chunk("reasoning-delta", 2000, json!({"text": "thought"}))];
        let record = single(parser.ingest(&clock.message(Msg { seq: 2, stream, ..Msg::default() })), kind);
        assert_eq!(record.request_start, None, "{kind} does not reuse obsolete dispatch timing");
        let stored = record.make_record();
        assert_eq!(stored.ttft, None, "{kind}: no dispatch time means no TTFT");
        near(stored.generation, 4.0, "generation remains measurable without request timing");
        near(stored.tps, 20.0, "generation-only rate is retained");
    }
}

#[test]
fn deepseek_malformed_header_cannot_fabricate_a_call() {
    let clock = Clock::new();
    let mut malformed = DeepSeekLogParser::new(None, None);
    malformed.ingest(&json!({"type": "session"}));
    malformed.ingest(&clock.first_start());
    assert!(!malformed.state.supported && !malformed.state.awaiting_response, "a header without version, id and date starts nothing");
    assert!(malformed.state.limitation.is_some(), "the malformed header is reported");
}

#[test]
fn deepseek_file_name_and_header_generations_must_agree() {
    let clock = Clock::new();
    let mut mismatch = DeepSeekLogParser::new(None, Some(3));
    mismatch.ingest(&clock.header(2, false, None));
    assert!(!mismatch.has_supported_header() && mismatch.state.limitation.is_some());
}

#[test]
fn deepseek_step_start_without_a_step_is_not_a_request() {
    let clock = Clock::new();
    let mut parser = DeepSeekLogParser::new(None, None);
    parser.ingest(&clock.v3());
    parser.ingest(&clock.event("step/start", 0, 0, json!({"turn": 1})));
    assert!(!parser.state.awaiting_response);
}

#[test]
fn deepseek_invalid_output_counter_is_rejected_but_still_ends_the_wait() {
    let clock = Clock::new();
    for invalid in [json!(-1), json!(true), json!(1.5)] {
        let mut parser = clock.requested();
        let records = parser.ingest(&clock.message(Msg { usage: Some(json!({"outputTokens": invalid, "inputTokens": 10})), ..Msg::default() }));
        assert!(records.is_empty(), "output counter {invalid} must not become a call");
        assert!(!parser.state.awaiting_response, "invalid final usage still closes response activity");
    }
}

#[test]
fn deepseek_zero_output_is_not_inferred_from_total_tokens() {
    let clock = Clock::new();
    let mut parser = clock.requested();
    let record = single(parser.ingest(&clock.message(Msg { usage: Some(json!({"inputTokens": 5, "outputTokens": 0, "totalTokens": 999})), ..Msg::default() })), "zero output");
    assert_eq!(record.output_tokens, 0);
}

#[test]
fn deepseek_surface_rewrites_are_not_provider_calls() {
    let clock = Clock::new();
    let mut parser = clock.requested();
    assert!(parser.ingest(&clock.message(Msg { surface_op: Some("replace"), ..Msg::default() })).is_empty());
    assert!(!parser.state.awaiting_response, "the rewrite still ends the wait");
}

#[test]
fn deepseek_failed_stream_finish_marks_the_call_interrupted() {
    let clock = Clock::new();
    let mut parser = clock.requested();
    let stream = vec![clock.chunk("finish", 5000, json!({"reason": {"kind": "error"}}))];
    assert!(single(parser.ingest(&clock.message(Msg { stream, ..Msg::default() })), "failed finish").aborted);
}

#[test]
fn deepseek_stream_stamps_after_the_message_create_no_negative_windows() {
    let clock = Clock::new();
    let mut parser = clock.requested();
    let stream = vec![clock.chunk("text-delta", 7000, json!({"text": "late"})), clock.chunk("finish", 8000, json!({"reason": {"kind": "stop"}}))];
    let record = single(parser.ingest(&clock.message(Msg { stream, ..Msg::default() })), "future stamps");
    assert_eq!(record.end, clock.after(6000), "the call ends when its message was written, not at a later stream stamp");
    assert_eq!(record.first_token, None, "a first token after the end is discarded");
}

// ---- DeepSeek files: inherited boundaries, generations, tails ---------------------------------

fn deepseek_watcher(root: &Path) -> SessionLogWatcher {
    SessionLogWatcher::new(&[LogSource::new("DeepSeek CLI", root)])
}

fn has_deepseek(watcher: &SessionLogWatcher) -> bool {
    watcher.has_logs("DeepSeek CLI")
}

#[test]
fn deepseek_seeded_log_admits_nothing_until_its_last_inherited_boundary_is_known() {
    let clock = Clock::new();
    let files = Scratch::new("ds-seeded");
    let missing = files.put("missing/session.v3.jsonl", lines(&[clock.header(3, true, None), clock.first_start(), clock.message(Msg { id: "ancestor", ..Msg::default() })]));
    let mut watcher = deepseek_watcher(&files.root);
    assert!(watcher.poll(clock.now()).is_empty(), "no ancestor call is admitted");
    assert!(!has_deepseek(&watcher) && watcher.active(clock.now()).is_empty(), "unresolved inherited cut is unavailable, not live");
    assert!(watcher.errors().values().any(|error| error.contains("inherited boundary")), "missing inherited boundary is surfaced: {:?}", watcher.errors());
    append(
        &missing,
        lines(&[
            clock.event("session/end-seed", 2, 6001, json!({"inherited": true})),
            clock.start(3, 10000, 2),
            clock.message(Msg { seq: 4, at: 16000, id: "ancestor-two", step: 2, ..Msg::default() }),
            clock.event("session/end-seed", 5, 16001, json!({"inherited": true})),
            clock.start(6, 20000, 3),
            clock.message(Msg { seq: 7, at: 26000, id: "owned", step: 3, ..Msg::default() }),
        ]),
    );
    assert!(watcher.poll(clock.now().add_seconds(1.0)).is_empty(), "seed metadata pass admits no ancestor calls");
    assert_eq!(keys(&watcher.poll(clock.now().add_seconds(2.0))), ["deepseek:owned"], "last inherited cut excludes nested ancestors");
}

#[test]
fn deepseek_historical_seed_length_suppresses_inherited_calls() {
    let clock = Clock::new();
    for version in [0, 1] {
        let files = Scratch::new("ds-seedlength");
        // V0's canonical generation name has no .v0 suffix.
        let name = if version == 0 { "session.jsonl".to_string() } else { format!("session.v{version}.jsonl") };
        files.put(
            &name,
            lines(&[
                clock.header(version, false, Some(2)),
                clock.first_start(),
                clock.message(Msg { id: "inherited", ..Msg::default() }),
                clock.start(2, 10000, 2),
                clock.message(Msg { seq: 3, at: 16000, id: "child", step: 2, ..Msg::default() }),
            ]),
        );
        let mut watcher = deepseek_watcher(&files.root);
        assert_eq!(keys(&watcher.poll(clock.now())), ["deepseek:child"], "v{version}");
    }
}

#[test]
fn deepseek_highest_generation_wins_and_a_future_one_never_falls_back() {
    let clock = Clock::new();
    let files = Scratch::new("ds-generations");
    files.put("session.v1.jsonl", lines(&[clock.header(1, false, None), clock.first_start(), clock.message(Msg { id: "old", ..Msg::default() })]));
    files.put("session.v3.jsonl", lines(&[clock.v3(), clock.first_start(), clock.message(Msg { id: "current", ..Msg::default() })]));
    let mut watcher = deepseek_watcher(&files.root);
    assert_eq!(keys(&watcher.poll(clock.now())), ["deepseek:current"], "highest numeric generation wins");
    files.put("session.v5.jsonl", lines(&[clock.header(5, false, None), clock.first_start(), clock.message(Msg { id: "unsupported", ..Msg::default() })]));
    let later = clock.now().add_seconds(5.0);
    assert!(watcher.poll(later).is_empty(), "an unsupported generation yields no calls");
    assert!(!has_deepseek(&watcher) && watcher.active(later).is_empty(), "future generation never falls back to older files");
}

#[test]
fn deepseek_ambiguous_raw_and_compressed_generation_is_refused() {
    let clock = Clock::new();
    let files = Scratch::new("ds-ambiguous");
    let content = lines(&[clock.v3(), clock.first_start()]);
    files.put("session.v3.jsonl", &content);
    files.put("session.v3.jsonl.zstd", frame(&content));
    let mut watcher = deepseek_watcher(&files.root);
    assert!(watcher.poll(clock.now()).is_empty() && !has_deepseek(&watcher));
    assert!(watcher.errors().values().any(|error| error.contains("Ambiguous")), "{:?}", watcher.errors());
}

#[test]
fn deepseek_raw_tail_waits_for_whole_rows_and_replays_emit_nothing_twice() {
    let clock = Clock::new();
    let files = Scratch::new("ds-tail");
    let path = files.put("session.v3.jsonl", lines(&[clock.v3(), clock.first_start()]));
    let mut watcher = deepseek_watcher(&files.root);
    watcher.poll(clock.now());
    assert!(has_deepseek(&watcher) && watcher.active(clock.now()).len() == 1, "the request is live");
    assert!(watcher.active(clock.now().add_minutes(11.0)).is_empty(), "DeepSeek live request expires without new response");
    let response = lines(&[clock.message(Msg::default())]);
    let (first, second) = response.split_at(response.len() / 2);
    append(&path, first);
    assert!(watcher.poll(clock.now().add_seconds(1.0)).is_empty(), "torn raw row is not interpreted prematurely");
    append(&path, second);
    let later = clock.now().add_seconds(2.0);
    assert_eq!(watcher.poll(later).len(), 1, "completed raw row emits once");
    assert!(watcher.active(later).is_empty(), "completed raw row clears activity");
    std::fs::write(&path, lines(&[clock.v3(), clock.first_start(), clock.message(Msg::default())])).unwrap();
    assert!(watcher.poll(clock.now().add_seconds(3.0)).is_empty(), "replacement replay keeps stable response-key dedup");
}

#[test]
fn deepseek_zstd_frame_survives_arbitrary_append_boundaries() {
    let clock = Clock::new();
    let files = Scratch::new("ds-zstd");
    let path = files.put("session.v3.jsonl.zstd", frame(&lines(&[clock.v3(), clock.first_start()])));
    let mut watcher = deepseek_watcher(&files.root);
    watcher.poll(clock.now());
    let later = clock.now().add_seconds(1.0);
    let mut records = Vec::new();
    // The second frame arrives seven bytes at a time, with a poll after each piece.
    for piece in frame(&lines(&[clock.message(Msg::default())])).chunks(7) {
        append(&path, piece);
        records.extend(watcher.poll(later));
    }
    assert_eq!(records.len(), 1, "the response is read exactly once, when its frame is complete");
    assert!(watcher.active(later).is_empty(), "the completed response clears activity");
    assert!(watcher.errors().is_empty(), "a partly written frame is not an error: {:?}", watcher.errors());
}

#[test]
fn deepseek_corrupt_frame_clears_availability_and_a_repaired_file_recovers() {
    let clock = Clock::new();
    let files = Scratch::new("ds-corrupt");
    let path = files.put("session.v3.jsonl.zstd", frame(&lines(&[clock.v3(), clock.first_start()])));
    let mut watcher = deepseek_watcher(&files.root);
    watcher.poll(clock.now());
    let mut invalid = frame(&lines(&[clock.message(Msg::default())]));
    invalid[0] ^= 0xff;
    append(&path, &invalid);
    let later = clock.now().add_seconds(1.0);
    watcher.poll(later);
    assert!(!has_deepseek(&watcher) && watcher.active(later).is_empty(), "a corrupt later frame clears availability and active state");
    assert!(!watcher.errors().is_empty(), "the corruption is reported");
    std::fs::write(&path, frame(&lines(&[clock.v3(), clock.first_start(), clock.message(Msg { id: "repaired", ..Msg::default() })]))).unwrap();
    let repaired = watcher.poll(clock.now().add_seconds(2.0));
    assert!(repaired.iter().any(|record| record.key == "deepseek:repaired"), "repair resets the failed decoder without suppressing valid recovered usage");
    assert!(has_deepseek(&watcher) && watcher.errors().is_empty(), "the repaired file is readable again: {:?}", watcher.errors());
}

// Session folders that each hold one very long tool-output line before their response.
const SESSION_IDS: [&str; 6] = ["budget-0", "budget-1", "budget-2", "budget-3", "budget-4", "budget-5"];

fn sessions_with_long_lines(clock: &Clock, count: usize, line_bytes: usize) -> Scratch {
    let files = Scratch::new("ds-budget");
    let long_line = "x".repeat(line_bytes);
    for (index, id) in SESSION_IDS.iter().take(count).enumerate() {
        files.put(
            &format!("{index}/session.v3.jsonl"),
            lines(&[clock.v3(), clock.first_start(), clock.event("tool/result", 1, 1000, json!({"turn": 1, "step": 1, "output": long_line})), clock.message(Msg { seq: 2, id, ..Msg::default() })]),
        );
    }
    files
}

fn poll_until_settled(watcher: &mut SessionLogWatcher, clock: &Clock, first: Vec<LogRecord>) -> BTreeSet<String> {
    let mut seen: BTreeSet<String> = first.into_iter().map(|record| record.key).collect();
    for index in 0..8 {
        seen.extend(watcher.poll(clock.now().add_seconds(f64::from(index + 1))).into_iter().map(|record| record.key));
    }
    seen
}

// One poll reads at most 16 MB across all files. Six sessions of 3 MB each are 18 MB, and each is
// small enough to finish in one go, so only the shared limit can stop the sixth.
#[test]
fn deepseek_tails_share_one_byte_budget_per_poll_and_all_resume() {
    let clock = Clock::new();
    let files = sessions_with_long_lines(&clock, 6, 3 * 1024 * 1024);
    let mut watcher = deepseek_watcher(&files.root);
    let first = watcher.poll(clock.now());
    assert!(first.len() < 6, "decoded-byte budget is shared across tails, got all {} in one poll", first.len());
    assert_eq!(poll_until_settled(&mut watcher, &clock, first).len(), 6, "bounded tail polling resumes all remaining sessions");
}

// One file is read at most 4 MB per poll, however much of the shared budget is left. Each response
// here sits behind a line just over 4 MB, so none can be reached in the first poll.
#[test]
fn deepseek_one_tail_reads_at_most_four_megabytes_per_poll_and_resumes() {
    let clock = Clock::new();
    let files = sessions_with_long_lines(&clock, 5, 4 * 1024 * 1024 + 100);
    let mut watcher = deepseek_watcher(&files.root);
    let first = watcher.poll(clock.now());
    assert_eq!(keys(&first), [""; 0], "no response behind more than 4 MB is reached in one poll");
    assert_eq!(poll_until_settled(&mut watcher, &clock, first).len(), 5, "bounded tail polling resumes all remaining sessions");
}


// ---- opencode ---------------------------------------------------------------------------------

/// An opencode database in write-ahead-log mode, kept open by its writer as opencode keeps it.
struct OpenCodeDb {
    files: Scratch,
    writer: Db,
    sequence: i64,
}

struct Assistant {
    current: bool,
    terminal: bool,
    created: i64,
    content: Option<Value>,
    usage: bool,
}

impl Default for Assistant {
    fn default() -> Self {
        Assistant { current: true, terminal: true, created: 0, content: None, usage: true }
    }
}

impl Clock {
    // An assistant message as opencode stores it: the v2 `session_message` shape, or the legacy one.
    fn assistant(&self, assistant: Assistant) -> Value {
        let mut data = Map::new();
        let created = self.at(assistant.created);
        data.insert("time".into(), if assistant.terminal { json!({"created": created, "completed": self.at(assistant.created + 20000)}) } else { json!({"created": created}) });
        if assistant.terminal {
            data.insert("finish".into(), json!("stop"));
        }
        if assistant.usage {
            data.insert("tokens".into(), json!({"input": 100, "output": 80, "reasoning": 20, "cache": {"read": 40, "write": 10}}));
        }
        if assistant.current {
            data.insert("model".into(), json!({"id": "fixture-model", "providerID": "fixture-provider"}));
            data.insert("content".into(), assistant.content.unwrap_or_else(|| json!([])));
        } else {
            data.insert("role".into(), json!("assistant"));
            data.insert("modelID".into(), json!("fixture-model"));
            data.insert("providerID".into(), json!("fixture-provider"));
        }
        Value::Object(data)
    }
}

impl OpenCodeDb {
    fn new() -> OpenCodeDb {
        let files = Scratch::new("opencode");
        let writer = Db::create(&files.path("opencode.db"));
        writer.run("PRAGMA journal_mode=WAL", &[]);
        OpenCodeDb { files, writer, sequence: 0 }
    }
    fn root(&self) -> PathBuf {
        self.files.root.clone()
    }
    fn database(&self) -> PathBuf {
        self.files.path("opencode.db")
    }
    fn schema(&self, current: bool, legacy: bool, indexed: bool) {
        if current {
            self.writer.run("CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,type TEXT NOT NULL,seq INTEGER NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL)", &[]);
            if indexed {
                self.writer.run("CREATE INDEX session_message_created ON session_message(time_created)", &[]);
            }
            self.writer.run("CREATE UNIQUE INDEX session_message_sequence ON session_message(session_id,seq)", &[]);
        }
        if legacy {
            self.writer.run(
                "CREATE TABLE session(id TEXT PRIMARY KEY,time_updated INTEGER NOT NULL); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL); CREATE INDEX message_session_created ON message(session_id,time_created,id); CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT NOT NULL,session_id TEXT NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL); CREATE INDEX part_message ON part(message_id,id)",
                &[],
            );
        }
    }
    fn message(&mut self, id: &str, session: &str, created: i64, data: &Value, current: bool) {
        let data = data.to_string();
        if current {
            self.writer.run("INSERT INTO session_message VALUES(?1,?2,'assistant',?3,?4,?4,?5)", &[Param::Text(id), Param::Text(session), Param::Int(self.sequence), Param::Int(created), Param::Text(&data)]);
            self.sequence += 1;
        } else {
            self.writer.run("INSERT OR REPLACE INTO session VALUES(?1,?2)", &[Param::Text(session), Param::Int(created + 30000)]);
            self.writer.run("INSERT INTO message VALUES(?1,?2,?3,?3,?4)", &[Param::Text(id), Param::Text(session), Param::Int(created), Param::Text(&data)]);
        }
    }
    fn update(&self, id: &str, data: &Value) {
        self.writer.run("UPDATE session_message SET data=?1,time_updated=time_updated+1 WHERE id=?2", &[Param::Text(&data.to_string()), Param::Text(id)]);
    }
    fn update_all(&self, data: &Value) {
        self.writer.run("UPDATE session_message SET data=?1,time_updated=time_updated+1", &[Param::Text(&data.to_string())]);
    }
    #[allow(clippy::too_many_arguments)]
    fn part(&self, id: &str, message: &str, kind: &str, persisted: i64, time: Value, synthetic: bool, ignored: bool, status: Option<&str>) {
        let data = json!({"type": kind, "time": time, "synthetic": synthetic, "ignored": ignored, "state": {"status": status}}).to_string();
        self.writer.run("INSERT INTO part VALUES(?1,?2,'fixture-session',?3,?3,?4)", &[Param::Text(id), Param::Text(message), Param::Int(persisted), Param::Text(&data)]);
    }
}

fn opencode_keys(records: Vec<LogRecord>) -> BTreeSet<String> {
    records.into_iter().map(|record| record.key).collect()
}

#[test]
fn opencode_current_schema_timing_activity_and_delayed_usage() {
    let clock = Clock::new();
    let mut db = OpenCodeDb::new();
    db.schema(true, false, true);
    let at = clock.at(0);
    db.message("text-first", "text", at, &clock.assistant(Assistant { content: Some(json!([{"type": "text", "text": "unretained content"}, {"type": "reasoning", "time": {"created": clock.at(2000)}}])), ..Assistant::default() }), true);
    db.message("reasoning-first", "reasoning", at, &clock.assistant(Assistant { content: Some(json!([{"type": "reasoning", "time": {"created": clock.at(2000)}}, {"type": "text"}])), ..Assistant::default() }), true);
    db.message(
        "tool-paused",
        "tool",
        at,
        &clock.assistant(Assistant { terminal: false, usage: false, content: Some(json!([{"type": "tool", "time": {"created": clock.at(2000)}, "state": {"status": "running", "input": "unretained arguments"}}])), ..Assistant::default() }),
        true,
    );
    db.message("delayed", "delay", at, &clock.assistant(Assistant { usage: false, ..Assistant::default() }), true);
    db.message("superseded", "same-session", at, &clock.assistant(Assistant { terminal: false, usage: false, ..Assistant::default() }), true);
    db.message("latest", "same-session", clock.at(21000), &clock.assistant(Assistant { created: 21000, ..Assistant::default() }), true);
    let mut summary = clock.assistant(Assistant { terminal: false, usage: false, ..Assistant::default() });
    summary["summary"] = json!(true);
    db.message("summary", "summary", at, &summary, true);

    let mut watcher = OpenCodeLogWatcher::new([db.database()]);
    let records = watcher.poll(clock.now());
    let text = records.iter().find(|record| record.key == "opencode:text-first").expect("text-first call recorded");
    assert_eq!((text.first_token, text.first_visible), (None, None), "v2 text-first never borrows a later reasoning timestamp");
    assert_eq!(text.make_record().tps, None, "no generation window means no speed");
    let reasoning = records.iter().find(|record| record.key == "opencode:reasoning-first").expect("reasoning-first call recorded").make_record();
    assert_eq!((reasoning.ttft, reasoning.first_visible), (None, None), "v2 request-relative timing remains unknown");
    near(reasoning.generation, 18.0, "v2 independently measured generation survives absent request timing");
    near(reasoning.tps, 100.0 / 18.0, "v2 exact generation-only TPS reaches the record");
    assert!(watcher.has_logs(), "the database is readable telemetry");
    assert!(watcher.active(clock.now()).is_empty(), "tool pause, summary, finished missing usage and superseded rows are not active: {:?}", ids(&watcher.active(clock.now())));

    let mut finished = clock.assistant(Assistant::default());
    finished["finish"] = json!("error");
    finished["error"] = json!({"type": "unknown", "message": "unretained diagnostic"});
    db.update("delayed", &finished);
    let delayed = watcher.poll(clock.now().add_seconds(1.0));
    assert_eq!(keys(&delayed), ["opencode:delayed"], "terminal missing usage remains refreshable until the actual usage arrives");
    assert!(delayed[0].aborted, "a call that ended in an error is marked interrupted");
    assert!(watcher.poll(clock.now().add_seconds(2.0)).is_empty(), "unchanged database does not rescan emitted calls");

    db.message("pending", "new-session", clock.at(25000), &clock.assistant(Assistant { terminal: false, created: 25000, usage: false, ..Assistant::default() }), true);
    watcher.poll(clock.now().add_seconds(3.0));
    assert_eq!(watcher.active(clock.now().add_seconds(3.0)).len(), 1, "current incomplete telemetry has source-based activity");
    assert!(watcher.active(clock.now().add_minutes(11.0)).is_empty(), "and that activity expires");
    db.update("pending", &clock.assistant(Assistant { created: 25000, ..Assistant::default() }));
    assert_eq!(watcher.poll(clock.now().add_seconds(4.0)).len(), 1, "WAL in-place completion refreshes");
    assert!(watcher.active(clock.now().add_seconds(4.0)).is_empty(), "and clears pending");
}

fn ids(calls: &[LiveCall]) -> Vec<&str> {
    calls.iter().map(|call| call.id.as_str()).collect()
}

#[test]
fn opencode_legacy_schema_timings_and_counter_validation() {
    let clock = Clock::new();
    let mut db = OpenCodeDb::new();
    db.schema(false, true, true);
    let legacy = || clock.assistant(Assistant { current: false, ..Assistant::default() });
    db.message("legacy", "legacy-session", clock.at(0), &legacy(), false);
    db.part("reason", "legacy", "reasoning", clock.at(10000), json!({"start": clock.at(1000), "end": clock.at(8000)}), false, false, None);
    db.part("synthetic", "legacy", "text", clock.at(1000), json!({"start": clock.at(10)}), true, false, None);
    db.part("ignored", "legacy", "text", clock.at(1000), json!({"start": clock.at(20)}), false, true, None);
    db.part("text", "legacy", "text", clock.at(12000), json!({"start": clock.at(2000)}), false, false, None);
    db.part("finish", "legacy", "step-finish", clock.at(11000), json!({"end": clock.at(19000)}), false, false, None);
    let mut negative = legacy();
    negative["tokens"] = json!({"output": -1, "reasoning": 2, "input": 1});
    db.message("negative", "negative-session", clock.at(0), &negative, false);
    let mut missing = legacy();
    missing["tokens"] = json!({"reasoning": 2, "input": 1});
    db.message("missing", "missing-session", clock.at(0), &missing, false);
    let mut overflow = legacy();
    overflow["tokens"] = json!({"output": i32::MAX, "reasoning": 1});
    db.message("overflow", "overflow-session", clock.at(0), &overflow, false);
    db.message("paused", "paused-session", clock.at(0), &clock.assistant(Assistant { current: false, terminal: false, ..Assistant::default() }), false);
    db.part("paused-tool", "paused", "tool", clock.at(1000), Value::Null, false, false, Some("completed"));

    let mut watcher = OpenCodeLogWatcher::new([db.root()]);
    let record = single(watcher.poll(clock.now()), "negative/missing/overflowing output cannot become exact OpenCode calls");
    assert_eq!((record.key.as_str(), record.output_tokens, record.input_tokens), ("opencode:legacy", 100, Some(150)));
    let stored = record.make_record();
    near(stored.ttft, 1.0, "synthetic and ignored part timings are excluded");
    near(stored.first_visible, 2.0, "legacy visible timestamp is from actual text");
    near(stored.generation, 10.0, "legacy generation ends at step-finish persistence, not time.end/tool cleanup");
    assert!(watcher.active(clock.now()).is_empty(), "legacy tool statuses suppress model activity");
}

#[test]
fn opencode_refresh_queue_continues_past_one_batch_of_pending_rows() {
    let clock = Clock::new();
    let mut db = OpenCodeDb::new();
    db.schema(true, false, true);
    for index in 0..300 {
        db.message(&format!("pending-{index:03}"), &format!("session-{index}"), clock.at(index), &clock.assistant(Assistant { terminal: false, created: index, usage: false, ..Assistant::default() }), true);
    }
    let mut watcher = OpenCodeLogWatcher::new([db.root()]);
    for _ in 0..5 {
        assert!(watcher.poll(clock.now()).is_empty(), "bounded history pages do not fabricate incomplete calls");
    }
    db.update_all(&clock.assistant(Assistant::default()));
    let mut seen = BTreeSet::new();
    for index in 0..5 {
        seen.extend(opencode_keys(watcher.poll(clock.now().add_seconds(f64::from(index + 1)))));
    }
    assert_eq!(seen.len(), 300, "refresh queue continues past 128 pending IDs even after data_version stops changing");
    assert!(watcher.active(clock.now().add_seconds(5.0)).is_empty(), "completed rows are no longer live");
    assert!(watcher.poll(clock.now().add_seconds(6.0)).is_empty(), "completed bounded backfill does not repeat unchanged DB work");
}

#[test]
fn opencode_unindexed_schema_is_walked_in_bounded_pages() {
    let clock = Clock::new();
    let mut db = OpenCodeDb::new();
    db.schema(true, false, false);
    for index in 0..300 {
        db.message(&format!("fallback-{index}"), &format!("fallback-session-{index}"), clock.at(index), &clock.assistant(Assistant { created: index, ..Assistant::default() }), true);
    }
    let mut watcher = OpenCodeLogWatcher::new([db.root()]);
    let mut seen = BTreeSet::new();
    for _ in 0..5 {
        seen.extend(opencode_keys(watcher.poll(clock.now())));
    }
    assert_eq!(seen.len(), 300, "unindexed schema walks bounded rowid pages instead of abandoning history or sorting the DB");
}

#[test]
fn opencode_schema_created_after_opening_is_discovered() {
    let clock = Clock::new();
    let mut db = OpenCodeDb::new();
    let mut watcher = OpenCodeLogWatcher::new([db.root()]);
    assert!(watcher.poll(clock.now()).is_empty() && !watcher.has_logs(), "empty supported root is not readable telemetry");
    db.schema(true, false, true);
    db.message("new-schema", "schema", clock.at(0), &clock.assistant(Assistant::default()), true);
    assert_eq!(watcher.poll(clock.now().add_seconds(1.0)).len(), 1, "schema added after opening is discovered without restart");
    assert!(watcher.has_logs());
}

#[test]
fn opencode_new_rows_are_caught_up_in_bounded_chunks() {
    let clock = Clock::new();
    let mut db = OpenCodeDb::new();
    db.schema(true, false, true);
    let mut watcher = OpenCodeLogWatcher::new([db.root()]);
    watcher.poll(clock.now());
    for index in 0..300 {
        db.message(&format!("inserted-{index}"), &format!("inserted-session-{index}"), clock.at(index), &clock.assistant(Assistant { created: index, ..Assistant::default() }), true);
    }
    let first = watcher.poll(clock.now().add_seconds(1.0));
    assert!(first.len() <= 128, "new insertion catch-up obeys the per-table chunk ceiling, got {}", first.len());
    let mut seen = opencode_keys(first);
    for index in 0..5 {
        seen.extend(opencode_keys(watcher.poll(clock.now().add_seconds(f64::from(index + 2)))));
    }
    assert_eq!(seen.len(), 300, "insertion cursor resumes after the database becomes unchanged");
}

#[test]
fn opencode_legacy_json_files_resume_and_follow_in_place_updates() {
    let clock = Clock::new();
    let files = Scratch::new("opencode-json");
    let identified = |mut data: Value, id: &str, session: &str| {
        data["id"] = json!(id);
        data["sessionID"] = json!(session);
        data
    };
    for index in 0..270 {
        let data = identified(clock.assistant(Assistant { current: false, terminal: false, created: index, usage: false, ..Assistant::default() }), &format!("pending-{index}"), &format!("session-{index}"));
        files.put(&format!("storage/message/session-{index}/pending-{index}.json"), data.to_string());
    }
    files.put("storage/message/completed/json-complete.json", identified(clock.assistant(Assistant { current: false, ..Assistant::default() }), "json-complete", "json-complete-session").to_string());
    let mut watcher = OpenCodeLogWatcher::new([files.root.clone()]);
    let mut seen = BTreeSet::new();
    for index in 0..4 {
        seen.extend(opencode_keys(watcher.poll(clock.now().add_seconds(f64::from(index)))));
    }
    assert!(seen.contains("opencode:json-complete"), "unresolved JSON files cannot starve later completed files");

    let mut pending = identified(clock.assistant(Assistant { current: false, terminal: false, usage: false, ..Assistant::default() }), "json-parts", "json-parts-session");
    let file = files.put("storage/message/parts/json-parts.json", pending.to_string());
    watcher.poll(clock.now().add_seconds(5.0));
    watcher.poll(clock.now().add_seconds(6.0));
    pending["finish"] = json!("tool-calls");
    pending["tokens"] = json!({"input": 10, "output": 5, "reasoning": 1});
    pending["time"] = json!({"created": clock.at(0), "completed": clock.at(20000)});
    std::fs::write(&file, pending.to_string()).unwrap();
    let finalized = watcher.poll(clock.now().add_seconds(7.0));
    assert!(finalized.iter().any(|record| record.key == "opencode:json-parts"), "legacy JSON in-place finalization remains refreshable");
    assert!(!watcher.active(clock.now().add_seconds(7.0)).iter().any(|call| call.id.ends_with("json-parts")), "the finalized message is no longer live");

    let paused = identified(clock.assistant(Assistant { current: false, terminal: false, created: 30000, ..Assistant::default() }), "json-tool", "json-tool-session");
    files.put("storage/message/tools/json-tool.json", paused.to_string());
    watcher.poll(clock.now().add_seconds(9.0));
    watcher.poll(clock.now().add_seconds(10.0));
    files.put("storage/part/json-tool/tool.json", json!({"type": "tool", "state": {"status": "running", "input": "unretained"}}).to_string());
    watcher.poll(clock.now().add_seconds(11.0));
    assert!(!watcher.active(clock.now().add_seconds(11.0)).iter().any(|call| call.id.ends_with("json-tool")), "independent tool-part updates clear JSON activity without message mtime growth");
    assert!(watcher.poll(clock.now().add_seconds(12.0)).is_empty(), "JSON completed response keys are not emitted twice");
}

// ---- Gemini CLI -------------------------------------------------------------------------------
// Lines shaped as Gemini CLI 0.46's ChatRecordingService writes them.

#[test]
fn gemini_reply_is_recorded_once_and_tool_completion_dates_the_next_request() {
    let clock = Clock::new();
    let mut parser = GeminiCliLogParser::new();
    assert!(parser.ingest(&json!({"sessionId": "s1", "projectHash": "abc", "startTime": clock.iso(0), "lastUpdated": clock.iso(0), "kind": "main"})).is_empty(), "session metadata is not a call");
    assert!(parser.state.supported, "Gemini session metadata recognised");
    parser.ingest(&json!({"id": "u1", "timestamp": clock.iso(0), "type": "user", "content": [{"text": "private prompt"}]}));
    assert!(parser.state.awaiting_response, "Gemini user message opens a request");
    assert_eq!(parser.state.last_request_at, Some(clock.epoch));

    let mut reply = json!({
        "id": "g1", "timestamp": clock.iso(6000), "type": "gemini", "content": "",
        "thoughts": [{"subject": "s", "description": "d", "timestamp": clock.iso(1500)}, {"subject": "s", "description": "d", "timestamp": clock.iso(3000)}],
        "tokens": {"input": 1000, "output": 200, "cached": 400, "thoughts": 100, "tool": 0, "total": 1300}, "model": "gemini-3-pro"
    });
    let first = single(parser.ingest(&reply), "Gemini reply recorded once its stream has ended");
    assert_eq!(first.key, "gemini:g1");
    assert!(!parser.state.awaiting_response, "the reply ends the wait");
    let stored = first.make_record();
    assert_eq!((stored.harness.as_str(), stored.model.as_str(), stored.output_tokens, stored.reasoning_tokens), ("Gemini CLI", "gemini-3-pro", 300, Some(100)), "Gemini output counts thinking with the reply");
    assert_eq!((stored.input_tokens, stored.cached_input_tokens), (Some(1000), Some(400)), "Gemini input and cached tokens carried over");
    near(stored.ttft, 1.5, "Gemini first thought is the first token");
    near(stored.tps, 300.0 / 4.5, "Gemini speed spans first thought to stream end");

    reply["toolCalls"] = json!([{"id": "t1", "name": "read_file", "status": "success", "timestamp": clock.iso(9000)}]);
    assert!(parser.ingest(&reply).is_empty(), "Gemini rewritten message adds no record");
    assert!(parser.state.awaiting_response, "the finished tool means the next request has left");
    assert_eq!(parser.state.last_request_at, Some(clock.after(9000)), "and dates it");

    let second = single(parser.ingest(&json!({"id": "g2", "timestamp": clock.iso(12000), "type": "gemini", "content": "done", "thoughts": [], "tokens": {"input": 1200, "output": 60, "cached": 0, "thoughts": 0, "tool": 0, "total": 1260}, "model": "gemini-3-pro"})), "second reply");
    assert_eq!(second.request_start, Some(clock.after(9000)), "the follow-up reply starts at the tool's completion");
    let stored = second.make_record();
    assert_eq!((stored.ttft, stored.reasoning_tokens), (None, None), "Gemini reply without thoughts has no first-token time");
    near(stored.tps, 20.0, "Gemini reply without thoughts falls back to the whole request");

    let third = single(parser.ingest(&json!({"id": "g3", "timestamp": clock.iso(30000), "type": "gemini", "content": "b", "tokens": {"output": 40, "thoughts": 0}, "model": "gemini-3-pro"})), "third reply");
    assert_eq!(third.request_start, None, "Gemini reply with no new request never borrows the old start");
    assert_eq!((third.make_record().ttft, third.make_record().tps), (None, None));
}

#[test]
fn gemini_tokens_arriving_on_a_later_write_produce_exactly_one_record() {
    let clock = Clock::new();
    let mut parser = GeminiCliLogParser::new();
    parser.ingest(&json!({"id": "u1", "timestamp": clock.iso(0), "type": "user", "content": "x"}));
    let mut pending = json!({"id": "g1", "timestamp": clock.iso(5000), "type": "gemini", "content": "a", "tokens": null, "model": "m"});
    assert!(parser.ingest(&pending).is_empty(), "Gemini message without tokens waits");
    pending["tokens"] = json!({"output": 100, "thoughts": 0});
    let arrived = single(parser.ingest(&pending), "tokens on a later write");
    assert!(parser.ingest(&pending).is_empty(), "the same message written again adds nothing");
    near(arrived.make_record().tps, 20.0, "Gemini late tokens keep the message's own end time");
}

#[test]
fn gemini_metadata_updates_carry_messages_and_errors_end_the_wait() {
    let clock = Clock::new();
    let mut parser = GeminiCliLogParser::new();
    parser.ingest(&json!({"$set": {"messages": [{"id": "u1", "timestamp": clock.iso(0), "type": "user", "content": [{"text": "x"}]}], "lastUpdated": clock.iso(0)}}));
    assert!(parser.state.awaiting_response, "Gemini messages inside a metadata update count");
    assert_eq!(parser.state.last_request_at, Some(clock.epoch));
    assert!(parser.ingest(&json!({"id": "i1", "timestamp": clock.iso(1000), "type": "info", "content": "note"})).is_empty(), "Gemini status lines are not model calls");
    assert!(parser.state.awaiting_response, "and do not end the wait");
    parser.ingest(&json!({"id": "e1", "timestamp": clock.iso(2000), "type": "error", "content": "quota"}));
    assert!(!parser.state.awaiting_response, "Gemini error ends the wait");
}

// ---- Antigravity ------------------------------------------------------------------------------
// Protobuf laid out as Antigravity's rows are. The prompt fields are present, as in real rows.

fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(byte);
            return bytes;
        }
        bytes.push(byte | 0x80);
    }
}

fn number(field: u64, value: u64) -> Vec<u8> {
    [varint(field << 3), varint(value)].concat()
}

fn bytes(field: u64, value: &[u8]) -> Vec<u8> {
    [varint(field << 3 | 2), varint(value.len() as u64), value.to_vec()].concat()
}

fn text(field: u64, value: &str) -> Vec<u8> {
    bytes(field, value.as_bytes())
}

// A google.protobuf.Duration.
fn seconds(value: f64) -> Vec<u8> {
    let whole = value.floor();
    [number(1, whole as u64), number(2, ((value - whole) * 1_000_000_000.0).round() as u64)].concat()
}

// A google.protobuf.Timestamp.
fn stamp(time: Time) -> Vec<u8> {
    [number(1, time.unix_seconds() as u64), number(2, (time.0.rem_euclid(10_000_000) * 100) as u64)].concat()
}

struct Call {
    steps: &'static [u64],
    output: Option<u64>,
    thinking: u64,
    ttft: Option<f64>,
    streaming: Option<f64>,
}

// A gen_metadata row: CortexStepGeneratorMetadata wrapping ChatModelMetadata and ModelUsageStats.
fn generation(call: Call) -> Vec<u8> {
    let mut chat = [text(1, "SYSTEM PROMPT: a secret that must never be read"), bytes(2, &text(1, "user said something private")), number(3, 1318)].concat();
    if let Some(tokens) = call.output {
        chat.extend(bytes(4, &[number(1, 1318), number(2, 4000), number(3, tokens), number(5, 20_000), number(6, 24), number(9, call.thinking), number(10, tokens - call.thinking)].concat()));
    }
    if let Some(first) = call.ttft {
        chat.extend(bytes(11, &seconds(first)));
    }
    if let Some(length) = call.streaming {
        chat.extend(bytes(12, &seconds(length)));
    }
    chat.extend(text(19, "gemini-3.8-flash-n"));
    let steps: Vec<u8> = call.steps.iter().flat_map(|step| varint(*step)).collect();
    [bytes(1, &chat), bytes(2, &steps), text(4, "exec-1")].concat()
}

#[test]
fn antigravity_row_decodes_ids_usage_and_timing_without_reading_text() {
    let burst = AntigravityGeneration::parse(&generation(Call { steps: &[1, 2], output: Some(212), thinking: 148, ttft: Some(5.256), streaming: Some(0.302) })).expect("a well-formed row parses");
    assert_eq!((burst.step_indices.as_slice(), burst.execution_id.as_str(), burst.model.as_deref()), (&[1i64, 2][..], "exec-1", Some("gemini-3.8-flash-n")), "Antigravity ids, steps and model decoded");
    assert_eq!((burst.output_tokens, burst.thinking_tokens, burst.input_tokens, burst.cache_read_tokens), (Some(212), Some(148), Some(4000), Some(20_000)), "Antigravity usage decoded");
    assert!(burst.is_complete());
    near(burst.time_to_first_token, 5.256, "Antigravity time to first token decoded");
    near(burst.streaming_duration, 0.302, "Antigravity streaming duration decoded");
    assert_eq!(protobuf::identifier(b"user said something private"), None, "Antigravity free text is never accepted as an identifier");
    assert_eq!(protobuf::identifier(b"gemini-3.8-flash-n").as_deref(), Some("gemini-3.8-flash-n"));
}

#[test]
fn antigravity_short_burst_is_timed_over_the_whole_call() {
    let clock = Clock::new();
    let burst = AntigravityGeneration::parse(&generation(Call { steps: &[1, 2], output: Some(212), thinking: 148, ttft: Some(5.256), streaming: Some(0.302) })).unwrap();
    let stored = burst.record("c", 0, clock.epoch).expect("a complete call yields a record").make_record();
    near(stored.ttft, 5.256, "Antigravity TTFT is the harness's own");
    near(stored.generation, 0.302, "Antigravity generation is the streaming time");
    near(Some(stored.total), 5.558, "Antigravity total spans request to end");
    near(stored.tps, 212.0 / 5.558, "Antigravity burst is timed over the whole call, not the burst");
    assert_eq!((stored.output_tokens, stored.reasoning_tokens, stored.input_tokens, stored.cached_input_tokens), (212, Some(148), Some(24_000), Some(20_000)));
    assert_eq!((stored.harness.as_str(), stored.source.as_deref()), ("Antigravity", Some("log")));
}

#[test]
fn antigravity_long_stream_is_timed_on_its_visible_tokens() {
    let clock = Clock::new();
    let longer = AntigravityGeneration::parse(&generation(Call { steps: &[39], output: Some(3900), thinking: 2556, ttft: Some(17.926), streaming: Some(31.942) })).unwrap().record("c", 19, clock.epoch).unwrap();
    assert_eq!(longer.key, "antigravity:c:exec-1:19", "Antigravity key is stable per conversation, turn and call");
    near(longer.make_record().tps, 1344.0 / 31.942, "Antigravity long stream is timed on its visible tokens");
}

#[test]
fn antigravity_malformed_bytes_are_rejected() {
    assert_eq!(AntigravityGeneration::parse(&[0xFF, 0xFF, 0xFF]), None);
    assert_eq!(protobuf::fields(&[0x0A, 0x05, 0x01]), None, "a length that runs past the end is not a message");
}

/// One Antigravity conversation database, kept open by its writer.
struct Conversation {
    files: Scratch,
    writer: Db,
    epoch: Time,
}

impl Conversation {
    fn new(epoch: Time) -> Conversation {
        Self::with_journal(epoch, false)
    }
    // Antigravity itself keeps its conversations in write-ahead-log mode.
    fn with_journal(epoch: Time, write_ahead_log: bool) -> Conversation {
        let files = Scratch::new("antigravity");
        let writer = Db::create(&files.path("11111111-0000-4000-8000-000000000001.db"));
        if write_ahead_log {
            writer.run("PRAGMA journal_mode=WAL", &[]);
        }
        writer.run("CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY (idx))", &[]);
        writer.run("CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, status integer NOT NULL DEFAULT 0, metadata blob, step_payload blob, PRIMARY KEY (idx))", &[]);
        Conversation { files, writer, epoch }
    }
    // A model-response step: created_at = 1, completed_at = 8.
    fn step(&self, index: i64, created: f64, completed: Option<f64>) {
        let mut metadata = [bytes(1, &stamp(self.epoch.add_seconds(created))), number(3, 7)].concat();
        if let Some(end) = completed {
            metadata.extend(bytes(8, &stamp(self.epoch.add_seconds(end))));
        }
        self.writer.run("INSERT OR REPLACE INTO steps (idx, step_type, status, metadata, step_payload) VALUES (?1, 15, 3, ?2, ?3)", &[Param::Int(index), Param::Blob(&metadata), Param::Blob(b"reply text")]);
    }
    fn generation(&self, index: i64, data: &[u8]) {
        self.writer.run("INSERT OR REPLACE INTO gen_metadata (idx, data, size) VALUES (?1, ?2, 0)", &[Param::Int(index), Param::Blob(data)]);
    }
    fn watcher(&self) -> AntigravityLogWatcher {
        AntigravityLogWatcher::new([self.files.root.clone()])
    }
    // Closes the writer, as when Antigravity exits, and hands back the folder.
    fn close(self) -> Scratch {
        self.files
    }
}

fn file_names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn antigravity_finished_calls_are_read_once_and_reading_leaves_no_files_behind() {
    let clock = Clock::new();
    let conversation = Conversation::new(clock.epoch);
    conversation.step(1, 0.019, Some(5.577));
    conversation.generation(0, &generation(Call { steps: &[1, 2], output: Some(212), thinking: 148, ttft: Some(5.256), streaming: Some(0.302) }));
    conversation.step(3, 5.593, Some(10.633));
    conversation.generation(1, &generation(Call { steps: &[3, 4], output: Some(154), thinking: 76, ttft: Some(4.978), streaming: Some(0.062) }));
    let mut watcher = conversation.watcher();
    let records = watcher.poll(clock.now());
    assert!(watcher.has_logs(), "the conversation is readable telemetry");
    assert_eq!(records.len(), 2, "Antigravity calls read from a conversation file");
    assert_eq!(records[0].request_start, Some(clock.after(19)), "the call starts when its first step was created");
    assert_eq!(watcher.latest_model(), Some("gemini-3.8-flash-n"));
    assert!(watcher.active(clock.now()).is_empty(), "Antigravity finished calls are not active");
    assert_eq!(watcher.last_request_at(), Some(clock.after(5593)));
    assert!(watcher.poll(clock.now().add_seconds(10.0)).is_empty(), "Antigravity unchanged file reports nothing twice");
    let left: Vec<_> = std::fs::read_dir(&conversation.files.root).unwrap().flatten().map(|entry| entry.file_name()).collect();
    assert_eq!(left.len(), 1, "reading leaves no -shm or -wal companions behind: {left:?}");
}

#[test]
fn antigravity_call_in_flight_is_awaited_then_reported_once_and_a_stuck_one_expires() {
    let clock = Clock::new();
    let conversation = Conversation::new(clock.epoch);
    conversation.step(1, 0.0, None);
    conversation.generation(0, &generation(Call { steps: &[1], output: None, thinking: 0, ttft: None, streaming: None }));
    let mut watcher = conversation.watcher();
    assert!(watcher.poll(clock.now()).is_empty(), "Antigravity call in flight is not recorded");
    let live = watcher.active(clock.now());
    assert_eq!(live.len(), 1, "it is awaited");
    assert_eq!(live[0].phase, "Waiting");

    conversation.step(1, 0.0, Some(8.0));
    conversation.generation(0, &generation(Call { steps: &[1], output: Some(400), thinking: 100, ttft: Some(3.0), streaming: Some(5.0) }));
    let later = clock.now().add_seconds(10.0);
    let done = single(watcher.poll(later), "Antigravity call is reported once it completes");
    assert!(watcher.active(later).is_empty(), "the completed call is no longer awaited");
    near(done.make_record().tps, 60.0, "Antigravity completed call speed");

    conversation.step(5, 20.0, None);
    watcher.poll(clock.now().add_seconds(20.0));
    assert_eq!(watcher.active(clock.now().add_seconds(20.0)).len(), 1, "a new request is in flight");
    assert!(watcher.active(clock.now().add_minutes(20.0)).is_empty(), "Antigravity request that never completes stops counting as in flight");
}

#[test]
fn antigravity_abandoned_and_malformed_rows_do_not_hold_up_later_calls() {
    let clock = Clock::new();
    let conversation = Conversation::new(clock.epoch);
    conversation.step(1, 0.0, None);
    conversation.generation(0, &generation(Call { steps: &[1], output: None, thinking: 0, ttft: None, streaming: None }));
    conversation.generation(1, &[0x0A, 0x7F, 0x00]);
    conversation.step(2, 10.0, Some(16.0));
    conversation.generation(2, &generation(Call { steps: &[2], output: Some(300), thinking: 0, ttft: Some(2.0), streaming: Some(4.0) }));
    let kept = single(conversation.watcher().poll(clock.now()), "only the complete call is kept");
    assert!(kept.key.ends_with(":exec-1:2"), "{}", kept.key);
}

// The incident this guards against: a plain read-only open of a write-ahead-log database whose log
// is absent makes SQLite create -shm and -wal companions that a read-only connection cannot remove,
// leaving files behind in Antigravity's own folder.
#[test]
fn antigravity_wal_mode_database_at_rest_is_read_without_creating_companion_files() {
    let clock = Clock::new();
    let conversation = Conversation::with_journal(clock.epoch, true);
    conversation.step(1, 0.0, Some(8.0));
    conversation.generation(0, &generation(Call { steps: &[1], output: Some(400), thinking: 100, ttft: Some(3.0), streaming: Some(5.0) }));
    let files = conversation.close();
    let database = "11111111-0000-4000-8000-000000000001.db";
    assert_eq!(file_names(&files.root), [database], "fixture: a cleanly closed write-ahead-log database is one file");
    let mut watcher = AntigravityLogWatcher::new([files.root.clone()]);
    assert_eq!(watcher.poll(clock.now()).len(), 1, "the call is read from the database at rest");
    assert_eq!(file_names(&files.root), [database], "reading created no -shm or -wal file");
}

// The other DeepSeek fixtures are compressed by the same library that decodes them. This one was
// produced by the reference Zstandard implementation (see fixtures/make_deepseek_reference.py):
// two frames with content checksums, the second three blocks long.
#[test]
fn deepseek_frames_from_the_reference_encoder_are_read_across_append_boundaries() {
    const REFERENCE: &[u8] = include_bytes!("fixtures/deepseek_reference.jsonl.zstd");
    // The fixture's fixed clock: 2026-10-08T12:34:56Z in epoch milliseconds.
    const STARTED: i64 = 1_791_462_896_000;
    let clock = Clock::new();
    let files = Scratch::new("ds-reference");
    let path = files.path("session.v3.jsonl.zstd");
    let mut watcher = deepseek_watcher(&files.root);
    let mut records = Vec::new();
    for piece in REFERENCE.chunks(7) {
        append(&path, piece);
        records.extend(watcher.poll(clock.now()));
        assert!(watcher.errors().is_empty(), "a frame still being written is not an error: {:?}", watcher.errors());
    }
    let record = single(records, "the session's one response");
    assert_eq!((record.key.as_str(), record.model.as_str(), record.output_tokens, record.input_tokens), ("deepseek:reference", "deepseek-v4", 80, Some(100)));
    assert_eq!(record.request_start, Some(Time::from_unix_ms(STARTED)), "the request time survives decompression exactly");
    assert_eq!(record.end, Time::from_unix_ms(STARTED + 6000));
    assert!(has_deepseek(&watcher));
}
