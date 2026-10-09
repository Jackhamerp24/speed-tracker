//! Optional, local-only HTTP reverse proxy. A harness pointed at it gets its stream measured exactly.
//! Nothing depends on it. Requests are forwarded untouched and never logged.

use crate::domain::{LiveCall, RequestRecord};
use crate::json::Json;
use crate::time::Time;
use crate::tracker::Tracker;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

/// Reads a model API response as it streams past: when the first token and first visible text
/// arrived, and the usage the provider reported. Understands Anthropic, OpenAI (chat and
/// responses), Gemini and Ollama, as server-sent events, NDJSON or one JSON body.
pub struct StreamMeasurement {
    pending: Vec<u8>,
    // The body is one JSON document rather than a stream of lines.
    whole: bool,
    ignoring: bool,
    reasoning_seen: bool,
    chars: i64,
    reported: Option<i32>,
    pub model: String,
    pub format: String,
    pub streamed: bool,
    pub failed: bool,
    pub first: Option<f64>,
    pub visible: Option<f64>,
    pub last: Option<f64>,
    pub input: Option<i32>,
    pub cache: Option<i32>,
    pub reasoning: Option<i32>,
}

impl Default for StreamMeasurement {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamMeasurement {
    const LIMIT: usize = 2 * 1024 * 1024;

    pub fn new() -> StreamMeasurement {
        StreamMeasurement {
            pending: Vec::new(),
            whole: false,
            ignoring: false,
            reasoning_seen: false,
            chars: 0,
            reported: None,
            model: "unknown".into(),
            format: "unknown".into(),
            streamed: false,
            failed: false,
            first: None,
            visible: None,
            last: None,
            input: None,
            cache: None,
            reasoning: None,
        }
    }

    pub fn recognized(&self) -> bool {
        self.format != "unknown"
    }
    /// Output tokens: the provider's count, or about four characters a token until it reports one.
    pub fn output(&self) -> i32 {
        self.reported
            .unwrap_or_else(|| (self.chars as f64 / 4.0).ceil().min(i32::MAX as f64) as i32)
    }
    pub fn estimated(&self) -> bool {
        self.reported.is_none()
    }
    // Reasoning that never appeared in the stream took no stream time, so it is left out of the rate.
    pub fn rate_tokens(&self) -> f64 {
        let output = self.output();
        match self.reasoning {
            Some(reasoning) if !self.reasoning_seen && reasoning > 0 && reasoning < output => {
                (output - reasoning) as f64
            }
            _ => output as f64,
        }
    }
    pub fn live_rate(&self) -> Option<f64> {
        let window = self.last? - self.first?;
        (window >= 0.05).then(|| self.rate_tokens() / window)
    }

    pub fn set_content_type(&mut self, content_type: &str) {
        let content_type = content_type.to_ascii_lowercase();
        self.whole = content_type.contains("json") && !content_type.contains("ndjson");
    }

    pub fn ingest(&mut self, bytes: &[u8], time: f64) {
        if self.ignoring {
            return;
        }
        // "data:" or "event:" opens a server-sent event stream, whatever the content type claimed.
        if self.pending.is_empty() && matches!(bytes.first(), Some(b'd' | b'e')) {
            self.whole = false;
        }
        if self.whole {
            if self.pending.len() + bytes.len() > Self::LIMIT {
                self.ignoring = true;
                self.pending.clear();
            } else {
                self.pending.extend_from_slice(bytes);
            }
            return;
        }
        for &value in bytes {
            if value == b'\n' {
                self.consume(time);
                self.pending.clear();
            } else if self.pending.len() < Self::LIMIT {
                self.pending.push(value);
            } else {
                self.ignoring = true;
                self.pending.clear();
                return;
            }
        }
    }

    pub fn complete(&mut self, time: f64) {
        if self.ignoring || self.pending.is_empty() {
            return;
        }
        if self.whole {
            let body = std::mem::take(&mut self.pending);
            self.parse(&body, None);
        } else {
            self.consume(time);
        }
        self.pending.clear();
    }

    fn consume(&mut self, time: f64) {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        let mut line = text.trim();
        if let Some(data) = line.strip_prefix("data:") {
            line = data.trim_start();
        }
        if line.starts_with('{') {
            self.streamed = true;
            self.parse(line.as_bytes(), Some(time));
        }
    }

    // Counts generated text and notes when it arrived. `signal` marks an event that proves generation
    // began even though it carries no text.
    fn mark(&mut self, value: Option<&str>, time: Option<f64>, reasoning: bool, signal: bool) {
        let count = value.map_or(0, |text| text.encode_utf16().count());
        if count == 0 && !signal {
            return;
        }
        self.chars += count as i64;
        self.reasoning_seen |= reasoning;
        let Some(time) = time else { return };
        self.first.get_or_insert(time);
        self.last = Some(time);
        if !reasoning {
            self.visible.get_or_insert(time);
        }
    }

    fn usage(&mut self, usage: &Value, input: &str, output: &str) {
        self.input = usage.i(input).or(self.input);
        self.reported = usage.i(output).or(self.reported);
        self.cache = usage
            .get_or_null("input_tokens_details")
            .i("cached_tokens")
            .or_else(|| {
                usage
                    .get_or_null("prompt_tokens_details")
                    .i("cached_tokens")
            })
            .or_else(|| usage.i("cache_read_input_tokens"))
            .or_else(|| usage.i("prompt_cache_hit_tokens"))
            .or(self.cache);
        self.reasoning = usage
            .get_or_null("output_tokens_details")
            .i("reasoning_tokens")
            .or_else(|| {
                usage
                    .get_or_null("completion_tokens_details")
                    .i("reasoning_tokens")
            })
            .or(self.reasoning);
    }

    fn set_model(&mut self, model: Option<&str>) {
        if let Some(model) = model {
            self.model = model.to_string();
        }
    }

    fn parse(&mut self, bytes: &[u8], time: Option<f64>) {
        let Ok(row) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        let kind = row.s("type").unwrap_or("");
        self.set_model(row.s("model"));
        if !row.get_or_null("error").is_null() {
            self.failed = true;
        }
        if kind.starts_with("response.") || row.s("object") == Some("response") {
            self.format = "openai-responses".into();
            let response = if row.has("response") {
                row.get_or_null("response")
            } else {
                &row
            };
            self.set_model(response.s("model"));
            self.usage(
                response.get_or_null("usage"),
                "input_tokens",
                "output_tokens",
            );
            if kind == "response.output_item.added" {
                let item = row.get_or_null("item");
                match item.s("type") {
                    Some("reasoning") => self.mark(None, time, true, true),
                    Some(item_kind) if item_kind.contains("call") => {
                        self.mark(item.s("name"), time, false, true)
                    }
                    _ => {}
                }
            } else if kind.ends_with(".delta")
                && (!kind.contains("audio") || kind.contains("transcript"))
            {
                self.mark(row.s("delta"), time, kind.contains("reasoning"), false);
            }
            if kind == "response.failed" {
                self.failed = true;
            }
        } else if matches!(
            kind,
            "message_start"
                | "content_block_start"
                | "content_block_delta"
                | "message_delta"
                | "message_stop"
                | "message"
        ) {
            self.format = "anthropic".into();
            let message = if kind == "message_start" {
                row.get_or_null("message")
            } else {
                &row
            };
            self.set_model(message.s("model"));
            self.usage(
                message.get_or_null("usage"),
                "input_tokens",
                "output_tokens",
            );
            self.usage(row.get_or_null("usage"), "input_tokens", "output_tokens");
            if kind == "content_block_start" {
                let block = row.get_or_null("content_block");
                let thinking = matches!(block.s("type"), Some("thinking" | "redacted_thinking"));
                self.mark(
                    block
                        .s("text")
                        .or_else(|| block.s("thinking"))
                        .or_else(|| block.s("name")),
                    time,
                    thinking,
                    true,
                );
            }
            if kind == "content_block_delta" {
                let delta = row.get_or_null("delta");
                self.mark(
                    delta
                        .s("text")
                        .or_else(|| delta.s("thinking"))
                        .or_else(|| delta.s("partial_json")),
                    time,
                    delta.s("type") == Some("thinking_delta"),
                    false,
                );
            }
            // A non-streamed reply: the whole content is there at once.
            if time.is_none() {
                for part in row.get_or_null("content").items() {
                    self.mark(
                        part.s("text").or_else(|| part.s("thinking")),
                        None,
                        part.s("type") == Some("thinking"),
                        false,
                    );
                }
            }
        } else if row.get_or_null("choices").is_array() {
            self.format = "openai-chat".into();
            for choice in row.get_or_null("choices").items() {
                let delta = if choice.has("delta") {
                    choice.get_or_null("delta")
                } else {
                    choice.get_or_null("message")
                };
                self.mark(
                    delta
                        .s("reasoning_content")
                        .or_else(|| delta.s("reasoning")),
                    time,
                    true,
                    false,
                );
                self.mark(
                    delta.s("content").or_else(|| choice.s("text")),
                    time,
                    false,
                    false,
                );
                for tool in delta.get_or_null("tool_calls").items() {
                    let function = tool.get_or_null("function");
                    let call = format!(
                        "{}{}",
                        function.s("name").unwrap_or(""),
                        function.s("arguments").unwrap_or("")
                    );
                    self.mark(Some(&call), time, false, false);
                }
            }
            self.usage(
                row.get_or_null("usage"),
                "prompt_tokens",
                "completion_tokens",
            );
        } else if row.get_or_null("candidates").is_array()
            || row.get_or_null("usageMetadata").is_object()
        {
            self.format = "gemini".into();
            self.set_model(row.s("modelVersion"));
            for candidate in row.get_or_null("candidates").items() {
                for part in candidate
                    .get_or_null("content")
                    .get_or_null("parts")
                    .items()
                {
                    self.mark(part.s("text"), time, part.b("thought"), false);
                    let call = part.get_or_null("functionCall");
                    if call.is_object() {
                        self.mark(Some(&call.to_string()), time, false, false);
                    }
                }
            }
            let usage = row.get_or_null("usageMetadata");
            self.input = usage.i("promptTokenCount").or(self.input);
            self.cache = usage.i("cachedContentTokenCount").or(self.cache);
            self.reasoning = usage.i("thoughtsTokenCount").or(self.reasoning);
            // Gemini counts thinking apart from the visible reply; output here is their sum.
            if let Some(count) = usage.i("candidatesTokenCount") {
                self.reported = Some(count.saturating_add(self.reasoning.unwrap_or(0)));
            }
        } else if row.get_or_null("done").is_boolean() {
            self.format = "ollama".into();
            let message = row.get_or_null("message");
            self.mark(message.s("thinking"), time, true, false);
            self.mark(
                message.s("content").or_else(|| row.s("response")),
                time,
                false,
                false,
            );
            self.input = row.i("prompt_eval_count").or(self.input);
            self.reported = row.i("eval_count").or(self.reported);
        }
    }
}

const HOP_HEADERS: [&str; 10] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

fn is_hop_header(name: &str) -> bool {
    HOP_HEADERS.iter().any(|hop| name.eq_ignore_ascii_case(hop))
}

pub fn default_routes() -> HashMap<String, String> {
    [
        ("anthropic", "https://api.anthropic.com"),
        ("openai", "https://api.openai.com"),
        ("chatgpt", "https://chatgpt.com/backend-api/codex"),
        ("deepseek", "https://api.deepseek.com"),
        ("openrouter", "https://openrouter.ai/api"),
        ("gemini", "https://generativelanguage.googleapis.com"),
        ("xai", "https://api.x.ai"),
        ("groq", "https://api.groq.com/openai"),
        ("cerebras", "https://api.cerebras.ai"),
        ("mistral", "https://api.mistral.ai"),
        ("moonshot", "https://api.moonshot.ai"),
        ("zai", "https://api.z.ai"),
        ("ollama", "http://127.0.0.1:11434"),
        ("lmstudio", "http://127.0.0.1:1234"),
    ]
    .into_iter()
    .map(|(route, upstream)| (route.to_string(), upstream.to_string()))
    .collect()
}

fn normalize_harness(tag: &str) -> String {
    match tag.to_lowercase().as_str() {
        "claude" | "claude-code" => "Claude Code",
        "codex" => "Codex",
        "omp" => "OMP",
        "pi" => "Pi",
        "dsh" | "deepseek" => "DeepSeek CLI",
        "opencode" => "opencode",
        _ => tag,
    }
    .to_string()
}

fn guess_harness(agent: &str) -> String {
    let text = agent.to_lowercase();
    for name in [
        "claude-code",
        "claude-cli",
        "codex",
        "opencode",
        "deepseek",
        "dsh",
        "omp",
        "pi-coding-agent",
    ] {
        if text.contains(name) {
            return match name {
                "claude-cli" => "Claude Code".into(),
                "pi-coding-agent" => "Pi".into(),
                other => normalize_harness(other),
            };
        }
    }
    "Unknown".into()
}

struct Request {
    method: String,
    target: String,
    http11: bool,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

// Reads one request head. None when the client closed the connection or sent something that is not HTTP.
fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let available = reader.fill_buf().ok()?;
        if available.is_empty() || head.len() > 64 * 1024 {
            return None;
        }
        // Take bytes up to the end of the head and no further: what follows is the body.
        let wanted = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        head.extend_from_slice(&available[..wanted]);
        reader.consume(wanted);
    }
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(parsed.parse(&head), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    Some(Request {
        method: parsed.method?.to_string(),
        target: parsed.path?.to_string(),
        http11: parsed.version == Some(1),
        headers: parsed
            .headers
            .iter()
            .filter_map(|header| {
                Some((
                    header.name.to_string(),
                    std::str::from_utf8(header.value).ok()?.to_string(),
                ))
            })
            .collect(),
    })
}

// A request body in chunked transfer encoding, decoded as it is forwarded.
struct ChunkedBody<'a> {
    reader: &'a mut BufReader<TcpStream>,
    remaining: u64,
    done: bool,
}

impl ChunkedBody<'_> {
    fn line(&mut self) -> io::Result<String> {
        let mut line = String::new();
        if self.reader.by_ref().take(8192).read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(line.trim().to_string())
    }
}

impl Read for ChunkedBody<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.done || buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let line = self.line()?;
            let size = line.split(';').next().unwrap_or("").trim();
            self.remaining = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if self.remaining == 0 {
                // Trailers, if any, end with an empty line.
                while !self.line()?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
        }
        let limit = buffer
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let count = self.reader.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= count as u64;
        if self.remaining == 0 {
            self.line()?;
        }
        Ok(count)
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "",
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) -> io::Result<()> {
    write!(stream, "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", reason(status), body.len())?;
    stream.flush()
}

// A DNS name or IP literal, as the `/_/<host>/…` route accepts.
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._:[]".contains(c))
        && url::Url::parse(&format!("https://{host}/")).is_ok_and(|url| url.host_str().is_some())
}

pub struct ProxyService {
    tracker: Arc<Tracker>,
    routes: HashMap<String, String>,
    requested_port: u16,
    agent: ureq::Agent,
    address: Mutex<Option<SocketAddr>>,
    status: Mutex<String>,
    stopped: AtomicBool,
}

impl ProxyService {
    pub fn new(
        tracker: Arc<Tracker>,
        port: u16,
        extra_routes: Option<HashMap<String, String>>,
    ) -> Arc<ProxyService> {
        let mut routes = default_routes();
        routes.extend(extra_routes.unwrap_or_default());
        let mut agent = ureq::AgentBuilder::new().redirects(0);
        // Windows' own TLS. Without it only plain-HTTP upstreams (local models) can be reached.
        if let Ok(connector) = native_tls::TlsConnector::new() {
            agent = agent.tls_connector(Arc::new(connector));
        }
        Arc::new(ProxyService {
            tracker,
            routes,
            requested_port: port,
            agent: agent.build(),
            address: Mutex::new(None),
            status: Mutex::new("Stopped".into()),
            stopped: AtomicBool::new(false),
        })
    }

    /// The port being listened on, once started.
    pub fn port(&self) -> u16 {
        self.address
            .lock()
            .unwrap()
            .map_or(self.requested_port, |address| address.port())
    }
    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
    fn set_status(&self, status: impl Into<String>) {
        *self.status.lock().unwrap() = status.into();
    }

    /// Listens on the loopback address only. A port conflict is reported, never fatal to detection.
    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        if self.address.lock().unwrap().is_some() {
            return Ok(());
        }
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, self.requested_port)).inspect_err(|_| {
                self.set_status("Optional proxy unavailable; passive detection continues")
            })?;
        let address = listener.local_addr()?;
        *self.address.lock().unwrap() = Some(address);
        self.set_status(format!("Optional proxy on 127.0.0.1:{}", address.port()));
        let service = Arc::clone(self);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if service.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let service = Arc::clone(&service);
                std::thread::spawn(move || service.serve(stream));
            }
        });
        Ok(())
    }

    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        // Wake the accept loop so it sees the flag.
        if let Some(address) = *self.address.lock().unwrap() {
            let _ = TcpStream::connect(address);
        }
        self.set_status("Stopped");
    }

    fn serve(&self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let local = stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback());
        let mut reader = BufReader::new(stream);
        while let Some(request) = read_request(&mut reader) {
            if !self
                .forward(&request, local, &mut reader, &mut writer)
                .unwrap_or(false)
            {
                break;
            }
        }
    }

    // Handles one request. True when the connection can carry another; for now it never does.
    fn forward(
        &self,
        request: &Request,
        local: bool,
        reader: &mut BufReader<TcpStream>,
        writer: &mut TcpStream,
    ) -> io::Result<bool> {
        let refuse = |writer: &mut TcpStream, status: u16| {
            respond(writer, status, "text/plain", "").map(|_| false)
        };
        // Only this machine, and never a web page: browsers send Origin, and a page must not reach model APIs through here.
        let host = request.header("host").map(|host| {
            if host.starts_with('[') {
                host.split_inclusive(']').next().unwrap_or(host)
            } else {
                host.split(':').next().unwrap_or(host)
            }
        });
        if !local
            || request.header("origin").is_some()
            || !matches!(host, Some("127.0.0.1" | "localhost" | "[::1]"))
        {
            return refuse(writer, 403);
        }
        if request.header("upgrade").is_some() || !request.target.starts_with('/') {
            return refuse(writer, 400);
        }
        let (path, query) = match request.target.find('?') {
            Some(index) => request.target.split_at(index),
            None => (request.target.as_str(), ""),
        };
        if path == "/health" {
            return respond(
                writer,
                200,
                "application/json",
                &format!("{{\"port\":{}}}", self.port()),
            )
            .map(|_| false);
        }
        // /<route>[@<harness>]/<path> forwards to a named upstream; /_/<host>/<path> to any HTTPS host.
        let slash = path[1..].find('/').map(|index| index + 1);
        let mut tag = path[1..slash.unwrap_or(path.len())].splitn(2, '@');
        let mut route = tag.next().unwrap_or("").to_string();
        let harness_tag = tag.next();
        let mut rest = slash.map_or("/", |index| &path[index..]);
        let upstream = if route == "_" {
            let end = rest[1..].find('/').map(|index| index + 1);
            let host = &rest[1..end.unwrap_or(rest.len())];
            if !valid_host(host) {
                return refuse(writer, 400);
            }
            route = host.to_string();
            rest = end.map_or("/", |index| &rest[index..]);
            format!("https://{host}")
        } else {
            match self.routes.get(&route) {
                Some(upstream) => upstream.clone(),
                None => return refuse(writer, 404),
            }
        };
        let destination =
            match url::Url::parse(&format!("{}{rest}{query}", upstream.trim_end_matches('/'))) {
                Ok(url)
                    if matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none() =>
                {
                    url
                }
                _ => return refuse(writer, 400),
            };
        let harness = harness_tag
            .map(normalize_harness)
            .unwrap_or_else(|| guess_harness(request.header("user-agent").unwrap_or("")));
        let id = Uuid::new_v4();
        let key = format!("proxy:{id}");
        let started = Time::now();
        let clock = Instant::now();
        let is_generation = request.method == "POST";
        let mut meter = StreamMeasurement::new();
        let (mut status, mut headers_at, mut aborted) = (502u16, None, false);

        let mut upstream_request = self.agent.request_url(&request.method, &destination);
        for (name, value) in &request.headers {
            if !is_hop_header(name) && !name.eq_ignore_ascii_case("accept-encoding") {
                upstream_request = upstream_request.set(name, value);
            }
        }
        // The stream is read as it passes, so it must not be compressed.
        upstream_request = upstream_request.set("Accept-Encoding", "identity");
        let length = request
            .header("content-length")
            .and_then(|value| value.trim().parse::<u64>().ok());
        let chunked = request.header("transfer-encoding").is_some();
        if is_generation {
            self.tracker.update_proxy(LiveCall {
                id: key.clone(),
                harness: harness.clone(),
                model: "unknown".into(),
                provider: route.clone(),
                phase: "Waiting".into(),
                started_at: started,
                last_activity: started,
                ttft: None,
                rate: None,
                output_tokens: None,
                estimated: false,
            });
        }
        let sent = if chunked {
            upstream_request.send(ChunkedBody {
                reader: &mut *reader,
                remaining: 0,
                done: false,
            })
        } else {
            match length {
                Some(length) if length > 0 => upstream_request
                    .set("Content-Length", &length.to_string())
                    .send((&mut *reader).take(length)),
                _ => upstream_request.call(),
            }
        };
        // An error status is still a response to pass on.
        let response = match sent {
            Ok(response) | Err(ureq::Error::Status(_, response)) => Some(response),
            Err(ureq::Error::Transport(_)) => None,
        };
        match response {
            None => {
                aborted = true;
                respond(writer, 502, "text/plain", "")?;
            }
            Some(response) => {
                headers_at = Some(clock.elapsed().as_secs_f64());
                status = response.status();
                meter.set_content_type(response.content_type());
                let has_body = request.method != "HEAD" && !matches!(status, 100..=199 | 204 | 304);
                let mut head = format!("HTTP/1.1 {status} {}\r\n", response.status_text());
                for name in response.headers_names() {
                    if !is_hop_header(&name) {
                        for value in response.all(&name) {
                            head.push_str(&format!("{name}: {value}\r\n"));
                        }
                    }
                }
                // One request per connection, and the client is told, so it never reuses a socket that is about to close.
                head.push_str(if has_body && request.http11 {
                    "Connection: close\r\nTransfer-Encoding: chunked\r\n\r\n"
                } else {
                    "Connection: close\r\n\r\n"
                });
                let relay = || -> io::Result<()> {
                    writer.write_all(head.as_bytes())?;
                    if !has_body {
                        return writer.flush();
                    }
                    let mut body = response.into_reader();
                    let mut buffer = [0u8; 16 * 1024];
                    loop {
                        let count = body.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        meter.ingest(&buffer[..count], clock.elapsed().as_secs_f64());
                        if request.http11 {
                            write!(writer, "{count:x}\r\n")?;
                            writer.write_all(&buffer[..count])?;
                            writer.write_all(b"\r\n")?;
                        } else {
                            writer.write_all(&buffer[..count])?;
                        }
                        writer.flush()?;
                        if is_generation && meter.recognized() {
                            let phase = if meter.first.is_none() {
                                "Waiting"
                            } else if meter.visible.is_none() {
                                "Thinking"
                            } else {
                                "Streaming"
                            };
                            self.tracker.update_proxy(LiveCall {
                                id: key.clone(),
                                harness: harness.clone(),
                                model: meter.model.clone(),
                                provider: route.clone(),
                                phase: phase.into(),
                                started_at: started,
                                last_activity: Time::now(),
                                ttft: meter.first,
                                rate: meter.live_rate(),
                                output_tokens: Some(meter.output()),
                                estimated: meter.estimated(),
                            });
                        }
                    }
                    if request.http11 {
                        writer.write_all(b"0\r\n\r\n")?;
                    }
                    writer.flush()?;
                    meter.complete(clock.elapsed().as_secs_f64());
                    Ok(())
                };
                // Either side dropping mid-stream ends the call; what was measured is still recorded.
                if relay().is_err() {
                    aborted = true;
                }
            }
        }
        if is_generation {
            let mut record = None;
            if meter.recognized() {
                let total = clock.elapsed().as_secs_f64();
                let generation = match (meter.first, meter.last) {
                    (Some(first), Some(last)) if last > first => Some(last - first),
                    _ => None,
                };
                let rate = match generation {
                    Some(generation) if generation >= 0.05 => {
                        Some(meter.rate_tokens() / generation)
                    }
                    _ if total >= 0.05 => Some(meter.output() as f64 / total),
                    _ => None,
                };
                record = Some(RequestRecord {
                    id,
                    started_at: started,
                    harness,
                    route,
                    upstream_host: destination.host_str().unwrap_or("").to_string(),
                    format: meter.format.clone(),
                    model: meter.model.clone(),
                    streamed: meter.streamed,
                    status: status as i32,
                    ttft: meter.first.filter(|_| meter.streamed),
                    first_visible: meter.visible.filter(|_| meter.streamed),
                    ttfb: headers_at,
                    generation,
                    total,
                    input_tokens: meter.input,
                    cached_input_tokens: meter.cache,
                    output_tokens: meter.output(),
                    reasoning_tokens: meter.reasoning,
                    tokens_estimated: meter.estimated(),
                    tps: rate,
                    aborted: aborted || meter.failed,
                    source: Some("proxy".into()),
                    source_key: Some(key.clone()),
                });
            }
            if self.tracker.finish_proxy(&key, record).is_err() {
                self.set_status("Proxy measurement is queued; history is temporarily unavailable");
            }
        }
        Ok(false)
    }
}

impl Drop for ProxyService {
    fn drop(&mut self) {
        self.stop();
    }
}
