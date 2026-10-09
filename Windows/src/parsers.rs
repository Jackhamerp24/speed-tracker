use crate::domain::RequestRecord;
use crate::json::{add, Json};
use crate::time::{earlier, Time, MAX_UNIX_MS};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// The largest integer a JSON number carries exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// One finished model call as a session log describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    pub key: String,
    pub harness: String,
    pub model: String,
    pub provider: String,
    pub request_start: Option<Time>,
    pub first_token: Option<Time>,
    pub first_visible: Option<Time>,
    pub end: Time,
    pub output_tokens: i32,
    pub reasoning_tokens: Option<i32>,
    pub input_tokens: Option<i32>,
    pub cached_input_tokens: Option<i32>,
    pub aborted: bool,
    /// A rate the harness measured itself, for when "all output between first token and end" does not fit.
    pub speed: Option<f64>,
}

impl LogRecord {
    pub fn make_record(&self) -> RequestRecord {
        let ttft = match (self.request_start, self.first_token) {
            (Some(start), Some(first)) => {
                Some(first.since(start)).filter(|seconds| (0.0..3600.0).contains(seconds))
            }
            _ => None,
        };
        // Generation time can be known without a dispatch time; TTFT cannot.
        let generation = self
            .first_token
            .filter(|_| self.request_start.is_none() || ttft.is_some())
            .map(|first| self.end.since(first).max(0.0));
        let total = match self.request_start {
            Some(start) => self.end.since(start).max(0.0),
            None => generation.unwrap_or(0.0),
        };
        let mut rate = match generation {
            Some(seconds) if seconds >= 0.05 => Some(self.output_tokens as f64 / seconds),
            _ if total >= 0.05 => Some(self.output_tokens as f64 / total),
            _ => None,
        };
        if let Some(measured) = self.speed.filter(|speed| speed.is_finite() && *speed > 0.0) {
            rate = Some(measured);
        }
        RequestRecord {
            started_at: self.request_start.or(self.first_token).unwrap_or(self.end),
            harness: self.harness.clone(),
            route: self.provider.clone(),
            upstream_host: self.provider.clone(),
            format: "unknown".into(),
            model: self.model.clone(),
            streamed: true,
            status: if self.aborted { 499 } else { 200 },
            ttft,
            first_visible: match (self.request_start, self.first_visible) {
                (Some(start), Some(visible)) if visible >= start => Some(visible.since(start)),
                _ => None,
            },
            generation,
            total,
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            output_tokens: self.output_tokens.max(0),
            reasoning_tokens: self.reasoning_tokens,
            tokens_estimated: false,
            tps: rate,
            aborted: self.aborted,
            source: Some("log".into()),
            source_key: Some(self.key.clone()),
            ..RequestRecord::default()
        }
    }
}

/// What a parser knows about its session right now, beyond the finished calls it has emitted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParserState {
    pub current_model: Option<String>,
    pub current_provider: String,
    pub awaiting_response: bool,
    pub last_request_at: Option<Time>,
    pub first_token_at: Option<Time>,
    pub first_visible_at: Option<Time>,
    /// The file is in a format this parser understands.
    pub supported: bool,
    pub limitation: Option<String>,
}

impl ParserState {
    fn note_request(&mut self, time: Option<Time>) {
        self.awaiting_response = true;
        self.last_request_at = time;
        self.first_token_at = None;
        self.first_visible_at = None;
    }
}

pub enum SessionLogParser {
    ClaudeCode(ClaudeCodeLogParser),
    Codex(CodexLogParser),
    Omp(OmpLogParser),
    GeminiCli(GeminiCliLogParser),
    DeepSeek(DeepSeekLogParser),
}

impl SessionLogParser {
    /// The parser for a harness's session files. Unlisted names use the OMP/Pi format.
    pub fn for_harness(harness: &str) -> SessionLogParser {
        match harness {
            "Claude Code" => SessionLogParser::ClaudeCode(ClaudeCodeLogParser::default()),
            "Codex" => SessionLogParser::Codex(CodexLogParser::new()),
            "DeepSeek CLI" => SessionLogParser::DeepSeek(DeepSeekLogParser::new(None, None)),
            "Gemini CLI" => SessionLogParser::GeminiCli(GeminiCliLogParser::new()),
            other => SessionLogParser::Omp(OmpLogParser::new(other)),
        }
    }
    pub fn harness(&self) -> &str {
        match self {
            SessionLogParser::ClaudeCode(_) => "Claude Code",
            SessionLogParser::Codex(_) => "Codex",
            SessionLogParser::Omp(parser) => &parser.harness,
            SessionLogParser::GeminiCli(_) => "Gemini CLI",
            SessionLogParser::DeepSeek(_) => "DeepSeek CLI",
        }
    }
    pub fn state(&self) -> &ParserState {
        match self {
            SessionLogParser::ClaudeCode(parser) => &parser.state,
            SessionLogParser::Codex(parser) => &parser.state,
            SessionLogParser::Omp(parser) => &parser.state,
            SessionLogParser::GeminiCli(parser) => &parser.state,
            SessionLogParser::DeepSeek(parser) => &parser.state,
        }
    }
    pub fn ingest(&mut self, entry: &Value) -> Vec<LogRecord> {
        match self {
            SessionLogParser::ClaudeCode(parser) => parser.ingest(entry),
            SessionLogParser::Codex(parser) => parser.ingest(entry),
            SessionLogParser::Omp(parser) => parser.ingest(entry),
            SessionLogParser::GeminiCli(parser) => parser.ingest(entry),
            SessionLogParser::DeepSeek(parser) => parser.ingest(entry),
        }
    }
    /// Parses one line of JSON and ingests it.
    pub fn ingest_str(&mut self, json: &str) -> Result<Vec<LogRecord>, serde_json::Error> {
        Ok(self.ingest(&serde_json::from_str(json)?))
    }
    /// Gives up calls the log cannot mark as finished, once the file has been quiet for `idle` seconds.
    pub fn flush(&mut self, idle: f64) -> Vec<LogRecord> {
        match self {
            SessionLogParser::ClaudeCode(parser) => parser.flush(idle),
            _ => Vec::new(),
        }
    }
    pub fn as_deepseek(&self) -> Option<&DeepSeekLogParser> {
        match self {
            SessionLogParser::DeepSeek(parser) => Some(parser),
            _ => None,
        }
    }
}

// Claude Code: one line per content block, stamped when the block finished. Every line of a message
// carries the final usage, so the last line cannot be recognised on arrival; a message is finalised by
// the next message id, a system line, or a second of silence.
//
// Measured on Claude Code 2.1 (2026-10-10): the lines of a message, and any tool results that came back
// while it streamed, reach the file in one write about half a second after the message ends. Nothing
// is written while a reply is streaming, so the log can say that a reply is due and what it measured
// once it is over, but never how fast it is arriving.
#[derive(Default)]
pub struct ClaudeCodeLogParser {
    pub state: ParserState,
    // Main conversation and sidechain (sub-agent) messages interleave, so each has its own slot.
    pending: HashMap<bool, PendingClaude>,
    input: HashMap<bool, Time>,
}

// Whether a `user` line means a request is about to leave. A prompt and a tool result do. What a local
// command printed, the summary a compaction leaves behind and the notice of an interruption do not:
// no reply follows them, and counting them would show a call in progress until the ten-minute limit.
fn starts_request(entry: &Value) -> bool {
    if entry.b("isCompactSummary") {
        return false;
    }
    // Only the opening marker of the line's own text is looked at; the prompt itself is never kept.
    // Text inside a tool result is the tool's output and says nothing about the request.
    let content = entry.get_or_null("message").get_or_null("content");
    let texts = content.as_str().into_iter().chain(
        content
            .items()
            .iter()
            .filter(|block| block.s("type") == Some("text"))
            .filter_map(|block| block.s("text")),
    );
    !texts.into_iter().any(|text| {
        ["[Request interrupted", "<local-command-"]
            .iter()
            .any(|marker| text.trim_start().starts_with(marker))
    })
}

struct PendingClaude {
    id: String,
    model: String,
    start: Option<Time>,
    first: Option<Time>,
    end: Time,
    output: i32,
    input: Option<i32>,
    cache: Option<i32>,
}

impl ClaudeCodeLogParser {
    pub fn ingest(&mut self, entry: &Value) -> Vec<LogRecord> {
        let side = entry.b("isSidechain");
        let kind = entry.s("type");
        let time = entry.get_or_null("timestamp").date();
        if kind == Some("user") {
            self.state.supported = true;
            // Text Claude Code adds beside a prompt or a tool result; the line it accompanies decides.
            if entry.b("isMeta") {
                return Vec::new();
            }
            if !starts_request(entry) {
                if !side {
                    self.state.awaiting_response = false;
                }
                return Vec::new();
            }
            if let Some(time) = time {
                self.input.insert(side, time);
            }
            if !side {
                self.state.note_request(time);
            }
            return Vec::new();
        }
        if kind == Some("system") {
            if !side {
                self.state.awaiting_response = false;
            }
            return self.finalize(side);
        }
        let message = entry.get_or_null("message");
        let (Some("assistant"), Some(time), Some(id), Some(model)) =
            (kind, time, message.s("id"), message.s("model"))
        else {
            return Vec::new();
        };
        if model == "<synthetic>" {
            return Vec::new();
        }
        self.state.supported = true;
        let finished = if self
            .pending
            .get(&side)
            .is_some_and(|previous| previous.id != id)
        {
            self.finalize(side)
        } else {
            Vec::new()
        };
        let first_line = !self.pending.contains_key(&side);
        let start = self.input.get(&side).copied();
        let current = self.pending.entry(side).or_insert_with(|| PendingClaude {
            id: id.to_string(),
            model: model.to_string(),
            start,
            first: None,
            end: time,
            output: 0,
            input: None,
            cache: None,
        });
        let usage = message.get_or_null("usage");
        // A thinking block records how long it took, which dates the start of generation.
        if first_line
            && message
                .get_or_null("content")
                .items()
                .first()
                .and_then(|block| block.s("type"))
                == Some("thinking")
        {
            if let Some(duration) = entry.n("thinkingDurationMs") {
                current.first = Some(time.add_ms(-duration));
            }
        }
        current.end = current.end.max(time);
        current.output = current.output.max(usage.i("output_tokens").unwrap_or(0));
        current.input = add(
            usage.i("input_tokens"),
            usage.i("cache_read_input_tokens"),
            usage.i("cache_creation_input_tokens"),
        )
        .or(current.input);
        current.cache = usage.i("cache_read_input_tokens").or(current.cache);
        if !side {
            self.state.current_model = Some(model.to_string());
            self.state.current_provider = "anthropic".into();
            self.state.awaiting_response = false;
        }
        finished
    }

    pub fn flush(&mut self, idle: f64) -> Vec<LogRecord> {
        if idle < 1.0 {
            return Vec::new();
        }
        let mut records = self.finalize(false);
        records.extend(self.finalize(true));
        records
    }

    fn finalize(&mut self, side: bool) -> Vec<LogRecord> {
        let Some(done) = self.pending.remove(&side).filter(|done| done.output > 0) else {
            return Vec::new();
        };
        let first = match (done.first, done.start) {
            (Some(first), Some(start)) if first < start => None,
            (first, _) => first,
        };
        vec![LogRecord {
            key: format!("claude:{}", done.id),
            harness: "Claude Code".into(),
            model: done.model,
            provider: "anthropic".into(),
            request_start: done.start,
            first_token: first,
            first_visible: None,
            end: done.end,
            output_tokens: done.output,
            reasoning_tokens: None,
            input_tokens: done.input,
            cached_input_tokens: done.cache,
            aborted: false,
            speed: None,
        }]
    }
}

// Codex: a response ends with token_usage_record. Completed Reasoning and AgentMessage items carry
// the time they started; the earliest is the first token. The request time is the preceding user
// message or tool output. token_count repeats and is only a fallback.
pub struct CodexLogParser {
    pub state: ParserState,
    session: String,
    saw_usage: bool,
    last_total: Option<i32>,
    index: u32,
}

impl Default for CodexLogParser {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexLogParser {
    pub fn new() -> CodexLogParser {
        CodexLogParser {
            state: ParserState {
                current_provider: "openai".into(),
                ..ParserState::default()
            },
            session: "unknown".into(),
            saw_usage: false,
            last_total: None,
            index: 0,
        }
    }

    pub fn ingest(&mut self, entry: &Value) -> Vec<LogRecord> {
        let payload = entry.get_or_null("payload");
        let time = entry.get_or_null("timestamp").date();
        match entry.s("type") {
            Some("session_meta") => {
                self.state.supported = true;
                if let Some(session) = payload.s("session_id").or_else(|| payload.s("id")) {
                    self.session = session.to_string();
                }
                if let Some(provider) = payload.s("model_provider") {
                    self.state.current_provider = provider.to_string();
                }
            }
            Some("turn_context") => {
                self.state.supported = true;
                if let Some(model) = payload.s("model") {
                    self.state.current_model = Some(model.to_string());
                }
            }
            Some("response_item") => {
                let kind = payload.s("type").unwrap_or("");
                if (kind == "message" && payload.s("role") == Some("user"))
                    || kind.ends_with("_output")
                {
                    self.state.supported = true;
                    self.state.note_request(time);
                }
            }
            Some("event_msg") => match payload.s("type") {
                Some("item_completed") => {
                    let item = payload.get_or_null("item").s("type").map(str::to_lowercase);
                    let item = item.as_deref();
                    let started = payload.get_or_null("started_at_ms").date();
                    if let (Some("reasoning" | "agentmessage" | "agent_message"), Some(started)) =
                        (item, started)
                    {
                        // An item that began before this request belongs to an earlier one.
                        if self
                            .state
                            .last_request_at
                            .is_none_or(|request| started >= request.add_seconds(-0.5))
                        {
                            self.state.first_token_at =
                                earlier(self.state.first_token_at, Some(started));
                            if item != Some("reasoning") {
                                self.state.first_visible_at =
                                    earlier(self.state.first_visible_at, Some(started));
                            }
                        }
                    }
                }
                Some("task_complete" | "turn_aborted") => self.state.awaiting_response = false,
                Some("token_count") if !self.saw_usage => {
                    let info = payload.get_or_null("info");
                    let total = info.get_or_null("total_token_usage").i("total_tokens");
                    if total != self.last_total {
                        self.last_total = total;
                        self.index += 1;
                        let key = format!("codex:{}:{}", self.session, self.index);
                        return self.emit(info.get_or_null("last_token_usage"), key, time);
                    }
                }
                _ => {}
            },
            Some("token_usage_record") => {
                self.state.supported = true;
                self.saw_usage = true;
                let key = match payload.s("response_id") {
                    Some(response) => format!("codex:{response}"),
                    None => {
                        self.index += 1;
                        format!("codex:{}:{}", self.session, self.index)
                    }
                };
                return self.emit(payload.get_or_null("usage"), key, time);
            }
            _ => {}
        }
        Vec::new()
    }

    fn emit(&mut self, usage: &Value, key: String, time: Option<Time>) -> Vec<LogRecord> {
        let mut result = Vec::new();
        if let (Some(time), Some(output)) =
            (time, usage.i("output_tokens").filter(|output| *output > 0))
        {
            result.push(LogRecord {
                key,
                harness: "Codex".into(),
                model: self
                    .state
                    .current_model
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                provider: self.state.current_provider.clone(),
                request_start: self
                    .state
                    .last_request_at
                    .filter(|_| self.state.awaiting_response),
                first_token: self.state.first_token_at,
                first_visible: self.state.first_visible_at,
                end: time,
                output_tokens: output,
                reasoning_tokens: usage.i("reasoning_output_tokens"),
                input_tokens: usage.i("input_tokens"),
                cached_input_tokens: usage.i("cached_input_tokens"),
                aborted: false,
                speed: None,
            });
        }
        self.state.awaiting_response = false;
        self.state.first_token_at = None;
        self.state.first_visible_at = None;
        result
    }
}

// OMP / Pi: the assistant entry carries ttft and duration in milliseconds, token usage, model and
// provider, and the request time. Nothing is inferred.
pub struct OmpLogParser {
    pub state: ParserState,
    harness: String,
}

impl OmpLogParser {
    pub fn new(harness: &str) -> OmpLogParser {
        OmpLogParser {
            state: ParserState::default(),
            harness: harness.to_string(),
        }
    }

    pub fn ingest(&mut self, entry: &Value) -> Vec<LogRecord> {
        match entry.s("type") {
            Some("model_change") => {
                self.state.supported = true;
                self.state.current_model = entry
                    .s("model")
                    .or_else(|| entry.s("modelId"))
                    .map(|model| model.rsplit('/').next().unwrap_or(model).to_string());
                if let Some(provider) = entry.s("provider") {
                    self.state.current_provider = provider.to_string();
                }
                return Vec::new();
            }
            Some("message") => {}
            _ => return Vec::new(),
        }
        let message = entry.get_or_null("message");
        let Some(role) = message.s("role") else {
            return Vec::new();
        };
        self.state.supported = true;
        self.state.awaiting_response = matches!(role, "user" | "toolResult");
        if self.state.awaiting_response {
            self.state.last_request_at = entry.get_or_null("timestamp").date();
            self.state.first_token_at = None;
            self.state.first_visible_at = None;
        }
        let usage = message.get_or_null("usage");
        let Some(output) = usage
            .i("output")
            .filter(|output| role == "assistant" && *output > 0)
        else {
            return Vec::new();
        };
        let model = message
            .s("model")
            .map(str::to_string)
            .or_else(|| self.state.current_model.clone())
            .unwrap_or_else(|| "unknown".into());
        self.state.current_model = Some(model.clone());
        self.state.current_provider = message.s("provider").unwrap_or("").to_string();
        let written = entry.get_or_null("timestamp").date();
        let duration = message.n("duration");
        let mut start = message.get_or_null("timestamp").date();
        if let (None, Some(written), Some(duration)) = (start, written, duration) {
            start = Some(written.add_ms(-duration));
        }
        let end = match (start, duration) {
            (Some(start), Some(duration)) => Some(start.add_ms(duration)),
            _ => written,
        };
        let Some(end) = end else { return Vec::new() };
        let first = match (start, message.n("ttft")) {
            (Some(start), Some(ttft)) => Some(start.add_ms(ttft)),
            _ => None,
        };
        let id = entry
            .s("id")
            .or_else(|| message.s("responseId"))
            .map(str::to_string)
            .unwrap_or_else(|| end.unix_ms().to_string());
        vec![LogRecord {
            key: format!("{}:{id}", self.harness.to_lowercase()),
            harness: self.harness.clone(),
            model,
            provider: self.state.current_provider.clone(),
            request_start: start,
            first_token: first,
            first_visible: None,
            end,
            output_tokens: output,
            reasoning_tokens: usage.i("reasoningTokens"),
            input_tokens: add(
                usage.i("input"),
                usage.i("cacheRead"),
                usage.i("cacheWrite"),
            ),
            cached_input_tokens: usage.i("cacheRead"),
            aborted: matches!(message.s("stopReason"), Some("aborted" | "error")),
            speed: None,
        }]
    }
}

// Gemini CLI: ~/.gemini/tmp/<project>/chats/session-*.jsonl. The first line is session metadata and
// {"$set": …} lines update it. Every other line is a message, written again whole each time it changes.
// A gemini message is created when its stream ends, so its timestamp is the end of the response; each
// thought carries its arrival time. Requests after tool calls go out when the tools finish.
pub struct GeminiCliLogParser {
    pub state: ParserState,
    emitted: HashSet<String>,
    // The last request time has been matched to a response; the next response needs a new one.
    request_used: bool,
}

impl Default for GeminiCliLogParser {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiCliLogParser {
    pub fn new() -> GeminiCliLogParser {
        GeminiCliLogParser {
            state: ParserState {
                current_provider: "google".into(),
                ..ParserState::default()
            },
            emitted: HashSet::new(),
            request_used: true,
        }
    }

    pub fn ingest(&mut self, entry: &Value) -> Vec<LogRecord> {
        let update = entry.get_or_null("$set");
        if update.is_object() {
            self.state.supported = true;
            return update
                .get_or_null("messages")
                .items()
                .iter()
                .flat_map(|message| self.message(message))
                .collect();
        }
        if entry.get_or_null("$rewindTo").is_string() {
            self.state.awaiting_response = false;
            self.request_used = true;
            return Vec::new();
        }
        if entry.s("sessionId").is_some() && entry.s("id").is_none() {
            self.state.supported = true;
            return Vec::new();
        }
        self.message(entry)
    }

    fn message(&mut self, message: &Value) -> Vec<LogRecord> {
        let (Some(id), Some(kind), Some(time)) = (
            message.s("id"),
            message.s("type"),
            message.get_or_null("timestamp").date(),
        ) else {
            return Vec::new();
        };
        self.state.supported = true;
        match kind {
            "user" => {
                self.note_request(time);
                return Vec::new();
            }
            "error" => {
                self.state.awaiting_response = false;
                return Vec::new();
            }
            "gemini" => {}
            _ => return Vec::new(),
        }
        if let Some(model) = message.s("model").filter(|model| !model.is_empty()) {
            self.state.current_model = Some(model.to_string());
        }
        let mut records = Vec::new();
        if !self.emitted.contains(id) {
            if let Some(record) = self.record(id, message, time) {
                self.emitted.insert(id.to_string());
                self.request_used = true;
                records.push(record);
            }
        }
        // The message is written again once its tools have run; the next request leaves then.
        let finished = message
            .get_or_null("toolCalls")
            .items()
            .iter()
            .filter_map(|call| call.get_or_null("timestamp").date())
            .max();
        match finished {
            Some(finished) if finished >= time => {
                if self
                    .state
                    .last_request_at
                    .is_none_or(|request| finished > request)
                {
                    self.note_request(finished);
                }
            }
            _ => self.state.awaiting_response = false,
        }
        records
    }

    fn note_request(&mut self, time: Time) {
        self.request_used = false;
        self.state.note_request(Some(time));
    }

    fn record(&self, id: &str, message: &Value, end: Time) -> Option<LogRecord> {
        // Tokens can be missing on the first write of a message and arrive with a later one.
        let tokens = message.get_or_null("tokens");
        if !tokens.is_object() {
            return None;
        }
        let thoughts = tokens.i("thoughts").unwrap_or(0);
        // Gemini counts thinking apart from the visible reply; the response produced both.
        let output = tokens.i("output").unwrap_or(0).saturating_add(thoughts);
        if output <= 0 {
            return None;
        }
        // A reply with no fresh request before it gets no start time, never the previous one.
        let start = self.state.last_request_at.filter(|requested| {
            !self.request_used && *requested <= end && end.since(*requested) < 3600.0
        });
        let mut first = message
            .get_or_null("thoughts")
            .items()
            .iter()
            .fold(None, |first, thought| {
                earlier(first, thought.get_or_null("timestamp").date())
            });
        if first.is_some_and(|first| first > end || start.is_some_and(|start| first < start)) {
            first = None;
        }
        Some(LogRecord {
            key: format!("gemini:{id}"),
            harness: "Gemini CLI".into(),
            model: self
                .state
                .current_model
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            provider: "google".into(),
            request_start: start,
            first_token: first,
            first_visible: None,
            end,
            output_tokens: output,
            reasoning_tokens: Some(thoughts).filter(|thoughts| *thoughts > 0),
            input_tokens: tokens.i("input"),
            cached_input_tokens: tokens.i("cached"),
            aborted: false,
            speed: None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Step {
    turn: i32,
    index: i32,
}

impl Step {
    fn read(data: &Value) -> Option<Step> {
        Some(Step {
            turn: data.i("turn")?,
            index: data.i("step")?,
        })
    }
}

// A whole number of events or bytes, small enough to be exact in JSON.
fn count(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .filter(|number| (0..=MAX_SAFE_INTEGER as i64).contains(number))
}

fn is_packed(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("text-chunks" | "reasoning-chunks" | "tool-call-chunks")
    )
}

// Official deepseek-ai/deepseek-harness (dsh), not a guess based on model vendor.
pub struct DeepSeekLogParser {
    pub state: ParserState,
    version: Option<i32>,
    has_supported_header: bool,
    inherited_cut: Option<i64>,
    known_inherited_cut: Option<i64>,
    expected_version: Option<i32>,
    session: String,
    seeded: bool,
    last_sequence: i64,
    pending: Option<(Step, Time)>,
    historical_step: Option<Step>,
    historical_timing: Timing,
}

impl DeepSeekLogParser {
    /// `inherited_event_count` is the boundary found by an earlier pass over a seeded log;
    /// `expected_version` is the generation the file name claims.
    pub fn new(
        inherited_event_count: Option<i64>,
        expected_version: Option<i32>,
    ) -> DeepSeekLogParser {
        DeepSeekLogParser {
            state: ParserState::default(),
            version: None,
            has_supported_header: false,
            inherited_cut: None,
            known_inherited_cut: inherited_event_count,
            expected_version,
            session: String::new(),
            seeded: false,
            last_sequence: -1,
            pending: None,
            historical_step: None,
            historical_timing: Timing::default(),
        }
    }
    pub fn version(&self) -> Option<i32> {
        self.version
    }
    pub fn has_supported_header(&self) -> bool {
        self.has_supported_header
    }
    /// Events up to this sequence were copied from the session this one was seeded from.
    pub fn inherited_cut(&self) -> Option<i64> {
        self.inherited_cut
    }
    /// A seeded v2+ log marks its inherited events only at their end, so it must be read once to
    /// find that mark before any telemetry in it can be trusted.
    pub fn needs_inherited_scan(&self) -> bool {
        self.has_supported_header
            && self.seeded
            && self.version.is_some_and(|version| version >= 2)
            && self.known_inherited_cut.is_none()
    }

    pub fn ingest(&mut self, entry: &Value) -> Vec<LogRecord> {
        let kind = entry.s("type");
        if kind == Some("session") {
            self.header(entry);
            return Vec::new();
        }
        let data = entry.get_or_null("data");
        if !self.has_supported_header || !data.is_object() {
            return Vec::new();
        }
        let version = self.version.unwrap_or(0);
        let packed = is_packed(kind);
        let (Some(sequence), Some(time)) = (
            count(entry.get_or_null(if packed { "seq0" } else { "seq" })),
            entry
                .get_or_null(if packed { "time0" } else { "time" })
                .date(),
        ) else {
            return Vec::new();
        };
        if sequence <= self.last_sequence {
            return Vec::new();
        }
        let mut packed_count = 1;
        if packed {
            let members = data
                .get_or_null(if kind == Some("tool-call-chunks") {
                    "args"
                } else {
                    "texts"
                })
                .items();
            if version >= 2
                || members.is_empty()
                || members.iter().any(|member| !member.is_string())
            {
                return Vec::new();
            }
            packed_count = members.len() as i64;
        }
        self.last_sequence = sequence + packed_count - 1;
        if kind == Some("session/end-seed") && data.b("inherited") {
            self.inherited_cut = Some(self.inherited_cut.unwrap_or(0).max(sequence));
        }
        if self.needs_inherited_scan()
            || self
                .inherited_cut
                .is_some_and(|cut| sequence + packed_count <= cut)
        {
            return Vec::new();
        }
        let step = Step::read(data);
        let pending_step = self.pending.map(|(step, _)| step);
        match (kind, step) {
            (Some("step/start"), Some(step)) => {
                self.pending = Some((step, time));
                self.historical_step = Some(step);
                self.historical_timing = Timing::default();
                self.state.note_request(Some(time));
                self.state.supported = true;
            }
            (Some("request/header"), _) => {
                self.set_model(data.get_or_null("header").get_or_null("config"))
            }
            (Some("request/context"), _) => self.set_model(data),
            (Some("assistant/chunk"), Some(step)) if version < 2 => {
                self.timing_for(step).chunk(data.get_or_null("chunk"), time);
                self.update_live_timing(step);
            }
            (
                Some(kind @ ("text-chunks" | "reasoning-chunks" | "tool-call-chunks")),
                Some(step),
            ) => {
                let skip = (self.inherited_cut.unwrap_or(0) - sequence).max(0);
                self.timing_for(step)
                    .packed(kind, entry.get_or_null("time0"), data, skip);
                self.update_live_timing(step);
            }
            (Some("assistant/attempt" | "llm/retry"), _) => {
                if step.is_some() && pending_step == step {
                    self.clear_pending();
                }
                self.historical_timing = Timing::default();
                self.historical_step = None;
                self.state.supported = true;
            }
            (Some("assistant/message"), Some(step)) => {
                return self.message(entry, data, step, sequence, time)
            }
            (Some("step/end"), _) => {
                if step.is_some() && pending_step == step {
                    self.clear_pending();
                }
                self.historical_timing = Timing::default();
                self.historical_step = None;
            }
            (Some("turn/end" | "session/end-seed"), _) => {
                self.clear_pending();
                self.historical_timing = Timing::default();
                self.historical_step = None;
            }
            _ => {}
        }
        Vec::new()
    }

    fn header(&mut self, entry: &Value) {
        if self.version.is_some() {
            return;
        }
        let (Some(version), Some(id), Some(_)) = (
            entry.i("version"),
            entry.s("id").filter(|id| !id.is_empty()),
            entry.get_or_null("createdAt").date(),
        ) else {
            self.state.limitation = Some("Invalid DeepSeek session header.".into());
            return;
        };
        self.version = Some(version);
        if version > 4
            || self
                .expected_version
                .is_some_and(|expected| expected != version)
        {
            self.state.limitation = Some(format!(
                "Unsupported DeepSeek session generation v{version}."
            ));
            return;
        }
        if version >= 2 {
            let Some(seeded) = entry.get_or_null("isSeeded").as_bool() else {
                self.state.limitation = Some("DeepSeek session header lacks isSeeded.".into());
                return;
            };
            self.seeded = seeded;
            self.inherited_cut = self.known_inherited_cut;
        } else {
            // v0 and v1 state the length of the inherited seed up front.
            self.seeded = entry.has("seedLength");
            let length = count(entry.get_or_null("seedLength"));
            if self.seeded && length.is_none() {
                self.state.limitation = Some("Invalid DeepSeek inherited seed length.".into());
                return;
            }
            self.inherited_cut = Some(length.unwrap_or(0));
        }
        self.session = id.to_string();
        self.has_supported_header = true;
        if self.needs_inherited_scan() {
            self.state.limitation =
                Some("DeepSeek seeded log has no complete inherited boundary.".into());
        }
    }

    fn message(
        &mut self,
        entry: &Value,
        data: &Value,
        step: Step,
        sequence: i64,
        time: Time,
    ) -> Vec<LogRecord> {
        let version = self.version.unwrap_or(0);
        let matched = self.pending.filter(|(pending, _)| *pending == step);
        let request = matched.map(|(_, time)| time);
        if matched.is_some() {
            self.clear_pending();
        }
        let mut timing = if version < 2 && self.historical_step == Some(step) {
            std::mem::take(&mut self.historical_timing)
        } else {
            Timing::default()
        };
        self.historical_timing = Timing::default();
        self.historical_step = None;
        for row in data.get_or_null("stream").items() {
            timing.stream_record(row);
        }
        let message = data.get_or_null("message");
        let mut source = message.get_or_null("source");
        let id =
            if version == 0 && !message.is_object() && data.get_or_null("provenance").is_object() {
                source = data.get_or_null("provenance");
                Some(format!("legacy-message:{}:{sequence}", self.session))
            } else {
                if message.s("role") != Some("assistant") || source.s("kind") != Some("model") {
                    return Vec::new();
                }
                message.s("id").map(str::to_string)
            };
        let (Some(id), Some(model), Some(provider)) = (
            id.filter(|id| !id.is_empty()),
            source.s("model").filter(|model| !model.is_empty()),
            source.s("provider"),
        ) else {
            return Vec::new();
        };
        // Only a message appended to the transcript is a new response; edits and replacements are not.
        if entry.has("surfaceOp") && entry.s("surfaceOp") != Some("append") {
            return Vec::new();
        }
        self.state.current_model = Some(model.to_string());
        self.state.current_provider = provider.to_string();
        self.state.supported = true;
        let usage = if data.get_or_null("usage").is_object() {
            Some(Usage::read(data.get_or_null("usage")))
        } else {
            timing.usage
        };
        let Some((usage, output)) =
            usage.and_then(|usage| usage.output.map(|output| (usage, output)))
        else {
            return Vec::new();
        };
        let end = timing
            .finish
            .filter(|finish| *finish <= time)
            .unwrap_or(time);
        let valid = |date: Option<Time>| {
            date.filter(|date| *date <= end && request.is_none_or(|start| *date >= start))
        };
        vec![LogRecord {
            key: format!("deepseek:{id}"),
            harness: "DeepSeek CLI".into(),
            model: model.to_string(),
            provider: provider.to_string(),
            request_start: request.filter(|request| *request <= end),
            first_token: valid(timing.first()),
            first_visible: valid(timing.visible()),
            end,
            output_tokens: output,
            reasoning_tokens: usage.reasoning,
            input_tokens: usage.input,
            cached_input_tokens: usage.cache,
            aborted: data.b("interrupted") || timing.aborted,
            speed: None,
        }]
    }

    // The timing being collected for this step, started afresh if the step changed.
    fn timing_for(&mut self, step: Step) -> &mut Timing {
        if self.historical_step != Some(step) {
            self.historical_timing = Timing::default();
            self.historical_step = Some(step);
        }
        &mut self.historical_timing
    }

    fn set_model(&mut self, data: &Value) {
        if let Some(model) = data.s("model").filter(|model| !model.is_empty()) {
            self.state.current_model = Some(model.to_string());
        }
        if let Some(provider) = data.s("provider") {
            self.state.current_provider = provider.to_string();
        }
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.state.awaiting_response = false;
        self.state.first_token_at = None;
        self.state.first_visible_at = None;
    }

    fn update_live_timing(&mut self, step: Step) {
        let Some((_, requested)) = self.pending.filter(|(pending, _)| *pending == step) else {
            return;
        };
        self.state.first_token_at = self
            .historical_timing
            .first()
            .filter(|first| *first >= requested);
        self.state.first_visible_at = self
            .historical_timing
            .visible()
            .filter(|visible| *visible >= requested);
    }
}

#[derive(Clone, Copy, Debug)]
struct Usage {
    output: Option<i32>,
    reasoning: Option<i32>,
    input: Option<i32>,
    cache: Option<i32>,
}

impl Usage {
    fn read(value: &Value) -> Usage {
        let input = value.i("inputTokens");
        let cache = value.i("cacheReadTokens");
        Usage {
            output: value.i("outputTokens"),
            reasoning: value.i("reasoningTokens"),
            input: input.and_then(|input| add(Some(input), cache, value.i("cacheWriteTokens"))),
            cache,
        }
    }
}

// When a DeepSeek response's first token, first visible text and end arrived. Actual token deltas are
// preferred; the start of a block is only a fallback for streams that recorded no deltas.
#[derive(Default)]
struct Timing {
    first: Option<Time>,
    visible: Option<Time>,
    block_first: Option<Time>,
    block_visible: Option<Time>,
    saw_deltas: bool,
    saw_text_deltas: bool,
    finish: Option<Time>,
    usage: Option<Usage>,
    aborted: bool,
}

impl Timing {
    fn first(&self) -> Option<Time> {
        if self.saw_deltas {
            self.first
        } else {
            self.block_first
        }
    }
    fn visible(&self) -> Option<Time> {
        if self.saw_text_deltas {
            self.visible
        } else {
            self.block_visible
        }
    }

    fn chunk(&mut self, chunk: &Value, date: Time) {
        match chunk.s("type") {
            Some("text-delta") => {
                self.saw_deltas = true;
                self.saw_text_deltas = true;
                if let Some(text) = chunk.s("text") {
                    if !text.is_empty() {
                        self.first = earlier(self.first, Some(date));
                    }
                    if !text.trim().is_empty() {
                        self.visible = earlier(self.visible, Some(date));
                    }
                }
            }
            Some("reasoning-delta") => {
                self.saw_deltas = true;
                if chunk.s("text").is_some_and(|text| !text.is_empty()) {
                    self.first = earlier(self.first, Some(date));
                }
            }
            Some("tool-call-delta") => {
                self.saw_deltas = true;
                if chunk
                    .s("argumentsDelta")
                    .is_some_and(|delta| !delta.is_empty())
                    || chunk.s("name").is_some()
                {
                    self.first = earlier(self.first, Some(date));
                }
            }
            Some("block-start") => {
                let block = chunk.s("blockType");
                if matches!(block, Some("reasoning" | "text" | "tool-call")) {
                    self.block_first = earlier(self.block_first, Some(date));
                }
                if block == Some("text") {
                    self.block_visible = earlier(self.block_visible, Some(date));
                }
            }
            Some("usage") => self.usage = Some(Usage::read(chunk.get_or_null("usage"))),
            Some("finish") => {
                self.finish = Some(date);
                self.aborted = matches!(
                    chunk.get_or_null("reason").s("kind"),
                    Some("aborted" | "error")
                );
            }
            _ => {}
        }
    }

    fn stream_record(&mut self, row: &Value) {
        match row.s("type") {
            Some("chunk") => {
                if let Some(time) = row.get_or_null("time").date() {
                    self.chunk(row.get_or_null("chunk"), time);
                }
            }
            Some(kind @ ("text-chunks" | "reasoning-chunks" | "tool-call-chunks")) => {
                self.packed(kind, row.get_or_null("time0"), row, 0)
            }
            _ => {}
        }
    }

    // Several deltas packed into one row: a start time, then the gap in milliseconds before each
    // later member. The first `skip` members were inherited from the seeding session.
    fn packed(&mut self, kind: &str, time: &Value, data: &Value, skip: i64) {
        let members = data.get_or_null(if kind == "tool-call-chunks" {
            "args"
        } else {
            "texts"
        });
        let gaps = data.get_or_null("dt");
        let whole = |number: f64| number.trunc() == number;
        let Some(mut stamp) = time
            .number()
            .filter(|stamp| *stamp > 0.0 && *stamp <= MAX_SAFE_INTEGER && whole(*stamp))
        else {
            return;
        };
        let (Some(members), Some(gaps)) = (members.as_array(), gaps.as_array()) else {
            return;
        };
        if members.is_empty()
            || gaps.len() != members.len() - 1
            || members.iter().any(|member| !member.is_string())
        {
            return;
        }
        self.saw_deltas = true;
        if kind == "text-chunks" {
            self.saw_text_deltas = true;
        }
        for (index, member) in members.iter().enumerate() {
            if index > 0 {
                let Some(gap) = gaps[index - 1]
                    .number()
                    .filter(|gap| whole(*gap) && gap.abs() <= MAX_SAFE_INTEGER)
                else {
                    return;
                };
                stamp += gap;
            }
            if (index as i64) < skip || stamp <= 0.0 || stamp >= MAX_UNIX_MS {
                continue;
            }
            let date = Time::from_unix_ms(stamp as i64);
            let text = member.as_str().unwrap_or("");
            if !text.is_empty() || (kind == "tool-call-chunks" && data.s("name").is_some()) {
                self.first = earlier(self.first, Some(date));
            }
            if kind == "text-chunks" && !text.trim().is_empty() {
                self.visible = earlier(self.visible, Some(date));
            }
        }
    }
}
