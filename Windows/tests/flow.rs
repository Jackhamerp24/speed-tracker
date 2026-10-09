//! The network-flow state machine and the tracker around it, driven by a scripted sampler:
//! traffic that must never count as a model call, upload grace, connection reuse, Live targets,
//! and recovery when history cannot be written. Ported from FlowRegression.cs.

mod common;

use common::{iso, sample, FakeSampler, Scratch};
use serde_json::json;
use speedtracker::discovery::LogSource;
use speedtracker::domain::{FlowSampler, LiveCall, RequestRecord};
use speedtracker::history::HistoryStore;
use speedtracker::time::Time;
use speedtracker::tracker::Tracker;
use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[track_caller]
fn near(actual: Option<f64>, expected: f64, label: &str) {
    common::near(actual, expected, 0.0001, label);
}

fn installed(names: &'static [&'static str]) -> Box<dyn Fn() -> BTreeSet<String> + Send + Sync> {
    Box::new(move || names.iter().map(|name| name.to_string()).collect())
}

/// A tracker on a fixed machine: these harnesses are installed and have no session logs,
/// whatever the real one has. Time is counted in seconds from `now`.
struct Fixture {
    files: Scratch,
    now: Time,
    history: Arc<HistoryStore>,
    tracker: Arc<Tracker>,
    sampler: Arc<FakeSampler>,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        Self::with_sources(label, |_| Vec::new())
    }
    fn with_sources(label: &str, sources: impl FnOnce(&Path) -> Vec<LogSource>) -> Fixture {
        let files = Scratch::new(label);
        let history = Arc::new(HistoryStore::new(Some(&files.path("history"))));
        let sampler = Arc::new(FakeSampler::default());
        let flows: Arc<dyn FlowSampler> = sampler.clone();
        let tracker = Tracker::new(Arc::clone(&history), Some(flows), Some(sources(&files.root)), Some(files.path("user")), Some(installed(&["Gemini CLI", "Qwen Code", "Aider", "Goose", "Crush"]))).expect("tracker starts");
        tracker.list_processes_with(Vec::new);
        tracker.set_enhanced_network(true);
        Fixture { files, now: Time::now(), history, tracker, sampler }
    }
    // Adds to a connection's byte counters, as traffic would.
    fn add_on(&self, received: u64, sent: u64, connection: &str, harness: &str, host: &str) {
        let previous = self.sampler.get(connection);
        self.sampler.push(sample(9001, harness, host, previous.as_ref().map_or(0, |sample| sample.received) + received, previous.as_ref().map_or(0, |sample| sample.sent) + sent, connection));
    }
    fn add(&self, received: u64, sent: u64, connection: &str) {
        self.add_on(received, sent, connection, "Aider", "api.anthropic.com");
    }
    fn try_poll(&self, time: f64) -> io::Result<()> {
        self.tracker.poll(Some(self.now.add_seconds(time)))
    }
    #[track_caller]
    fn poll(&self, time: f64) {
        self.try_poll(time).expect("poll succeeds");
    }
    #[track_caller]
    fn step(&self, time: f64, received: u64, sent: u64) {
        self.add(received, sent, "conn");
        self.poll(time);
    }
    fn try_close(&self, time: f64) -> io::Result<()> {
        self.sampler.remove("conn");
        self.try_poll(time)
    }
    #[track_caller]
    fn close(&self, time: f64) {
        self.try_close(time).expect("poll succeeds");
    }
    // A request followed by a response dense enough to be a stream.
    #[track_caller]
    fn stream(&self) {
        self.step(0.0, 0, 0);
        self.step(0.5, 0, 600);
        self.step(1.0, 1200, 0);
        self.step(1.5, 1200, 0);
        self.step(2.0, 1200, 0);
    }
    fn records(&self) -> Vec<Arc<RequestRecord>> {
        self.history.read(None).expect("history reads")
    }
    fn active(&self) -> Vec<LiveCall> {
        self.tracker.active()
    }
    // Makes history unwritable by putting a folder where the file belongs. Returns its path.
    fn block_history(&self) -> std::path::PathBuf {
        let blocker = self.history.home().join("history.jsonl");
        std::fs::create_dir_all(&blocker).unwrap();
        blocker
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.tracker.dispose();
    }
}

// ---- sparse pings and low-rate responses never become model calls -----------------------------

#[test]
fn sparse_receive_samples_stay_silent_and_unrecorded() {
    let fixture = Fixture::new("flow-sparse");
    fixture.step(0.0, 0, 0);
    fixture.step(0.5, 0, 600);
    for tick in 2..=9 {
        fixture.step(f64::from(tick) * 0.5, if matches!(tick, 2 | 5 | 8) { 600 } else { 0 }, 0);
        assert!(fixture.active().is_empty(), "sparse receive samples stay silent (tick {tick})");
    }
    fixture.close(5.0);
    assert!(fixture.records().is_empty(), "sparse pings do not pollute completed history");
    assert_eq!(fixture.tracker.held_rate(), None, "rejected pings do not replace held speed");
}

#[test]
fn dense_samples_below_400_bytes_per_second_are_not_generation() {
    let fixture = Fixture::new("flow-slow");
    fixture.step(0.0, 0, 0);
    fixture.step(0.1, 0, 600);
    fixture.step(0.5, 300, 0);
    fixture.step(0.8, 10, 0);
    fixture.step(1.1, 10, 0);
    assert!(fixture.active().is_empty(), "dense samples below 400 bytes/sec are not generation");
    fixture.close(2.0);
    assert!(fixture.records().is_empty(), "low-rate responses are not recorded");
}

#[test]
fn a_response_must_sustain_400_bytes_per_second_overall_to_enter_history() {
    let fixture = Fixture::new("flow-slow-finish");
    fixture.stream();
    for time in [3.5, 5.0, 6.5, 8.0, 9.5, 11.0] {
        fixture.step(time, 10, 0);
    }
    fixture.close(11.5);
    assert!(fixture.records().is_empty(), "an initially dense response that then trickles is not a recorded call");
}

#[test]
fn tiny_uploads_never_show_as_waiting() {
    let fixture = Fixture::new("flow-waiting");
    fixture.add(0, 0, "known");
    fixture.add_on(0, 0, "unknown", "Aider", "updates.example.test");
    fixture.poll(0.0);
    fixture.add(0, 320, "known");
    fixture.add_on(0, 320, "unknown", "Aider", "updates.example.test");
    fixture.poll(0.5);
    for time in [1.0, 10.0, 180.0, 601.0] {
        fixture.poll(time);
        assert!(fixture.active().is_empty(), "tiny uploads never expose unproven network waiting (time {time})");
    }
    assert!(fixture.records().is_empty(), "known and unknown housekeeping uploads create no records");
}

#[test]
fn a_receive_only_stream_needs_a_witnessed_request_upload() {
    let fixture = Fixture::new("flow-receive-only");
    fixture.step(0.0, 100_000, 0);
    fixture.step(0.5, 1200, 0);
    fixture.step(1.0, 1200, 0);
    fixture.step(1.5, 1200, 0);
    fixture.close(2.0);
    assert!(fixture.active().is_empty(), "no live call without a seen request");
    assert!(fixture.records().is_empty(), "and no record");
}

// ---- upload control bytes and the 300 ms grace cannot become TTFT -----------------------------

#[test]
fn upload_time_control_bytes_are_excluded_from_ttft_and_token_estimates() {
    let fixture = Fixture::new("flow-grace");
    fixture.step(0.0, 0, 0);
    fixture.step(0.5, 100, 1200);
    fixture.step(0.8, 100, 1200);
    fixture.step(1.0, 1000, 0);
    fixture.step(1.09, 1000, 0);
    fixture.step(1.11, 100, 0);
    fixture.step(1.31, 100, 0);
    fixture.step(1.51, 600, 0);
    fixture.step(1.91, 600, 0);
    assert!(fixture.active().is_empty(), "upload and early replies remain silent until stream density is proven");
    fixture.step(2.31, 600, 0);
    let live = fixture.active();
    assert_eq!(live.len(), 1, "a dense post-upload response becomes one live call");
    assert!(live[0].estimated, "and it is an explicit estimate");
    near(live[0].ttft, 1.01, "first response follows both upload grace and the 300-byte response threshold");
    fixture.close(2.5);
    let records = fixture.records();
    assert_eq!(records.len(), 1, "the stream is recorded once");
    near(records[0].ttft, 1.01, "persisted network TTFT excludes upload-time control traffic");
    near(records[0].generation, 0.8, "generation starts at the qualified first response, not TLS control bytes");
    // 2,000 bytes arrived after the grace period, at 22 bytes a token for an Anthropic host.
    assert_eq!(records[0].output_tokens, 91, "estimated output excludes every upload/grace control byte");
}

#[test]
fn a_multi_second_upload_keeps_its_original_request_start() {
    let fixture = Fixture::new("flow-long-upload");
    fixture.step(0.0, 0, 0);
    for time in [0.5, 1.5, 2.5, 3.5] {
        fixture.step(time, 100, 1200);
    }
    fixture.step(4.0, 1200, 0);
    fixture.step(4.5, 1200, 0);
    fixture.step(5.0, 1200, 0);
    fixture.close(5.5);
    let records = fixture.records();
    assert_eq!(records.len(), 1);
    near(records[0].ttft, 3.5, "a multi-second continuous upload retains its original request start");
    // 3,600 response bytes at 22 bytes a token.
    assert_eq!(records[0].output_tokens, 164, "a long upload's control traffic never enters generated-byte estimates");
}

// ---- keep-alive requests finish old calls and start the current upload ------------------------

#[test]
fn a_second_request_on_one_connection_is_a_separate_call() {
    // The second upload arrives either just after the first response went quiet, or well after.
    for second_upload in [2.1, 4.1] {
        let fixture = Fixture::new("flow-reuse");
        fixture.stream();
        fixture.step(second_upload, 1000, 600);
        assert_eq!(fixture.records().len(), 1, "a reused TCP upload finishes the earlier response (upload at {second_upload})");
        assert!(fixture.active().is_empty(), "the next unproven request does not inherit the old stream's live state");
        fixture.step(second_upload + 0.4, 1200, 0);
        fixture.step(second_upload + 0.9, 1200, 0);
        fixture.step(second_upload + 1.4, 1200, 0);
        let live = fixture.active();
        assert_eq!(live.len(), 1);
        near(live[0].ttft, 0.4, "reused-connection TTFT belongs to the new upload");
        fixture.close(second_upload + 1.6);
        let mut records = fixture.records();
        records.sort_by_key(|record| record.started_at);
        assert_eq!(records.len(), 2, "two requests on one connection yield two records");
        assert_ne!(records[0].id, records[1].id, "with distinct ids");
        assert_eq!(records[1].started_at, fixture.now.add_seconds(second_upload), "the upload arriving with old-call expiry is not lost");
        near(records[1].generation, 1.0, "the second generation is not merged into the first");
        assert_eq!(records[0].output_tokens, records[1].output_tokens, "new-upload control bytes do not contaminate the second response");
    }
}

#[test]
fn connection_ids_separate_streams_from_one_process_to_one_host() {
    let fixture = Fixture::new("flow-identity");
    fixture.add(0, 0, "one");
    fixture.add(0, 0, "two");
    fixture.poll(0.0);
    fixture.add(0, 600, "one");
    fixture.add(0, 600, "two");
    fixture.poll(0.5);
    for time in [1.0, 1.5, 2.0] {
        fixture.add(1200, 0, "one");
        fixture.add(1200, 0, "two");
        fixture.poll(time);
    }
    assert_eq!(fixture.active().len(), 2, "ConnectionID separates same-PID same-host TCP streams");
    fixture.sampler.remove("one");
    fixture.sampler.remove("two");
    fixture.poll(2.5);
    assert_eq!(fixture.records().len(), 2, "separate connection identities each persist their own request");
}

// ---- live speed holds through pauses and targets include observed harnesses -------------------

fn reloaded(fixture: &Fixture) -> Arc<Tracker> {
    Tracker::new(Arc::new(HistoryStore::new(Some(fixture.history.home()))), None, Some(Vec::new()), None, Some(installed(&[]))).expect("tracker restarts")
}

#[test]
fn live_target_catalog_holds_installed_and_observed_harnesses_and_survives_restart() {
    let fixture = Fixture::new("flow-target");
    let offered = fixture.tracker.harnesses();
    for harness in ["Gemini CLI", "Qwen Code", "Aider", "Goose", "Crush"] {
        assert!(offered.iter().any(|status| status.name == harness && status.is_installed), "installed network-only harness {harness} is available in the Live target catalog");
    }
    assert!(!offered.iter().any(|status| matches!(status.name.as_str(), "Pi" | "OMP")), "a harness that is not installed, logged or running is not in the Live target catalog");
    fixture.tracker.set_target(Some("Aider")).expect("an installed harness can be pinned");
    assert_eq!(reloaded(&fixture).target().as_deref(), Some("Aider"), "Aider target persists even before the first completed call");

    fixture.add_on(0, 0, "observed", "Observed Harness", "api.anthropic.com");
    fixture.poll(4.5);
    assert!(fixture.tracker.harnesses().iter().any(|status| status.name == "Observed Harness"), "observed sampler harnesses are added to Live targets");
    fixture.tracker.set_target(Some("Observed Harness")).expect("an observed harness can be pinned");
    let restarted = reloaded(&fixture);
    assert_eq!(restarted.target().as_deref(), Some("Observed Harness"), "observed target survives restart without a completed record");
    assert!(restarted.harnesses().iter().any(|status| status.name == "Observed Harness"), "and stays listed so it can be changed");
    assert!(restarted.set_target(Some("Not A Harness")).is_err(), "unobserved unknown targets remain rejected");
}

#[test]
fn live_rate_holds_through_a_pause_and_the_accepted_speed_stays_held_when_idle() {
    let fixture = Fixture::new("flow-hold");
    fixture.tracker.set_target(Some("Aider")).expect("an installed harness can be pinned");
    fixture.stream();
    let rate = fixture.active().first().and_then(|call| call.rate).expect("the stream has a live rate");
    fixture.poll(2.8);
    near(fixture.active().first().and_then(|call| call.rate), rate, "a short receive pause holds the measured live rate");
    fixture.poll(3.9);
    near(fixture.active().first().and_then(|call| call.rate), rate, "held live rate does not decay with wall-clock silence");
    fixture.poll(4.0);
    assert!(fixture.active().is_empty(), "two seconds of silence end the stream");
    assert!(fixture.tracker.held_rate().is_some_and(|held| held > 0.0), "ended streams leave their accepted speed held while idle");
}

// ---- consumed parser batches survive transient history failures -------------------------------

#[test]
fn parsed_log_records_are_kept_until_history_can_be_written() {
    let fixture = Fixture::with_sources("flow-log-recovery", |root| vec![LogSource::new("OMP", root.join("logs"))]);
    let rows: String = (1..=2)
        .map(|index| {
            let start = fixture.now.add_seconds(-30.0 + f64::from(index) * 10.0);
            let row = json!({
                "type": "message", "timestamp": iso(start.add_seconds(2.0)),
                "message": {"role": "assistant", "model": "claude-opus-4", "provider": "anthropic", "timestamp": iso(start), "duration": 2000, "ttft": 250, "stopReason": "stop", "responseId": format!("recover_{index}"), "usage": {"output": 400, "input": 50}}
            });
            format!("{row}\n")
        })
        .collect();
    fixture.files.put("logs/session.jsonl", rows);
    let blocker = fixture.block_history();
    assert!(fixture.try_poll(0.0).is_err(), "history persistence failures are surfaced");
    std::fs::remove_dir(&blocker).unwrap();
    fixture.poll(0.5);
    let recovered: BTreeSet<String> = fixture.records().iter().filter_map(|record| record.source_key.clone()).collect();
    assert_eq!(recovered, BTreeSet::from(["omp:recover_1".to_string(), "omp:recover_2".to_string()]), "the failed record and remaining consumed parser batch are retried");
    fixture.poll(1.0);
    assert_eq!(fixture.records().len(), 2, "recovered batches are committed exactly once");
}

#[test]
fn a_finished_network_call_survives_a_history_failure() {
    let fixture = Fixture::new("flow-network-recovery");
    fixture.stream();
    let blocker = fixture.block_history();
    assert!(fixture.try_close(2.5).is_err(), "network history failures are surfaced");
    assert!(fixture.active().is_empty(), "failed persistence does not leave an ended network call live");
    std::fs::remove_dir(&blocker).unwrap();
    fixture.poll(3.0);
    assert_eq!(fixture.records().len(), 1, "the finished call is written once history is writable again");
    assert!(fixture.tracker.held_rate().is_some_and(|held| held > 0.0), "and restores held metrics");
    fixture.poll(3.5);
    assert_eq!(fixture.records().len(), 1, "recovered network calls are not duplicated");
}

// ---- explicit proxy live updates merge with passive collection and retry history --------------

#[test]
fn proxy_calls_merge_with_passive_live_state_and_share_the_recoverable_history_queue() {
    let fixture = Fixture::new("flow-proxy");
    fixture.stream();
    let changed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&changed);
    fixture.tracker.on_changed(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let call = LiveCall { id: "proxy-regression".into(), harness: "Aider".into(), model: "exact-model".into(), provider: "anthropic".into(), phase: "Waiting".into(), started_at: fixture.now, last_activity: fixture.now, ttft: None, rate: None, output_tokens: None, estimated: false };
    fixture.tracker.update_proxy(call.clone());
    assert!(changed.load(Ordering::Relaxed) > 0, "proxy updates publish immediately");
    assert_eq!(fixture.active().len(), 2, "without replacing passive live calls");
    fixture.poll(2.1);
    let live = fixture.active();
    assert!(live.iter().any(|known| known.id == call.id) && live.iter().any(|known| known.id.starts_with("network:")), "proxy live state remains merged across passive polls");
    fixture.tracker.update_proxy(LiveCall { phase: "Streaming".into(), ttft: Some(0.5), rate: Some(50.0), output_tokens: Some(100), ..call.clone() });
    near(fixture.active().iter().find(|known| known.id == call.id).and_then(|known| known.rate), 50.0, "proxy updates expose exact current speed immediately");

    let record = RequestRecord {
        started_at: fixture.now,
        harness: "Aider".into(),
        model: "exact-model".into(),
        upstream_host: "api.anthropic.com".into(),
        streamed: true,
        ttft: Some(0.5),
        generation: Some(2.0),
        total: 2.5,
        output_tokens: 100,
        tps: Some(50.0),
        source: Some("proxy".into()),
        source_key: Some(call.id.clone()),
        ..RequestRecord::default()
    };
    let blocker = fixture.block_history();
    assert!(fixture.tracker.finish_proxy(&call.id, Some(record)).is_err(), "proxy history failures are surfaced");
    assert!(!fixture.active().iter().any(|known| known.id == call.id), "proxy finish publishes the removed call even if history is temporarily blocked");
    std::fs::remove_dir(&blocker).unwrap();
    fixture.poll(2.2);
    let stored = |fixture: &Fixture| fixture.records().iter().filter(|record| record.source_key.as_deref() == Some("proxy-regression")).count();
    assert_eq!(stored(&fixture), 1, "proxy completion uses the same recoverable persistence queue");
    near(fixture.tracker.held_rate(), 50.0, "accepted exact proxy metrics become held history");
    fixture.tracker.finish_proxy("cancelled-proxy", None).expect("a cancelled call writes nothing");
    assert_eq!(stored(&fixture), 1, "proxy cancellation without an accepted record invents no completion");
}

// ---- a harness with a session log: traffic shows the reply live, the log records it -------------

fn claude_fixture(label: &str) -> Fixture {
    Fixture::with_sources(label, |root| vec![LogSource::new("Claude Code", root.join("logs"))])
}

// The request-then-dense-reply pattern of `Fixture::stream`, on a Claude Code connection.
#[track_caller]
fn claude_stream(fixture: &Fixture) {
    for (time, received, sent) in [(0.0, 0, 0), (0.5, 0, 600), (1.0, 1200, 0), (1.5, 1200, 0), (2.0, 1200, 0)] {
        fixture.add_on(received, sent, "claude-conn", "Claude Code", "api.anthropic.com");
        fixture.poll(time);
    }
}

#[test]
fn a_log_covered_harness_shows_its_reply_arriving_from_traffic_and_is_recorded_only_from_its_log() {
    let fixture = claude_fixture("flow-covered-live");
    // The log keeps milliseconds, so the request is dated on a whole one.
    let asked = Time::from_unix_ms(common::ms(fixture.now.add_seconds(-0.5)));
    let log = fixture.files.put("logs/p1/session.jsonl", format!("{}\n", json!({"type": "user", "timestamp": iso(asked), "message": {"role": "user", "content": "hello"}})));
    claude_stream(&fixture);
    let live = fixture.active();
    assert_eq!(live.len(), 1, "one call: the log's, not a second one invented from traffic");
    assert_eq!((live[0].harness.as_str(), live[0].phase.as_str(), live[0].started_at), ("Claude Code", "Streaming · network estimate", asked), "the call the log says is due, now seen streaming");
    assert!(live[0].estimated && !live[0].id.starts_with("network:"), "marked as an estimate on the log's own call");
    // The first reply bytes came at 1.0 s; the log dates the request about 0.5 s before zero (not 0.5 s
    // after it, where the upload was seen).
    near(live[0].ttft, fixture.now.add_seconds(1.0).since(asked), "time to first byte is counted from the request in the log");
    assert!(live[0].ttft.is_some_and(|ttft| (1.5..1.502).contains(&ttft)), "which is 1.5 s, to the millisecond the log keeps: {:?}", live[0].ttft);
    // 3,600 bytes in the second after the first one, at 22 bytes a token for an Anthropic host.
    near(live[0].rate, 3600.0 / 22.0, "speed estimated from the bytes arriving");
    assert_eq!(live[0].output_tokens, Some(163));

    // The reply lands in the log; the connection then closes.
    common::append(
        &log,
        format!("{}\n", json!({"type": "assistant", "timestamp": iso(fixture.now.add_seconds(2.1)), "message": {"id": "msg_joined", "model": "claude-opus-5-5", "usage": {"output_tokens": 250, "input_tokens": 40}, "content": [{"type": "text"}]}})),
    );
    fixture.poll(2.5);
    assert!(fixture.active().is_empty(), "with no reply due in the log the stream is no longer shown");

    // A tool result sends the next request while the last bytes of the old reply are still arriving.
    common::append(&log, format!("{}\n", json!({"type": "user", "timestamp": iso(fixture.now.add_seconds(2.6)), "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1"}]}})));
    fixture.add_on(1200, 0, "claude-conn", "Claude Code", "api.anthropic.com");
    fixture.poll(3.0);
    let next = fixture.active();
    assert_eq!(next.iter().map(|call| (call.phase.as_str(), call.rate, call.estimated)).collect::<Vec<_>>(), [("Waiting", None, false)], "a stream that began before a request is not that request's reply");

    fixture.sampler.remove("claude-conn");
    fixture.poll(6.0);
    let records = fixture.records();
    assert_eq!(records.iter().map(|record| (record.source.as_deref(), record.source_key.as_deref(), record.output_tokens)).collect::<Vec<_>>(), [(Some("log"), Some("claude:msg_joined"), 250)], "recorded once, from the log, with the log's exact token count");
}

#[test]
fn a_log_covered_harness_streaming_with_no_reply_due_in_its_log_is_not_activity() {
    let fixture = claude_fixture("flow-covered-idle");
    let earlier = fixture.now.add_seconds(-60.0);
    fixture.files.put(
        "logs/p1/session.jsonl",
        common::lines(&[
            json!({"type": "user", "timestamp": iso(earlier), "message": {"role": "user", "content": "hello"}}),
            json!({"type": "assistant", "timestamp": iso(earlier.add_seconds(4.0)), "message": {"id": "msg_done", "model": "claude-opus-5-5", "usage": {"output_tokens": 80, "input_tokens": 40}, "content": [{"type": "text"}]}}),
            json!({"type": "system", "timestamp": iso(earlier.add_seconds(4.0))}),
        ]),
    );
    for (time, received, sent) in [(0.0, 0, 0), (0.5, 0, 600), (1.0, 1200, 0), (1.5, 1200, 0), (2.0, 1200, 0), (2.5, 1200, 0)] {
        fixture.add_on(received, sent, "claude-conn", "Claude Code", "api.anthropic.com");
        fixture.poll(time);
        assert!(fixture.active().is_empty(), "at {time} s: housekeeping on a harness's connection is not a model call");
    }
    fixture.sampler.remove("claude-conn");
    fixture.poll(5.0);
    assert_eq!(fixture.records().iter().map(|record| record.source_key.clone()).collect::<Vec<_>>(), [Some("claude:msg_done".to_string())], "only the logged call is in history; the traffic adds none");
}
