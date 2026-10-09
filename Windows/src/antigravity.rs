// Passive, read-only Antigravity telemetry, for both the editor and the agy terminal agent.
//
// Each conversation is a SQLite file in ~/.gemini/antigravity/conversations. Its gen_metadata table
// holds one protobuf per model call, and Antigravity measures the call itself: time to first token,
// streaming time and token counts. The steps table says when the call was made. Field numbers were
// read from the schema embedded in Antigravity 2.21 and checked against a real conversation.
//
// The same rows also hold the prompt. Only numbers, the model name and ids are decoded; text fields
// are stepped over without being turned into strings. Mirrors AntigravityLogs.swift.

use crate::domain::LiveCall;
use crate::parsers::LogRecord;
use crate::sqlite::{Connection, SqliteError};
use crate::time::Time;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// The newest call keeps its whole prompt until the next one replaces it.
const MAX_BLOB_BYTES: i64 = 64 * 1024 * 1024;
const MAX_ROWS_PER_POLL: usize = 256;
const WEEK: f64 = 7.0 * 86_400.0;

// Size and write time of the database and of its write-ahead log, if it has one.
type Stamp = (u64, Option<SystemTime>, Option<u64>, Option<SystemTime>);

#[derive(Default)]
struct Conversation {
    stamp: Option<Stamp>,
    // Calls below this index are finished with: emitted, or permanently unusable.
    next_index: i64,
    awaiting_since: Option<Time>,
}

pub struct AntigravityLogWatcher {
    roots: Vec<PathBuf>,
    conversations: HashMap<PathBuf, Conversation>,
    emitted: HashSet<String>,
    files: Vec<PathBuf>,
    last_scan: Time,
    latest_model_at: Time,
    has_logs: bool,
    limitation: Option<String>,
    latest_model: Option<String>,
    last_request_at: Option<Time>,
}

impl AntigravityLogWatcher {
    /// Each root is a folder of `<conversation id>.db` files.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> AntigravityLogWatcher {
        let mut unique: Vec<PathBuf> = Vec::new();
        for root in roots {
            if !unique.contains(&root) {
                unique.push(root);
            }
        }
        AntigravityLogWatcher {
            roots: unique,
            conversations: HashMap::new(),
            emitted: HashSet::new(),
            files: Vec::new(),
            last_scan: Time::MIN,
            latest_model_at: Time::MIN,
            has_logs: false,
            limitation: None,
            latest_model: None,
            last_request_at: None,
        }
    }
    pub fn has_logs(&self) -> bool {
        self.has_logs
    }
    pub fn limitation(&self) -> Option<&str> {
        self.limitation.as_deref()
    }
    pub fn latest_model(&self) -> Option<&str> {
        self.latest_model.as_deref()
    }
    pub fn last_request_at(&self) -> Option<Time> {
        self.last_request_at
    }

    /// A call has been made, by Antigravity's own record, and its reply is not complete.
    pub fn active(&self, now: Time) -> Vec<LiveCall> {
        let mut calls: Vec<LiveCall> = self
            .conversations
            .iter()
            .filter_map(|(path, conversation)| {
                let since = conversation
                    .awaiting_since
                    .filter(|since| *since <= now && now.since(*since) < 600.0)?;
                Some(LiveCall {
                    id: path.to_string_lossy().into_owned(),
                    harness: "Antigravity".into(),
                    model: self
                        .latest_model
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    provider: "google".into(),
                    phase: "Waiting".into(),
                    started_at: since,
                    last_activity: since,
                    ttft: None,
                    rate: None,
                    output_tokens: None,
                    estimated: false,
                })
            })
            .collect();
        calls.sort_by(|a, b| a.id.cmp(&b.id));
        calls
    }

    pub fn poll(&mut self, now: Time) -> Vec<LogRecord> {
        if now.since(self.last_scan) >= 4.0 {
            self.last_scan = now;
            self.files = self
                .roots
                .iter()
                .flat_map(|root| std::fs::read_dir(root).into_iter().flatten().flatten())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("db"))
                })
                .collect();
            self.files.sort();
            let files = &self.files;
            self.conversations.retain(|path, _| files.contains(path));
            if !self.has_logs {
                self.has_logs = self.probe();
            }
        }
        let cutoff = now.add_seconds(-WEEK);
        let mut records = Vec::new();
        self.limitation = None;
        for file in self.files.clone() {
            let Ok(metadata) = std::fs::metadata(&file) else {
                continue;
            };
            let log = std::fs::metadata(wal_path(&file)).ok();
            let stamp: Stamp = (
                metadata.len(),
                metadata.modified().ok(),
                log.as_ref().map(|log| log.len()),
                log.as_ref().and_then(|log| log.modified().ok()),
            );
            let touched = stamp
                .3
                .max(stamp.1)
                .map(Time::from_system)
                .unwrap_or(Time::MIN);
            let conversation = self.conversations.entry(file.clone()).or_default();
            // An untouched file has nothing new. Old conversations are never opened at all.
            if conversation.stamp == Some(stamp) || touched < cutoff {
                continue;
            }
            let name = file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            match Database::open(&file)
                .and_then(|database| database.read(&name, conversation.next_index))
            {
                Ok(result) => {
                    self.has_logs = true;
                    conversation.next_index = result.next_index;
                    conversation.awaiting_since = result.awaiting_since;
                    // More rows than one pass reads: leave the stamp unset so the next poll continues.
                    if !result.truncated {
                        conversation.stamp = Some(stamp);
                    }
                    if let Some(request) = result.last_request_at {
                        self.last_request_at = self.last_request_at.max(Some(request));
                    }
                    for record in result.records {
                        if record.end < cutoff || !self.emitted.insert(record.key.clone()) {
                            continue;
                        }
                        if record.end >= self.latest_model_at {
                            self.latest_model_at = record.end;
                            self.latest_model = Some(record.model.clone());
                        }
                        records.push(record);
                    }
                }
                // Busy, locked or not a conversation: come back on the next poll. Nothing is guessed.
                Err(_) => {
                    self.limitation =
                        Some("Antigravity conversation telemetry is temporarily unreadable.".into())
                }
            }
        }
        records.sort_by_key(|record| record.end);
        records
    }

    // Whether Antigravity keeps conversations here in a form this reader understands, even if none
    // is recent enough to read. One look at the newest file answers it.
    fn probe(&self) -> bool {
        let modified = |path: &PathBuf| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        };
        let Some(newest) = self.files.iter().max_by_key(|path| modified(path)) else {
            return false;
        };
        Database::open(newest)
            .and_then(|database| database.has_generations())
            .unwrap_or(false)
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

// SQLite URI filenames cannot name a network share, so those are opened the plain way.
fn immutable_uri(path: &Path) -> Option<String> {
    let normal = path.to_string_lossy().replace('\\', "/");
    if normal.starts_with("//") {
        return None;
    }
    let escape = |part: &str| -> String {
        part.bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                    (byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect()
    };
    // A drive letter keeps its colon; everything else is percent-encoded.
    let escaped: Vec<String> = normal
        .split('/')
        .enumerate()
        .map(|(index, part)| {
            if index == 0 && part.ends_with(':') {
                part.to_string()
            } else {
                escape(part)
            }
        })
        .collect();
    let escaped = escaped.join("/");
    Some(format!(
        "file:{}{escaped}?immutable=1",
        if escaped.starts_with('/') { "" } else { "/" }
    ))
}

struct ReadResult {
    records: Vec<LogRecord>,
    next_index: i64,
    truncated: bool,
    awaiting_since: Option<Time>,
    last_request_at: Option<Time>,
}

struct Database {
    connection: Connection,
}

impl Database {
    fn open(path: &Path) -> Result<Database, SqliteError> {
        // These files are in write-ahead-log mode. Opening one normally makes SQLite create its -shm
        // and -wal companions, and a read-only connection cannot remove them again, so a plain open
        // would leave files behind in Antigravity's folder. With no log present the main file is
        // complete on its own and is read as an immutable snapshot, which creates nothing.
        let source = if wal_path(path).exists() {
            None
        } else {
            immutable_uri(path)
        }
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Ok(Database {
            connection: Connection::open_readonly(&source)?,
        })
    }

    fn has_generations(&self) -> Result<bool, SqliteError> {
        Ok(self.connection.scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('gen_metadata', 'steps')")? == 2)
    }

    fn read(&self, conversation: &str, first: i64) -> Result<ReadResult, SqliteError> {
        let mut rows: Vec<(i64, i64)> = Vec::new();
        {
            let mut command = self.connection.prepare(
                "SELECT idx, length(data) FROM gen_metadata WHERE idx >= ?1 ORDER BY idx LIMIT ?2",
            )?;
            command.bind_int(1, first)?;
            command.bind_int(2, MAX_ROWS_PER_POLL as i64 + 1)?;
            while command.step()? {
                rows.push((
                    command.int(0),
                    if command.is_null(1) {
                        0
                    } else {
                        command.int(1)
                    },
                ));
            }
        }
        let truncated = rows.len() > MAX_ROWS_PER_POLL;
        let newest = if truncated {
            None
        } else {
            rows.last().map(|(index, _)| *index)
        };
        let (mut records, mut next, mut blocked) = (Vec::new(), first, false);
        for (index, size) in rows.iter().take(MAX_ROWS_PER_POLL).copied() {
            let mut finished = true;
            if size > 0 && size <= MAX_BLOB_BYTES {
                if let Some(generation) = self
                    .blob("SELECT data FROM gen_metadata WHERE idx = ?1", index)?
                    .and_then(|data| AntigravityGeneration::parse(&data))
                {
                    if generation.is_complete() {
                        let started = match generation.step_indices.first() {
                            Some(step) => self.step_time(*step, 1)?,
                            None => None,
                        };
                        records.extend(
                            started.and_then(|started| {
                                generation.record(conversation, index, started)
                            }),
                        );
                    } else if Some(index) == newest {
                        // Written before its reply finished; look again when the file changes. An unfinished
                        // call with later calls after it was abandoned and never will be.
                        finished = false;
                    }
                }
            }
            if !finished {
                blocked = true;
            }
            if !blocked {
                next = index + 1;
            }
        }
        // The newest model step tells when the last request went out, and whether its reply is still due.
        // CortexStepMetadata: created_at = 1, completed_at = 8. Step type 15 is the model's response.
        let (mut awaiting, mut last_request) = (None, None);
        let latest = {
            let mut command = self
                .connection
                .prepare("SELECT idx FROM steps WHERE step_type = 15 ORDER BY idx DESC LIMIT 1")?;
            if command.step()? && !command.is_null(0) {
                Some(command.int(0))
            } else {
                None
            }
        };
        if let Some(latest) = latest {
            if let Some(created) = self.step_time(latest, 1)? {
                last_request = Some(created);
                if self.step_time(latest, 8)?.is_none() {
                    awaiting = Some(created);
                }
            }
        }
        Ok(ReadResult {
            records,
            next_index: next,
            truncated,
            awaiting_since: awaiting,
            last_request_at: last_request,
        })
    }

    fn step_time(&self, index: i64, field: u32) -> Result<Option<Time>, SqliteError> {
        let Some(data) = self.blob(
            "SELECT metadata FROM steps WHERE idx = ?1 AND length(metadata) <= 1048576",
            index,
        )?
        else {
            return Ok(None);
        };
        Ok(protobuf::fields(&data).and_then(|fields| {
            fields
                .iter()
                .find(|candidate| candidate.number == field && candidate.is_bytes)
                .and_then(|candidate| protobuf::timestamp(candidate.bytes(&data)))
        }))
    }

    fn blob(&self, sql: &str, index: i64) -> Result<Option<Vec<u8>>, SqliteError> {
        let mut command = self.connection.prepare(sql)?;
        command.bind_int(1, index)?;
        Ok(if command.step()? {
            command.blob(0)
        } else {
            None
        })
    }
}

/// What one gen_metadata row says about a model call.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AntigravityGeneration {
    pub step_indices: Vec<i64>,
    pub execution_id: String,
    pub model: Option<String>,
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub cache_write_tokens: Option<i32>,
    pub cache_read_tokens: Option<i32>,
    pub thinking_tokens: Option<i32>,
    pub time_to_first_token: Option<f64>,
    pub streaming_duration: Option<f64>,
}

impl AntigravityGeneration {
    /// A stream shorter than this is one or two network chunks: it shows that the reply arrived,
    /// not how fast it was written.
    pub const SHORTEST_TIMED_STREAM: f64 = 1.0;

    // CortexStepGeneratorMetadata: chat_model = 1, step_indices = 2, execution_id = 4.
    pub fn parse(data: &[u8]) -> Option<AntigravityGeneration> {
        let mut value = AntigravityGeneration::default();
        let mut chat_model = None;
        for field in protobuf::fields(data)? {
            match (field.number, field.is_bytes) {
                (1, true) => chat_model = Some(field.bytes(data)),
                (2, true) => value.step_indices.extend(
                    protobuf::packed_varints(field.bytes(data))
                        .into_iter()
                        .map(|index| index as i64),
                ),
                (2, false) => value.step_indices.push(field.value as i64),
                (4, true) => {
                    value.execution_id = protobuf::identifier(field.bytes(data)).unwrap_or_default()
                }
                _ => {}
            }
        }
        // ChatModelMetadata: usage = 4, time_to_first_token = 11, streaming_duration = 12, response_model = 19.
        let chat = chat_model?;
        for field in protobuf::fields(chat)?
            .into_iter()
            .filter(|field| field.is_bytes)
        {
            let bytes = field.bytes(chat);
            match field.number {
                4 => value.read_usage(bytes),
                11 => value.time_to_first_token = protobuf::duration(bytes),
                12 => value.streaming_duration = protobuf::duration(bytes),
                19 => value.model = protobuf::identifier(bytes),
                _ => {}
            }
        }
        Some(value)
    }

    // ModelUsageStats: input = 2, output = 3, cache_write = 4, cache_read = 5, thinking_output = 9.
    // Output already includes thinking.
    fn read_usage(&mut self, data: &[u8]) {
        for field in protobuf::fields(data).unwrap_or_default() {
            if field.is_bytes || field.value > i32::MAX as u64 {
                continue;
            }
            let number = Some(field.value as i32);
            match field.number {
                2 => self.input_tokens = number,
                3 => self.output_tokens = number,
                4 => self.cache_write_tokens = number,
                5 => self.cache_read_tokens = number,
                9 => self.thinking_tokens = number,
                _ => {}
            }
        }
    }

    /// The call is finished: Antigravity writes usage and timing when the reply ends.
    pub fn is_complete(&self) -> bool {
        self.output_tokens.is_some_and(|output| output > 0)
            && (self.time_to_first_token.is_some() || self.streaming_duration.is_some())
    }

    pub fn record(&self, conversation: &str, index: i64, started_at: Time) -> Option<LogRecord> {
        let output = self.output_tokens.filter(|_| self.is_complete())?;
        let streaming = self.streaming_duration.unwrap_or(0.0).max(0.0);
        let first_token = self
            .time_to_first_token
            .map(|ttft| started_at.add_seconds(ttft.max(0.0)));
        let end = first_token.unwrap_or(started_at).add_seconds(streaming);
        let total = end.since(started_at);
        let thinking = self.thinking_tokens.unwrap_or(0).min(output);
        // Antigravity's first token is the first visible one: thinking is over by then. So the streaming
        // window holds only the visible reply, and counting every output token in it would inflate the
        // rate. When the window is too short to be a rate at all, the honest figure is the whole call.
        let speed = if streaming >= Self::SHORTEST_TIMED_STREAM && output > thinking {
            Some((output - thinking) as f64 / streaming)
        } else if total >= 0.05 {
            Some(output as f64 / total)
        } else {
            None
        };
        let input = self
            .input_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_write_tokens.unwrap_or(0));
        Some(LogRecord {
            key: format!(
                "antigravity:{conversation}:{}:{index}",
                if self.execution_id.is_empty() {
                    "-"
                } else {
                    &self.execution_id
                }
            ),
            harness: "Antigravity".into(),
            model: self.model.clone().unwrap_or_else(|| "unknown".into()),
            provider: "google".into(),
            request_start: Some(started_at),
            first_token,
            first_visible: first_token,
            end,
            output_tokens: output,
            reasoning_tokens: Some(thinking).filter(|thinking| *thinking > 0),
            input_tokens: Some(input).filter(|input| *input > 0),
            cached_input_tokens: self.cache_read_tokens,
            aborted: false,
            speed,
        })
    }
}

/// Just enough of the protobuf wire format to pick numbered fields out of a message without its
/// schema. Unknown fields are stepped over, never interpreted.
pub mod protobuf {
    use crate::time::Time;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Field {
        pub number: u32,
        pub is_bytes: bool,
        pub value: u64,
        offset: usize,
        length: usize,
    }

    impl Field {
        pub fn bytes<'a>(&self, parent: &'a [u8]) -> &'a [u8] {
            &parent[self.offset..self.offset + self.length]
        }
    }

    /// The top-level fields of a message, or None if the bytes are not a well-formed one.
    pub fn fields(data: &[u8]) -> Option<Vec<Field>> {
        let mut fields = Vec::new();
        let mut index = 0;
        while index < data.len() {
            let key = varint(data, &mut index)?;
            let number = u32::try_from(key >> 3)
                .ok()
                .filter(|number| *number > 0 && *number <= i32::MAX as u32)?;
            let scalar = |value: u64| Field {
                number,
                is_bytes: false,
                value,
                offset: 0,
                length: 0,
            };
            match key & 7 {
                0 => fields.push(scalar(varint(data, &mut index)?)),
                1 => {
                    fields.push(scalar(u64::from_le_bytes(
                        data.get(index..index + 8)?.try_into().ok()?,
                    )));
                    index += 8;
                }
                2 => {
                    let length = usize::try_from(varint(data, &mut index)?)
                        .ok()
                        .filter(|length| *length <= data.len() - index)?;
                    fields.push(Field {
                        number,
                        is_bytes: true,
                        value: 0,
                        offset: index,
                        length,
                    });
                    index += length;
                }
                5 => {
                    fields.push(scalar(u32::from_le_bytes(
                        data.get(index..index + 4)?.try_into().ok()?,
                    ) as u64));
                    index += 4;
                }
                _ => return None,
            }
        }
        Some(fields)
    }

    pub fn packed_varints(data: &[u8]) -> Vec<u64> {
        let mut values = Vec::new();
        let mut index = 0;
        while index < data.len() {
            match varint(data, &mut index) {
                Some(value) => values.push(value),
                None => break,
            }
        }
        values
    }

    /// google.protobuf.Duration: seconds = 1, nanos = 2.
    pub fn duration(data: &[u8]) -> Option<f64> {
        let (seconds, nanos) = seconds_and_nanos(data)?;
        let value = seconds as f64 + nanos as f64 / 1_000_000_000.0;
        (0.0..86_400.0).contains(&value).then_some(value)
    }

    /// google.protobuf.Timestamp: seconds = 1, nanos = 2.
    pub fn timestamp(data: &[u8]) -> Option<Time> {
        let (seconds, nanos) = seconds_and_nanos(data)?;
        (seconds > 1_000_000_000 && seconds < 10_000_000_000).then(|| {
            Time(
                Time::from_unix_seconds(seconds)
                    .0
                    .saturating_add(nanos / 100),
            )
        })
    }

    /// A short ASCII token such as a model name or UUID. Anything else is refused, so free text sitting
    /// in a neighbouring field can never be mistaken for one.
    pub fn identifier(data: &[u8]) -> Option<String> {
        if data.is_empty()
            || data.len() > 128
            || !data
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._/:".contains(byte))
        {
            return None;
        }
        String::from_utf8(data.to_vec()).ok()
    }

    fn seconds_and_nanos(data: &[u8]) -> Option<(i64, i64)> {
        let (mut seconds, mut nanos) = (0, 0);
        for field in fields(data)? {
            if field.is_bytes {
                return None;
            }
            match field.number {
                1 => seconds = field.value as i64,
                2 => nanos = field.value as i64,
                _ => {}
            }
        }
        Some((seconds, nanos))
    }

    fn varint(data: &[u8], index: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        while *index < data.len() && shift < 64 {
            let byte = data[*index];
            *index += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
        }
        None
    }
}
