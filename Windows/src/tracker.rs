use crate::antigravity::AntigravityLogWatcher;
use crate::discovery::{self, LogSource};
use crate::domain::{
    DashboardSummary, FlowSample, FlowSampler, HarnessStatus, LiveCall, ProviderIdentity,
    RequestRecord,
};
use crate::harness;
use crate::history::HistoryStore;
use crate::logs::{error_name, SessionLogWatcher};
use crate::opencode::OpenCodeLogWatcher;
use crate::parsers::LogRecord;
use crate::processes;
use crate::time::Time;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use uuid::Uuid;

const REQUEST_BYTES: u64 = 300;
const RESPONSE_BYTES: u64 = 300;
// Reply bytes this soon after an upload are flow control for the upload, not the response.
const UPLOAD_GRACE: f64 = 0.3;
const STREAM_BYTES_PER_SECOND: f64 = 400.0;
const NO_TELEMETRY: &str =
    "No readable supported session telemetry found. Processes alone do not activate tracking.";
const UNKNOWN_TARGET: &str = "Unknown live harness target.";

pub type InstalledScan = Box<dyn Fn() -> BTreeSet<String> + Send + Sync>;
pub type ProcessScan = Box<dyn Fn() -> Vec<String> + Send + Sync>;

fn valid_harness_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= 128
        && name == name.trim()
        && !name.chars().any(char::is_control)
}

#[derive(Serialize, Deserialize)]
struct LiveTargetPreference {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default)]
    harnesses: Option<Vec<String>>,
}

// What the UI reads. Never held while files or the network are touched.
struct Shared {
    active: Vec<LiveCall>,
    harnesses: Vec<HarnessStatus>,
    known_harnesses: BTreeSet<String>,
    // Newest first.
    recent: Vec<Arc<RequestRecord>>,
    target: Option<String>,
    network_enabled: bool,
    poll_error: Option<String>,
    disposed: bool,
}

// Everything a poll works on. Held for the whole poll, so polls and proxy updates never interleave.
struct PollState {
    logs: SessionLogWatcher,
    open_code: OpenCodeLogWatcher,
    antigravity: AntigravityLogWatcher,
    flows: HashMap<String, NetworkFlow>,
    proxy_calls: IndexMap<String, LiveCall>,
    // Finished calls not yet written to history.
    pending: VecDeque<RequestRecord>,
    process_scan: Time,
    running: HashSet<String>,
    // Harnesses whose command or folder exists on this machine. Looked up at start and once a minute.
    installed: BTreeSet<String>,
    presence_scan: Time,
    // When the network sampler last saw each harness's process: a harness seen moments ago is on this machine.
    sampled: HashMap<String, Time>,
}

impl PollState {
    fn has_logs(&self, harness: &str) -> bool {
        match harness {
            "opencode" => self.open_code.has_logs(),
            "Antigravity" => self.antigravity.has_logs(),
            other => self.logs.has_logs(other),
        }
    }
}

/// Combines the passive sources into live state and finished-call history.
pub struct Tracker {
    history: Arc<HistoryStore>,
    sampler: Option<Arc<dyn FlowSampler>>,
    // Reports which harnesses are installed; tests replace it.
    find_installed: InstalledScan,
    // Names the running processes; tests replace it.
    list_processes: Mutex<ProcessScan>,
    shared: Mutex<Shared>,
    poll: Mutex<PollState>,
    listeners: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    stop: Arc<AtomicBool>,
    timer: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Tracker {
    pub fn new(
        history: Arc<HistoryStore>,
        sampler: Option<Arc<dyn FlowSampler>>,
        sources: Option<Vec<LogSource>>,
        user_home: Option<PathBuf>,
        find_installed: Option<InstalledScan>,
    ) -> io::Result<Arc<Tracker>> {
        let home = user_home.clone();
        let find_installed = find_installed.unwrap_or_else(|| {
            Box::new(move || harness::installed(&discovery::homes(home.as_deref()), None))
        });
        // Before anything else, see which harnesses this machine has, so the first list offered holds only those.
        let installed = find_installed();
        let discovered =
            sources.unwrap_or_else(|| discovery::default_sources(user_home.as_deref()));
        let mut recent = history.read(None)?;
        recent.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut known: BTreeSet<String> = [
            "claude", "codex", "omp", "pi", "opencode", "dsh", "gemini", "qwen", "aider", "goose",
            "crush",
        ]
        .iter()
        .filter_map(|command| harness::classify(command, None))
        .map(str::to_string)
        .collect();
        known.extend(
            discovered
                .iter()
                .map(|source| source.harness.clone())
                .chain(recent.iter().map(|record| record.harness.clone()))
                .filter(|name| valid_harness_name(name)),
        );

        let mut target = None;
        let preference = history.home().join("live-target.json");
        if std::fs::metadata(&preference).is_ok_and(|metadata| metadata.len() <= 16_384) {
            // Older versions stored just the name; newer ones also remember which harnesses had been seen.
            match std::fs::read_to_string(&preference)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            {
                Some(serde_json::Value::String(name)) => target = Some(name),
                Some(value @ serde_json::Value::Object(_)) => {
                    if let Ok(saved) = serde_json::from_value::<LiveTargetPreference>(value) {
                        known.extend(
                            saved
                                .harnesses
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|name| valid_harness_name(name)),
                        );
                        target = saved.target;
                    }
                }
                _ => {}
            }
            target = target.filter(|name| known.contains(name));
        }
        // Session logs have not been read yet, so the first list is what is installed. A pinned target stays listed so it can be changed.
        let harnesses = known
            .iter()
            .filter(|name| installed.contains(*name) || Some(*name) == target.as_ref())
            .map(|name| HarnessStatus {
                name: name.clone(),
                has_logs: false,
                is_running: false,
                limitation: Some(NO_TELEMETRY.into()),
                is_installed: installed.contains(name),
            })
            .collect();

        let tracker = Arc::new(Tracker {
            sampler,
            find_installed,
            list_processes: Mutex::new(Box::new(processes::executables)),
            shared: Mutex::new(Shared {
                active: Vec::new(),
                harnesses,
                known_harnesses: known,
                recent,
                target,
                network_enabled: false,
                poll_error: None,
                disposed: false,
            }),
            poll: Mutex::new(PollState {
                logs: SessionLogWatcher::new(&discovered),
                open_code: OpenCodeLogWatcher::new(
                    discovered
                        .iter()
                        .filter(|source| source.harness == "opencode")
                        .map(|source| source.root.clone()),
                ),
                antigravity: AntigravityLogWatcher::new(
                    discovered
                        .iter()
                        .filter(|source| source.harness == "Antigravity")
                        .map(|source| source.root.clone()),
                ),
                flows: HashMap::new(),
                proxy_calls: IndexMap::new(),
                pending: VecDeque::new(),
                process_scan: Time::MIN,
                running: HashSet::new(),
                installed,
                presence_scan: Time::now(),
                sampled: HashMap::new(),
            }),
            listeners: Mutex::default(),
            stop: Arc::new(AtomicBool::new(false)),
            timer: Mutex::new(None),
            history: Arc::clone(&history),
        });
        let weak: Weak<Tracker> = Arc::downgrade(&tracker);
        history.on_recorded(move |record| {
            if let Some(tracker) = weak.upgrade() {
                tracker.on_recorded(record);
            }
        });
        Ok(tracker)
    }

    /// Replaces how running processes are listed, so tests describe a machine instead of reading the real one.
    pub fn list_processes_with(&self, list: impl Fn() -> Vec<String> + Send + Sync + 'static) {
        *self.list_processes.lock().unwrap() = Box::new(list);
    }

    /// Calls `listener` whenever live state, the harness list or the history changes. It may run on any thread.
    pub fn on_changed(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(listener));
    }

    fn changed(&self) {
        for listener in self.listeners.lock().unwrap().iter() {
            listener();
        }
    }

    pub fn active(&self) -> Vec<LiveCall> {
        self.shared.lock().unwrap().active.clone()
    }
    pub fn harnesses(&self) -> Vec<HarnessStatus> {
        self.shared.lock().unwrap().harnesses.clone()
    }
    /// The newest finished call for the Live target.
    pub fn held_record(&self) -> Option<Arc<RequestRecord>> {
        let shared = self.shared.lock().unwrap();
        let held = shared.selected().next().cloned();
        held
    }
    pub fn held_rate(&self) -> Option<f64> {
        self.shared
            .lock()
            .unwrap()
            .last_speed()
            .and_then(|record| record.tps)
    }
    pub fn held_ttft(&self) -> Option<f64> {
        self.shared
            .lock()
            .unwrap()
            .last_latency()
            .and_then(|record| record.ttft)
    }
    pub fn held_rate_estimated(&self) -> bool {
        self.shared
            .lock()
            .unwrap()
            .last_speed()
            .is_some_and(|record| record.tokens_estimated)
    }
    pub fn held_ttft_estimated(&self) -> bool {
        self.shared
            .lock()
            .unwrap()
            .last_latency()
            .is_some_and(|record| {
                record.tokens_estimated || record.source.as_deref() == Some("network")
            })
    }
    pub fn enhanced_network_available(&self) -> bool {
        self.sampler
            .as_ref()
            .is_some_and(|sampler| sampler.available())
    }
    pub fn enhanced_network_enabled(&self) -> bool {
        self.shared.lock().unwrap().network_enabled && self.enhanced_network_available()
    }
    pub fn network_status(&self) -> String {
        let shared = self.shared.lock().unwrap();
        if let Some(error) = &shared.poll_error {
            return error.clone();
        }
        if !shared.network_enabled {
            return "Standard-user log-first tracking. Live timing is absent where a harness does not publish it. Enhanced network collection is off.".into();
        }
        self.sampler
            .as_ref()
            .map(|sampler| sampler.status())
            .unwrap_or_else(|| {
                "Enhanced network collection is unavailable on this platform.".into()
            })
    }
    /// The harness Live follows; None is Auto.
    pub fn target(&self) -> Option<String> {
        self.shared.lock().unwrap().target.clone()
    }

    pub fn set_target(&self, value: Option<&str>) -> Result<(), String> {
        if value.is_some_and(|name| !valid_harness_name(name)) {
            return Err(UNKNOWN_TARGET.into());
        }
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.disposed {
                return Err("Speed Tracker is shutting down.".into());
            }
            if value.is_some_and(|name| !shared.known_harnesses.contains(name)) {
                return Err(UNKNOWN_TARGET.into());
            }
            if shared.target.as_deref() == value {
                return Ok(());
            }
            let preference = LiveTargetPreference {
                target: value.map(str::to_string),
                harnesses: Some(shared.known_harnesses.iter().cloned().collect()),
            };
            let home = self.history.home();
            let path = home.join("live-target.json");
            // Written beside the real file and moved over it, so a crash never leaves half a preference.
            let temporary = home.join(format!("live-target.json.{}.tmp", Uuid::new_v4().simple()));
            let written = std::fs::create_dir_all(home)
                .and_then(|_| {
                    std::fs::write(
                        &temporary,
                        serde_json::to_string(&preference).unwrap_or_default(),
                    )
                })
                .and_then(|_| std::fs::rename(&temporary, &path));
            if let Err(error) = written {
                let _ = std::fs::remove_file(&temporary);
                return Err(error.to_string());
            }
            shared.target = value.map(str::to_string);
        }
        self.changed();
        Ok(())
    }

    pub fn recent(&self, count: usize, harness: Option<&str>) -> Vec<Arc<RequestRecord>> {
        self.shared
            .lock()
            .unwrap()
            .recent
            .iter()
            .filter(|record| harness.is_none_or(|harness| record.harness == harness))
            .take(count)
            .cloned()
            .collect()
    }

    fn on_recorded(&self, record: &Arc<RequestRecord>) {
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.disposed {
                return;
            }
            if valid_harness_name(&record.harness) {
                shared.known_harnesses.insert(record.harness.clone());
            }
            let position = shared
                .recent
                .iter()
                .position(|known| known.started_at < record.started_at)
                .unwrap_or(shared.recent.len());
            shared.recent.insert(position, Arc::clone(record));
        }
        self.changed();
    }

    /// Polls twice a second on a background thread until the tracker is disposed.
    pub fn start(self: &Arc<Self>) {
        let mut timer = self.timer.lock().unwrap();
        if timer.is_some() || self.shared.lock().unwrap().disposed {
            return;
        }
        let weak = Arc::downgrade(self);
        let stop = Arc::clone(&self.stop);
        *timer = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Some(tracker) = weak.upgrade() else { break };
                tracker.timer_poll();
                drop(tracker);
                std::thread::sleep(Duration::from_millis(500));
            }
        }));
    }

    fn timer_poll(&self) {
        if let Err(error) = self.poll(None) {
            self.shared.lock().unwrap().poll_error = Some(format!(
                "Telemetry/history error: {}. Completed history was not silently discarded.",
                error_name(&error)
            ));
            self.changed();
        }
    }

    pub fn set_enhanced_network(&self, enabled: bool) {
        {
            let mut poll = self.poll.lock().unwrap();
            let mut shared = self.shared.lock().unwrap();
            if shared.disposed {
                return;
            }
            shared.network_enabled = enabled && self.enhanced_network_available();
            if !shared.network_enabled {
                shared
                    .active
                    .retain(|call| !call.id.starts_with("network:"));
            }
            poll.flows.clear();
        }
        self.changed();
    }

    /// Shows a call the optional proxy is measuring.
    pub fn update_proxy(&self, call: LiveCall) {
        {
            let mut poll = self.poll.lock().unwrap();
            let mut shared = self.shared.lock().unwrap();
            if shared.disposed {
                return;
            }
            poll.proxy_calls.insert(call.id.clone(), call.clone());
            if valid_harness_name(&call.harness) {
                shared.known_harnesses.insert(call.harness.clone());
            }
            shared.active.retain(|known| known.id != call.id);
            shared.active.push(call);
            shared
                .active
                .sort_by(|a, b| b.started_at.cmp(&a.started_at));
        }
        self.changed();
    }

    /// Ends a proxy call. A None record means it was not a generation worth recording.
    pub fn finish_proxy(&self, id: &str, record: Option<RequestRecord>) -> io::Result<()> {
        let result = {
            let mut poll = self.poll.lock().unwrap();
            {
                let mut shared = self.shared.lock().unwrap();
                if shared.disposed {
                    return Ok(());
                }
                poll.proxy_calls.shift_remove(id);
                shared.active.retain(|call| call.id != id);
            }
            poll.pending.extend(record);
            self.persist_pending(&mut poll)
        };
        self.changed();
        result
    }

    /// One pass over every source. `now` is passed by tests; the timer uses the clock.
    pub fn poll(&self, now: Option<Time>) -> io::Result<()> {
        let wall = now.unwrap_or_else(Time::now);
        let changed;
        {
            let mut guard = self.poll.lock().unwrap();
            let poll = &mut *guard;
            if self.shared.lock().unwrap().disposed {
                return Ok(());
            }
            let mut observed = Vec::new();
            for records in [
                poll.logs.poll(wall),
                poll.open_code.poll(wall),
                poll.antigravity.poll(wall),
            ] {
                accept_logs(records, wall, &mut poll.pending, &mut observed);
            }
            let mut live: Vec<LiveCall> = poll.logs.active(wall);
            live.extend(poll.open_code.active(wall));
            live.extend(poll.antigravity.active(wall));
            live.extend(poll.proxy_calls.values().cloned());
            if self.enhanced_network_enabled() {
                if let Some(sampler) = &self.sampler {
                    poll_network(poll, sampler.sample(), wall, &mut live, &mut observed);
                }
            }
            if wall.since(poll.process_scan) >= 4.0 {
                poll.running.clear();
                let running = (self.list_processes.lock().unwrap())();
                for name in running {
                    if let Some(harness) = harness::classify(&name, None) {
                        poll.running.insert(harness.to_string());
                        observed.push(harness.to_string());
                    }
                }
                poll.process_scan = wall;
            }
            // A harness can be installed while the app is open, so look again once a minute.
            if wall.since(poll.presence_scan).abs() >= 60.0 {
                poll.installed = (self.find_installed)();
                poll.presence_scan = wall;
            }
            let (names, pinned) = {
                let mut shared = self.shared.lock().unwrap();
                shared
                    .known_harnesses
                    .extend(observed.into_iter().filter(|name| valid_harness_name(name)));
                (shared.known_harnesses.clone(), shared.target.clone())
            };
            // Only harnesses present on this machine are offered: installed, keeping readable session
            // logs, or running right now. One that is none of these cannot be chosen. The pinned target
            // stays listed so it can be changed.
            let statuses: Vec<HarnessStatus> = names
                .into_iter()
                .map(|name| {
                    let has_logs = poll.has_logs(&name);
                    let source_error = match name.as_str() {
                        "opencode" => poll.open_code.limitation().map(str::to_string),
                        "Antigravity" => poll.antigravity.limitation().map(str::to_string),
                        other => poll.logs.errors().get(other).cloned(),
                    };
                    let seen = poll.sampled.get(&name).is_some_and(|sampled| wall.since(*sampled).abs() < 120.0);
                    let limitation = source_error.unwrap_or_else(|| if has_logs { "Passive session logs. Metrics not exposed by this source stay unknown, never zero; provider labels are unverified.".into() } else { NO_TELEMETRY.into() });
                    HarnessStatus { has_logs, is_running: poll.running.contains(&name) || seen || live.iter().any(|call| call.harness == name), limitation: Some(limitation), is_installed: poll.installed.contains(&name), name }
                })
                .filter(|status| status.has_logs || status.is_running || status.is_installed || Some(&status.name) == pinned.as_ref())
                .collect();
            live.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            {
                let mut shared = self.shared.lock().unwrap();
                changed = shared.active != live
                    || shared.harnesses != statuses
                    || shared.poll_error.is_some();
                shared.active = live;
                shared.harnesses = statuses;
                shared.poll_error = None;
            }
            self.persist_pending(poll)?;
        }
        if changed {
            self.changed();
        }
        Ok(())
    }

    fn persist_pending(&self, poll: &mut PollState) -> io::Result<()> {
        while let Some(record) = poll.pending.front() {
            // Parser offsets and flow state already advanced. Keep this item and the
            // rest of the batch until persistence succeeds or confirms a duplicate.
            self.history.append(record.clone())?;
            poll.pending.pop_front();
        }
        Ok(())
    }

    /// Stops polling and releases every source. Safe to call more than once.
    pub fn dispose(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut poll = self.poll.lock().unwrap();
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.disposed {
                return;
            }
            shared.disposed = true;
            shared.active.clear();
        }
        poll.flows.clear();
        poll.proxy_calls.clear();
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Shared {
    fn selected(&self) -> impl Iterator<Item = &Arc<RequestRecord>> {
        self.recent.iter().filter(|record| {
            self.target
                .as_ref()
                .is_none_or(|target| record.harness == *target)
        })
    }
    fn last_speed(&self) -> Option<&Arc<RequestRecord>> {
        self.selected().find(|record| {
            !record.aborted
                && record.status < 400
                && record
                    .tps
                    .is_some_and(|rate| rate.is_finite() && rate >= 0.0)
        })
    }
    fn last_latency(&self) -> Option<&Arc<RequestRecord>> {
        self.selected()
            .find(|record| DashboardSummary::valid_latency(record))
    }
}

fn accept_logs(
    mut records: Vec<LogRecord>,
    wall: Time,
    pending: &mut VecDeque<RequestRecord>,
    observed: &mut Vec<String>,
) {
    records.sort_by_key(|record| record.end);
    for record in records {
        if record.end >= wall.add_days(-7.0) && record.end <= wall.add_minutes(1.0) {
            observed.push(record.harness.clone());
            pending.push_back(record.make_record());
        }
    }
}

// Tokens are estimated from bytes; Anthropic's stream spends far fewer bytes per token than the others.
fn bytes_per_token(sample: &FlowSample) -> f64 {
    let identity = ProviderIdentity::from(&RequestRecord {
        upstream_host: sample.host.clone(),
        ..RequestRecord::default()
    });
    if identity.key == "anthropic" {
        22.0
    } else {
        180.0
    }
}

struct NetworkFlow {
    previous: FlowSample,
    id: Uuid,
    token_ratio: f64,
    start: Option<Time>,
    first: Option<Time>,
    last_upload: Option<Time>,
    last: Time,
    bytes: u64,
    sustained: bool,
    recent: VecDeque<(Time, u64)>,
    // The harness keeps a session log, which records its calls; this traffic only shows them live.
    covered: bool,
}

// A stream seen on the connection of a harness whose log names the call it belongs to.
struct CoveredStream {
    harness: String,
    first: Time,
    last: Time,
    rate: f64,
    tokens: f64,
}

impl NetworkFlow {
    fn new(sample: FlowSample) -> NetworkFlow {
        NetworkFlow {
            token_ratio: bytes_per_token(&sample),
            previous: sample,
            id: Uuid::nil(),
            start: None,
            first: None,
            last_upload: None,
            last: Time(0),
            bytes: 0,
            sustained: false,
            recent: VecDeque::new(),
            covered: false,
        }
    }
    // A request was seen, a reply began, and data then flowed steadily for at least half a second.
    fn stream(&self) -> Option<(Time, Time)> {
        match (self.start, self.first) {
            (Some(start), Some(first)) if self.sustained && self.last.since(first) >= 0.5 => {
                Some((start, first))
            }
            _ => None,
        }
    }
    fn begin(&mut self, now: Time) {
        self.start = Some(now);
        self.id = Uuid::new_v4();
        self.last = now;
    }
    // Whether the reply has become a sustained stream: data in most samples of the last 1.3 s, fast enough to be text.
    fn qualify(&mut self, now: Time, received: u64) {
        self.recent.push_back((now, received));
        while self
            .recent
            .front()
            .is_some_and(|(time, _)| now.since(*time) > 1.3)
        {
            self.recent.pop_front();
        }
        let Some(&(start, _)) = self.recent.front() else {
            return;
        };
        let duration = now.since(start);
        if duration < 0.6 {
            return;
        }
        let hits = self.recent.iter().filter(|(_, bytes)| *bytes > 0).count();
        // The oldest sample's bytes arrived before the window it opens.
        let bytes: u64 = self.recent.iter().skip(1).map(|(_, bytes)| *bytes).sum();
        self.sustained = hits >= 3
            && hits as f64 >= 0.75 * self.recent.len() as f64
            && bytes as f64 / duration >= STREAM_BYTES_PER_SECOND;
    }
    fn reset_response(&mut self) {
        self.first = None;
        self.bytes = 0;
        self.sustained = false;
        self.recent.clear();
    }
    fn reset(&mut self) {
        self.start = None;
        self.last_upload = None;
        self.reset_response();
    }
}

fn finish_network(flow: &NetworkFlow, pending: &mut VecDeque<RequestRecord>) {
    // A harness with a readable log is recorded from the log; its traffic is not measured twice.
    if flow.covered {
        return;
    }
    let Some((start, first)) = flow.stream() else {
        return;
    };
    let sample = &flow.previous;
    let generation = flow.last.since(first);
    if flow.bytes as f64 / generation < STREAM_BYTES_PER_SECOND {
        return;
    }
    let tokens = (flow.bytes as f64 / flow.token_ratio)
        .round_ties_even()
        .min(i32::MAX as f64) as i32;
    if tokens < 5 {
        return;
    }
    let ttft = first.since(start);
    pending.push_back(RequestRecord {
        id: flow.id,
        started_at: start,
        harness: sample.harness.clone(),
        route: sample.host.clone(),
        upstream_host: sample.host.clone(),
        model: "unknown".into(),
        streamed: true,
        ttft: Some(ttft),
        ttfb: Some(ttft),
        generation: Some(generation),
        total: flow.last.since(start),
        output_tokens: tokens,
        tokens_estimated: true,
        tps: Some(tokens as f64 / generation),
        source: Some("network".into()),
        source_key: Some(format!("network:{}", flow.id)),
        ..RequestRecord::default()
    });
}

fn poll_network(
    poll: &mut PollState,
    samples: Vec<FlowSample>,
    now: Time,
    live: &mut Vec<LiveCall>,
    observed: &mut Vec<String>,
) {
    let mut seen = HashSet::new();
    let mut streams: Vec<CoveredStream> = Vec::new();
    for sample in samples {
        if !valid_harness_name(&sample.harness) {
            continue;
        }
        observed.push(sample.harness.clone());
        poll.sampled.insert(sample.harness.clone(), now);
        let key = sample
            .connection_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", sample.pid, sample.host));
        seen.insert(key.clone());
        let covered = poll.has_logs(&sample.harness);
        let Some(flow) = poll.flows.get_mut(&key) else {
            let mut flow = NetworkFlow::new(sample);
            flow.covered = covered;
            poll.flows.insert(key, flow);
            continue;
        };
        // Counters that went backwards, or a different process or host, mean the connection was reused.
        if sample.received < flow.previous.received
            || sample.sent < flow.previous.sent
            || sample.pid != flow.previous.pid
            || sample.host != flow.previous.host
            || sample.harness != flow.previous.harness
        {
            finish_network(flow, &mut poll.pending);
            *flow = NetworkFlow::new(sample);
            flow.covered = covered;
            continue;
        }
        let sent = sample.sent - flow.previous.sent;
        let received = sample.received - flow.previous.received;
        flow.previous = sample;
        flow.covered = covered;

        // Expire the old response before applying this sample's upload, so a
        // keep-alive request arriving after a pause is not consumed by Reset.
        if flow.start.is_some_and(|start| {
            (flow.sustained && now.since(flow.last) >= 2.0)
                || (now.since(start) > 180.0 && now.since(flow.last) > 90.0)
        }) {
            finish_network(flow, &mut poll.pending);
            flow.reset();
        }
        let uploading = sent >= REQUEST_BYTES;
        if uploading {
            if let Some(first) = flow.first {
                if !flow.sustained && flow.bytes < 8000 && now.since(first) < 1.0 {
                    // An early small reply followed by more upload was a handshake.
                    flow.reset_response();
                } else {
                    finish_network(flow, &mut poll.pending);
                    flow.reset();
                }
            } else if flow.start.is_some()
                && (flow
                    .last_upload
                    .is_some_and(|upload| now.since(upload) >= 3.0)
                    || (flow.bytes > 0 && now.since(flow.last) > 1.0))
            {
                flow.reset();
            }
            if flow.start.is_none() {
                flow.begin(now);
            }
            flow.last_upload = Some(now);
        }
        let control = uploading
            || flow
                .last_upload
                .is_some_and(|upload| now.since(upload) < UPLOAD_GRACE);
        if flow.start.is_some() && received > 0 && !control {
            flow.last = now;
            flow.bytes += received;
            if flow.first.is_none() && flow.bytes >= RESPONSE_BYTES {
                flow.first = Some(now);
            }
        }
        if flow.first.is_some() && !flow.sustained {
            flow.qualify(now, if control { 0 } else { received });
        }
        // Network-only traffic is never enough to claim Waiting/Thinking.
        // Show only a proven stream, and keep its last measured rate through pauses.
        if let Some((start, first)) = flow.stream() {
            let generation = flow.last.since(first);
            let tokens = flow.bytes as f64 / flow.token_ratio;
            if flow.covered {
                streams.push(CoveredStream {
                    harness: flow.previous.harness.clone(),
                    first,
                    last: flow.last,
                    rate: tokens / generation,
                    tokens,
                });
                continue;
            }
            live.push(LiveCall {
                id: format!("network:{}", flow.id),
                harness: flow.previous.harness.clone(),
                model: "unknown".into(),
                provider: flow.previous.host.clone(),
                phase: "Streaming · network estimate".into(),
                started_at: start,
                last_activity: flow.last,
                ttft: Some(first.since(start)),
                rate: Some(tokens / generation),
                output_tokens: Some(tokens.min(i32::MAX as f64) as i32),
                estimated: true,
            });
        }
    }
    // A session log writes nothing while a reply streams, so the log says a call is in flight and the
    // traffic says how fast it is arriving. A stream with no call due in the log is shown nowhere:
    // the same connection carries housekeeping that is not model output.
    for stream in streams {
        let due = live
            .iter_mut()
            .filter(|call| {
                call.harness == stream.harness
                    && !call.estimated
                    && !poll.proxy_calls.contains_key(&call.id)
                    && call.started_at <= stream.first
            })
            .max_by_key(|call| call.started_at);
        if let Some(call) = due {
            call.phase = "Streaming · network estimate".into();
            call.ttft = call.ttft.or(Some(stream.first.since(call.started_at)));
            call.rate = Some(stream.rate);
            call.output_tokens = Some(stream.tokens.min(i32::MAX as f64) as i32);
            call.last_activity = stream.last;
            call.estimated = true;
        }
    }
    // A connection that disappeared has ended.
    let PollState { flows, pending, .. } = poll;
    flows.retain(|key, flow| {
        if seen.contains(key) {
            return true;
        }
        finish_network(flow, pending);
        false
    });
}
