use crate::discovery::{self, FileWalk, Pattern};
use crate::domain::LiveCall;
use crate::json::{add, Json};
use crate::logs::error_name;
use crate::parsers::LogRecord;
use crate::sqlite::{Connection, SqliteError, Statement};
use crate::time::{earlier, Time, MAX_UNIX_MS};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAXIMUM_REMEMBERED: usize = 32_768;
const MAXIMUM_PENDING: usize = 512;
const MAXIMUM_FILE_BYTES: u64 = 1024 * 1024;
const WEEK: f64 = 7.0 * 86_400.0;
const TEN_MINUTES: f64 = 600.0;

enum ReadError {
    Sqlite,
    Io(io::Error),
    Json,
}

impl ReadError {
    fn name(&self) -> &'static str {
        match self {
            ReadError::Sqlite => "SqliteException",
            ReadError::Io(error) => error_name(error),
            ReadError::Json => "JsonException",
        }
    }
}

impl From<SqliteError> for ReadError {
    fn from(_: SqliteError) -> Self {
        ReadError::Sqlite
    }
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        ReadError::Io(error)
    }
}

impl From<serde_json::Error> for ReadError {
    fn from(_: serde_json::Error) -> Self {
        ReadError::Json
    }
}

struct PendingFile {
    file: PathBuf,
    storage: PathBuf,
    updated: Time,
}

struct Part {
    kind: String,
    start: Option<Time>,
    created: Option<Time>,
    persisted: Option<Time>,
    synthetic: bool,
    ignored: bool,
    status: Option<String>,
}

impl Part {
    fn read(value: &Value, persisted: Option<Time>) -> Part {
        let time = value.get_or_null("time");
        let flag = |name: &str| value.b(name) || value.n(name) == Some(1.0);
        Part {
            kind: value.s("type").unwrap_or("").to_string(),
            start: time.get_or_null("start").date(),
            created: time.get_or_null("created").date(),
            persisted,
            synthetic: flag("synthetic"),
            ignored: flag("ignored"),
            status: value.get_or_null("state").s("status").map(str::to_string),
        }
    }
}

struct Telemetry {
    id: String,
    session: String,
    model: String,
    provider: String,
    created: Time,
    updated: Time,
    // The session_message (v2) schema, where creation is first-content time, not dispatch time.
    current: bool,
    awaiting: bool,
    first: Option<Time>,
    visible: Option<Time>,
    record: Option<LogRecord>,
}

impl Telemetry {
    fn read(
        id: &str,
        session: &str,
        data: &Value,
        parts: &[Part],
        current: bool,
        updated: Option<Time>,
        complete: bool,
    ) -> Option<Telemetry> {
        let model = if current {
            data.get_or_null("model").s("id")
        } else {
            data.s("modelID")
        }
        .filter(|model| !model.is_empty())?;
        let provider = if current {
            data.get_or_null("model").s("providerID")
        } else {
            data.s("providerID")
        }
        .unwrap_or("");
        let time = data.get_or_null("time");
        let created = time.get_or_null("created").date()?;
        let completed = time.get_or_null("completed").date();
        let finish = data.s("finish");
        let failed = !data.get_or_null("error").is_null() || finish == Some("error");
        let terminal = completed.is_some() || finish.is_some() || failed;
        // A tool that is running or done means the model's turn is over; tool execution is not generation.
        let tool_ran = parts.iter().any(|part| {
            part.kind == "tool"
                && matches!(
                    part.status.as_deref(),
                    Some("running" | "completed" | "error")
                )
        });
        let awaiting = complete
            && !terminal
            && !data.b("summary")
            && data.n("summary") != Some(1.0)
            && !tool_ran;
        let (mut first, mut visible);
        if current {
            first = parts
                .first()
                .filter(|part| matches!(part.kind.as_str(), "reasoning" | "tool"))
                .and_then(|part| part.created);
            visible = parts
                .iter()
                .find(|part| matches!(part.kind.as_str(), "text" | "tool"))
                .filter(|part| part.kind == "tool")
                .and_then(|part| part.created);
        } else {
            let generated = || {
                parts.iter().filter(|part| {
                    matches!(part.kind.as_str(), "reasoning" | "text")
                        && !part.synthetic
                        && !part.ignored
                })
            };
            first = generated().fold(None, |first, part| earlier(first, part.start));
            visible = generated()
                .filter(|part| part.kind == "text")
                .fold(None, |first, part| earlier(first, part.start));
        }
        let end = if current {
            completed
        } else {
            parts
                .iter()
                .filter(|part| part.kind == "step-finish")
                .filter_map(|part| part.persisted)
                .max()
                .or(completed)
        };
        let tokens = data.get_or_null("tokens");
        let cache = tokens.get_or_null("cache");
        let reasoning = tokens.i("reasoning");
        let total_output = tokens
            .i("output")
            .and_then(|output| add(Some(output), reasoning, None));
        let input = tokens
            .i("input")
            .and_then(|uncached| add(Some(uncached), cache.i("read"), cache.i("write")));
        let mut record = None;
        if let (true, Some(end), Some(output)) =
            (terminal, end.filter(|end| *end >= created), total_output)
        {
            if output > 0 || input.is_some_and(|input| input > 0) {
                let within = |date: Option<Time>| {
                    date.filter(|date| complete && *date >= created && *date <= end)
                };
                first = within(first);
                visible = within(visible);
                record = Some(LogRecord {
                    key: format!("opencode:{id}"),
                    harness: "opencode".into(),
                    model: model.to_string(),
                    provider: provider.to_string(),
                    request_start: (!current).then_some(created),
                    first_token: first,
                    first_visible: visible,
                    end,
                    output_tokens: output,
                    reasoning_tokens: reasoning,
                    input_tokens: input,
                    cached_input_tokens: cache.i("read"),
                    aborted: failed || matches!(finish, Some("abort" | "aborted" | "cancelled")),
                    speed: None,
                });
            }
        }
        let activity = updated.or(completed).unwrap_or(created).max(created);
        Some(Telemetry {
            id: id.to_string(),
            session: session.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
            created,
            updated: activity,
            current,
            awaiting,
            first,
            visible,
            record,
        })
    }
}

// Keeps the newest `maximum` entries.
fn trim<V>(values: &mut IndexMap<String, V>, maximum: usize, date: impl Fn(&V) -> Time) {
    if values.len() <= maximum {
        return;
    }
    let mut dated: Vec<(Time, String)> = values
        .iter()
        .map(|(key, value)| (date(value), key.clone()))
        .collect();
    dated.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, key) in dated.into_iter().skip(maximum) {
        values.shift_remove(&key);
    }
}

/// Read-only opencode telemetry: the current `session_message` and legacy `message`/`part` SQLite
/// tables, and the older per-entity JSON files. Projections never return prompts, text, tool inputs
/// or credentials.
pub struct OpenCodeLogWatcher {
    roots: Vec<PathBuf>,
    databases: IndexMap<String, Database>,
    json_revisions: IndexMap<String, (u64, Time)>,
    scans: HashMap<PathBuf, FileWalk>,
    next_scan: HashMap<PathBuf, Time>,
    pending_files: IndexMap<String, PendingFile>,
    active: IndexMap<String, (Telemetry, LiveCall)>,
    completed: IndexMap<String, Time>,
    latest_session: IndexMap<String, (Time, String)>,
    json_bytes_remaining: i64,
    readable_json: bool,
    has_logs: bool,
    limitation: Option<String>,
}

impl OpenCodeLogWatcher {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> OpenCodeLogWatcher {
        let mut unique: Vec<PathBuf> = Vec::new();
        for root in roots {
            if !unique.contains(&root) {
                unique.push(root);
            }
        }
        OpenCodeLogWatcher {
            roots: unique,
            databases: IndexMap::new(),
            json_revisions: IndexMap::new(),
            scans: HashMap::new(),
            next_scan: HashMap::new(),
            pending_files: IndexMap::new(),
            active: IndexMap::new(),
            completed: IndexMap::new(),
            latest_session: IndexMap::new(),
            json_bytes_remaining: 0,
            readable_json: false,
            has_logs: false,
            limitation: None,
        }
    }
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
    pub fn has_logs(&self) -> bool {
        self.has_logs
    }
    pub fn limitation(&self) -> Option<&str> {
        self.limitation.as_deref()
    }

    /// Unfinished assistant messages that are the newest in their session.
    pub fn active(&self, now: Time) -> Vec<LiveCall> {
        self.active
            .values()
            .filter(|(telemetry, call)| {
                self.latest_session
                    .get(&telemetry.session)
                    .is_some_and(|(_, id)| *id == telemetry.id)
                    && call.started_at <= now
                    && now.since(call.last_activity) < TEN_MINUTES
                    && now.since(call.started_at) < TEN_MINUTES
            })
            .map(|(_, call)| call.clone())
            .collect()
    }

    pub fn poll(&mut self, now: Time) -> Vec<LogRecord> {
        let mut records = Vec::new();
        let cutoff = now.add_seconds(-WEEK);
        self.json_bytes_remaining = 4 * 1024 * 1024;
        self.limitation = None;
        for root in self.roots.clone() {
            let is_database = root
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("db"));
            let path = if is_database {
                root.clone()
            } else {
                root.join("opencode.db")
            };
            let key = path.to_string_lossy().into_owned();
            if path.is_file() {
                if let Err(error) = self.poll_database(&path, &key, cutoff, &mut records) {
                    self.databases.shift_remove(&key);
                    self.limitation = Some(format!(
                        "OpenCode database telemetry unavailable: {}.",
                        error.name()
                    ));
                    self.remove_active_source(&key);
                }
            } else if self.databases.shift_remove(&key).is_some() {
                self.remove_active_source(&key);
            }
            if is_database {
                continue;
            }
            let storage = if root.file_name().is_some_and(|name| name == "storage") {
                root.clone()
            } else {
                root.join("storage")
            };
            self.scan_json(&storage, cutoff, now, &mut records);
        }
        // Messages whose usage had not arrived yet are looked at again.
        let waiting: Vec<(PathBuf, PathBuf)> = self
            .pending_files
            .values()
            .map(|pending| (pending.file.clone(), pending.storage.clone()))
            .collect();
        for (file, storage) in waiting {
            if self.json_bytes_remaining <= 0 {
                break;
            }
            self.read_json(&file, &storage, cutoff, &mut records);
        }
        self.active.retain(|_, (telemetry, call)| {
            !(telemetry.updated < cutoff || now.since(call.started_at) >= TEN_MINUTES)
        });
        self.pending_files
            .retain(|_, pending| pending.updated >= cutoff);
        self.completed.retain(|_, end| *end >= cutoff);
        self.latest_session
            .retain(|_, (created, _)| *created >= cutoff);
        trim(&mut self.completed, MAXIMUM_REMEMBERED, |end| *end);
        trim(
            &mut self.latest_session,
            MAXIMUM_REMEMBERED,
            |(created, _)| *created,
        );
        trim(
            &mut self.json_revisions,
            MAXIMUM_REMEMBERED,
            |(_, modified)| *modified,
        );
        trim(&mut self.pending_files, MAXIMUM_PENDING, |pending| {
            pending.updated
        });
        trim(&mut self.active, MAXIMUM_PENDING, |(telemetry, _)| {
            telemetry.updated
        });
        self.has_logs = self.readable_json
            || self
                .databases
                .values()
                .any(|database| database.has_telemetry);
        records.sort_by_key(|record| record.end);
        records
    }

    fn poll_database(
        &mut self,
        path: &Path,
        key: &str,
        cutoff: Time,
        records: &mut Vec<LogRecord>,
    ) -> Result<(), ReadError> {
        // A database replaced by another file needs a new connection and a new read.
        if self
            .databases
            .get(key)
            .is_some_and(|database| !database.is_current_file())
        {
            self.databases.shift_remove(key);
            self.remove_active_source(key);
        }
        if !self.databases.contains_key(key) {
            self.databases
                .insert(key.to_string(), Database::open(path, cutoff)?);
        }
        let found = self
            .databases
            .get_mut(key)
            .expect("inserted above")
            .poll()?;
        let source = format!("{key}:");
        for telemetry in found {
            self.accept(telemetry, cutoff, records, &source);
        }
        Ok(())
    }

    fn remove_active_source(&mut self, path: &str) {
        let prefix = format!("{path}:");
        self.active
            .retain(|_, (_, call)| !call.id.starts_with(&prefix));
    }

    fn accept(
        &mut self,
        telemetry: Telemetry,
        cutoff: Time,
        records: &mut Vec<LogRecord>,
        source: &str,
    ) {
        if telemetry.updated < cutoff {
            return;
        }
        let newer = self
            .latest_session
            .get(&telemetry.session)
            .is_none_or(|(created, id)| {
                telemetry.created > *created
                    || (telemetry.created == *created && telemetry.id >= *id)
            });
        if newer {
            self.latest_session.insert(
                telemetry.session.clone(),
                (telemetry.created, telemetry.id.clone()),
            );
        }
        let id = telemetry.id.clone();
        let record = telemetry.record.clone();
        if telemetry.awaiting && !self.completed.contains_key(&id) {
            let phase = if telemetry.current {
                "Streaming · log timing limited"
            } else if telemetry.first.is_none() {
                "Waiting"
            } else if telemetry.visible.is_none() {
                "Thinking"
            } else {
                "Streaming"
            };
            let call = LiveCall {
                id: format!("{source}{id}"),
                harness: "opencode".into(),
                model: telemetry.model.clone(),
                provider: telemetry.provider.clone(),
                phase: phase.into(),
                started_at: telemetry.created,
                last_activity: telemetry.updated,
                ttft: telemetry
                    .first
                    .filter(|first| !telemetry.current && *first >= telemetry.created)
                    .map(|first| first.since(telemetry.created)),
                rate: None,
                output_tokens: None,
                estimated: false,
            };
            self.active.insert(id.clone(), (telemetry, call));
        } else {
            self.active.shift_remove(&id);
        }
        let Some(record) = record else { return };
        self.pending_files.shift_remove(&id);
        if record.end >= cutoff && !self.completed.contains_key(&id) {
            self.completed.insert(id, record.end);
            records.push(record);
        }
    }

    fn scan_json(&mut self, storage: &Path, cutoff: Time, now: Time, records: &mut Vec<LogRecord>) {
        if !self.scans.contains_key(storage) {
            if self.next_scan.get(storage).is_some_and(|next| now < *next) {
                return;
            }
            self.scans.insert(
                storage.to_path_buf(),
                discovery::files(&storage.join("message"), Pattern::Json),
            );
            self.next_scan
                .insert(storage.to_path_buf(), now.add_seconds(4.0));
        }
        // The walk is resumed across polls: a few hundred files at a time, within the byte budget.
        let mut visited = 0;
        while visited < 256 && self.json_bytes_remaining > 0 {
            visited += 1;
            let Some(file) = self.scans.get_mut(storage).and_then(Iterator::next) else {
                self.scans.remove(storage);
                break;
            };
            let Ok(metadata) = std::fs::metadata(&file) else {
                continue;
            };
            let modified = metadata
                .modified()
                .map(Time::from_system)
                .unwrap_or(Time(0));
            if metadata.len() > MAXIMUM_FILE_BYTES || (modified < cutoff && self.readable_json) {
                continue;
            }
            let key = file.to_string_lossy().into_owned();
            let revision = (metadata.len(), modified);
            if self.json_revisions.get(&key) == Some(&revision) {
                continue;
            }
            if self.read_json(&file, storage, cutoff, records) {
                self.json_revisions.insert(key, revision);
            }
        }
    }

    // True when the file was dealt with and need not be read again until it changes.
    fn read_json(
        &mut self,
        file: &Path,
        storage: &Path,
        cutoff: Time,
        records: &mut Vec<LogRecord>,
    ) -> bool {
        match self.read_json_message(file, storage, cutoff, records) {
            Ok(done) => done,
            Err(error) => {
                self.limitation = Some(format!(
                    "OpenCode JSON telemetry unavailable: {}.",
                    error.name()
                ));
                false
            }
        }
    }

    fn read_json_message(
        &mut self,
        file: &Path,
        storage: &Path,
        cutoff: Time,
        records: &mut Vec<LogRecord>,
    ) -> Result<bool, ReadError> {
        let Some(data) = self.json_file(file)? else {
            return Ok(false);
        };
        // The ids become folder names below, so anything that could leave the storage folder is refused.
        let (Some("assistant"), Some(id), Some(session)) = (
            data.s("role"),
            data.s("id").filter(|id| safe_component(id)),
            data.s("sessionID")
                .filter(|session| safe_component(session)),
        ) else {
            return Ok(true);
        };
        let mut parts = Vec::new();
        let mut complete = true;
        let mut walk = discovery::files(&storage.join("part").join(id), Pattern::Json);
        for part_file in walk.by_ref().take(128) {
            match self.json_file(&part_file)? {
                Some(part) => parts.push(Part::read(&part, None)),
                None => complete = false,
            }
        }
        if walk.next().is_some() {
            complete = false;
        }
        let updated = Time::from_system(std::fs::metadata(file)?.modified()?);
        let Some(telemetry) =
            Telemetry::read(id, session, &data, &parts, false, Some(updated), complete)
        else {
            return Ok(true);
        };
        self.readable_json = true;
        if telemetry.record.is_none() {
            self.pending_files.insert(
                id.to_string(),
                PendingFile {
                    file: file.to_path_buf(),
                    storage: storage.to_path_buf(),
                    updated: telemetry.updated,
                },
            );
        }
        self.accept(telemetry, cutoff, records, "");
        Ok(true)
    }

    // A whole small JSON file, or None when it is too large, over this poll's budget, or changed while being read.
    fn json_file(&mut self, path: &Path) -> Result<Option<Value>, ReadError> {
        let Ok(metadata) = std::fs::metadata(path) else {
            return Ok(None);
        };
        let size = metadata.len();
        if size > MAXIMUM_FILE_BYTES || size as i64 + 1 > self.json_bytes_remaining {
            return Ok(None);
        }
        // One byte more than the size, to notice a file that grew in the meantime.
        let mut bytes = Vec::with_capacity(size as usize + 1);
        std::fs::File::open(path)?
            .take(size + 1)
            .read_to_end(&mut bytes)?;
        self.json_bytes_remaining -= bytes.len() as i64;
        if bytes.len() as u64 != size {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', ':'])
}

struct Table {
    name: &'static str,
    indexed_history: bool,
    session_history: bool,
    // Rows above this were inserted after the watcher first looked.
    high_row: i64,
    fallback_row: i64,
    history_time: i64,
    history_id: String,
    history_done: bool,
    probe_done: bool,
    session_row: i64,
    sessions_done: bool,
    sessions: VecDeque<String>,
    work: VecDeque<String>,
    queued: HashSet<String>,
    // Messages seen without a finished record yet; read again whenever the database changes.
    pending: IndexMap<String, Time>,
}

impl Table {
    fn new(name: &'static str, high: i64, cutoff: i64) -> Table {
        Table {
            name,
            indexed_history: false,
            session_history: false,
            high_row: high,
            fallback_row: high.saturating_add(1),
            history_time: cutoff,
            history_id: String::new(),
            history_done: false,
            probe_done: false,
            session_row: 0,
            sessions_done: false,
            sessions: VecDeque::new(),
            work: VecDeque::new(),
            queued: HashSet::new(),
            pending: IndexMap::new(),
        }
    }
    fn current(&self) -> bool {
        self.name == "session_message"
    }
    fn enqueue(&mut self, id: String) {
        if self.queued.insert(id.clone()) {
            self.work.push_back(id);
        }
    }
}

const BATCH: usize = 128;
const MAX_JSON_BYTES: i64 = 4 * 1024 * 1024;

struct Database {
    path: PathBuf,
    creation: Option<SystemTime>,
    connection: Connection,
    tables: Vec<Table>,
    has_part: bool,
    schema_version: i64,
    version: i64,
    cutoff: i64,
    bytes_remaining: i64,
    has_telemetry: bool,
}

fn created(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.created())
        .ok()
}

// A millisecond timestamp column, stored as an integer or as text.
fn milliseconds(statement: &Statement<'_>, column: i32) -> Option<Time> {
    let value: i64 = statement.text(column)?.parse().ok()?;
    (value > 0 && (value as f64) < MAX_UNIX_MS).then(|| Time::from_unix_ms(value))
}

impl Database {
    fn open(path: &Path, from: Time) -> Result<Database, SqliteError> {
        Ok(Database {
            path: path.to_path_buf(),
            creation: created(path),
            connection: Connection::open_readonly(&path.to_string_lossy())?,
            tables: Vec::new(),
            has_part: false,
            schema_version: -1,
            version: -1,
            cutoff: from.unix_ms(),
            bytes_remaining: 0,
            has_telemetry: false,
        })
    }

    fn is_current_file(&self) -> bool {
        self.path.is_file() && created(&self.path) == self.creation
    }

    fn strings(
        &self,
        sql: &str,
        column: i32,
        parameter: Option<&str>,
    ) -> Result<Vec<String>, SqliteError> {
        let mut statement = self.connection.prepare(sql)?;
        if let Some(parameter) = parameter {
            statement.bind_text(1, parameter)?;
        }
        let mut values = Vec::new();
        while statement.step()? {
            values.extend(statement.text(column));
        }
        Ok(values)
    }

    fn columns(&self, table: &str) -> Result<HashSet<String>, SqliteError> {
        Ok(self
            .strings(&format!("PRAGMA table_info(\"{table}\")"), 1, None)?
            .into_iter()
            .collect())
    }

    // The column lists of every index on a table.
    fn index_columns(&self, table: &str) -> Result<Vec<Vec<String>>, SqliteError> {
        let names = self.strings(
            "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?1",
            0,
            Some(table),
        )?;
        names
            .iter()
            .map(|name| {
                self.strings(
                    &format!("PRAGMA index_info('{}')", name.replace('\'', "''")),
                    2,
                    None,
                )
            })
            .collect()
    }

    fn discover(&mut self) -> Result<(), SqliteError> {
        let names: HashSet<String> = self.strings("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('message','session_message','session','part')", 0, None)?.into_iter().collect();
        let has_all = |columns: &HashSet<String>, wanted: &[&str]| {
            wanted.iter().all(|name| columns.contains(*name))
        };
        // Parts are fetched per message, so without an index on message_id they are left alone.
        self.has_part = names.contains("part")
            && has_all(
                &self.columns("part")?,
                &["id", "message_id", "time_created", "data"],
            )
            && self
                .index_columns("part")?
                .iter()
                .any(|fields| fields.first().is_some_and(|field| field == "message_id"));
        let has_session = names.contains("session")
            && has_all(&self.columns("session")?, &["id", "time_updated"]);
        let mut tables = Vec::new();
        for name in ["session_message", "message"] {
            if !names.contains(name) {
                continue;
            }
            let columns = self.columns(name)?;
            if !has_all(
                &columns,
                &["id", "session_id", "time_created", "time_updated", "data"],
            ) || (name == "session_message" && !columns.contains("type"))
            {
                continue;
            }
            let mut table = Table::new(
                name,
                self.connection
                    .scalar(&format!("SELECT COALESCE(MAX(rowid),0) FROM \"{name}\""))?,
                self.cutoff,
            );
            for fields in self.index_columns(name)? {
                if fields.first().is_some_and(|field| field == "time_created") {
                    table.indexed_history = true;
                }
                if fields.len() > 1 && fields[0] == "session_id" && fields[1] == "time_created" {
                    table.session_history = has_session;
                }
            }
            tables.push(table);
        }
        self.tables = tables;
        Ok(())
    }

    fn poll(&mut self) -> Result<Vec<Telemetry>, ReadError> {
        self.bytes_remaining = 16 * 1024 * 1024;
        let schema = self.connection.scalar("PRAGMA schema_version")?;
        if schema != self.schema_version {
            self.discover()?;
            self.schema_version = schema;
            self.version = -1;
        }
        let current_version = self.connection.scalar("PRAGMA data_version")?;
        let changed = current_version != self.version;
        let mut more_insertions = false;
        let mut result = Vec::new();
        let mut tables = std::mem::take(&mut self.tables);
        let outcome = tables.iter_mut().try_for_each(|table| {
            self.poll_table(table, changed, &mut more_insertions, &mut result)
        });
        self.tables = tables;
        outcome?;
        // A full batch of new rows means more are waiting: look again even if nothing else changes.
        self.version = if more_insertions { -1 } else { current_version };
        Ok(result)
    }

    fn poll_table(
        &mut self,
        table: &mut Table,
        changed: bool,
        more_insertions: &mut bool,
        result: &mut Vec<Telemetry>,
    ) -> Result<(), ReadError> {
        if changed {
            for id in table.pending.keys().cloned().collect::<Vec<_>>() {
                table.enqueue(id);
            }
        }
        if !table.probe_done {
            for id in self.strings(
                &format!(
                    "SELECT id FROM \"{}\" ORDER BY rowid DESC LIMIT 8",
                    table.name
                ),
                0,
                None,
            )? {
                table.enqueue(id);
            }
            table.probe_done = true;
        }
        if changed {
            let mut inserts = self.connection.prepare(&format!("SELECT rowid,id,time_created FROM \"{}\" WHERE rowid>?1 ORDER BY rowid LIMIT {BATCH}", table.name))?;
            inserts.bind_int(1, table.high_row)?;
            let mut count = 0;
            while inserts.step()? {
                count += 1;
                table.high_row = inserts.int(0);
                if inserts.int(2) >= self.cutoff {
                    table.enqueue(inserts.text(1).unwrap_or_default());
                }
            }
            *more_insertions |= count == BATCH;
        }
        if !table.history_done && table.work.len() < 512 {
            self.history(table)?;
        }
        for _ in 0..BATCH {
            let Some(id) = table.work.front().cloned() else {
                break;
            };
            // None: this poll's byte budget is spent; the id stays at the head of the queue.
            let Some(telemetry) = self.read(table, &id)? else {
                break;
            };
            table.work.pop_front();
            table.queued.remove(&id);
            let Some(telemetry) = telemetry else {
                table.pending.shift_remove(&id);
                continue;
            };
            self.has_telemetry = true;
            if telemetry.record.is_none() && telemetry.updated.unix_ms() >= self.cutoff {
                table.pending.insert(id, telemetry.updated);
            } else {
                table.pending.shift_remove(&id);
            }
            result.push(telemetry);
        }
        let cutoff = self.cutoff;
        table
            .pending
            .retain(|_, updated| updated.unix_ms() >= cutoff);
        trim(&mut table.pending, MAXIMUM_PENDING, |updated| *updated);
        Ok(())
    }

    // Queues the next batch of messages from the last seven days, by the cheapest route the schema has:
    // an index on time, an index on session and time, or a backwards walk over row ids.
    fn history(&mut self, table: &mut Table) -> Result<(), SqliteError> {
        let by_session = !table.indexed_history && table.session_history;
        if by_session && table.sessions.is_empty() && !table.sessions_done {
            let mut sessions = self.connection.prepare(
                "SELECT rowid,id,time_updated FROM session WHERE rowid>?1 ORDER BY rowid LIMIT 32",
            )?;
            sessions.bind_int(1, table.session_row)?;
            let mut count = 0;
            while sessions.step()? {
                count += 1;
                table.session_row = sessions.int(0);
                if sessions.int(2) >= self.cutoff {
                    table
                        .sessions
                        .push_back(sessions.text(1).unwrap_or_default());
                }
            }
            table.sessions_done = count < 32;
        }
        if by_session && table.sessions.is_empty() {
            table.history_done = table.sessions_done;
            return Ok(());
        }
        if table.indexed_history || table.session_history {
            let mut command = self.connection.prepare(&format!(
                "SELECT id,time_created FROM \"{}\" WHERE {}time_created >= ?1 AND (time_created>?2 OR (time_created=?2 AND id>?3)) ORDER BY time_created,id LIMIT {BATCH}",
                table.name,
                if by_session { "session_id=?4 AND " } else { "" }
            ))?;
            command.bind_int(1, self.cutoff)?;
            command.bind_int(2, table.history_time)?;
            command.bind_text(3, &table.history_id)?;
            if by_session {
                command.bind_text(4, table.sessions.front().map(String::as_str).unwrap_or(""))?;
            }
            let mut count = 0;
            while command.step()? {
                count += 1;
                table.history_id = command.text(0).unwrap_or_default();
                table.history_time = command.int(1);
                table.enqueue(table.history_id.clone());
            }
            if count < BATCH {
                if table.indexed_history {
                    table.history_done = true;
                } else {
                    table.sessions.pop_front();
                    table.history_time = self.cutoff;
                    table.history_id.clear();
                    table.history_done = table.sessions_done && table.sessions.is_empty();
                }
            }
        } else {
            let mut command = self.connection.prepare(&format!("SELECT rowid,id,time_created FROM \"{}\" WHERE rowid<?1 ORDER BY rowid DESC LIMIT {BATCH}", table.name))?;
            command.bind_int(1, table.fallback_row)?;
            let mut count = 0;
            while command.step()? {
                count += 1;
                table.fallback_row = command.int(0);
                if command.int(2) >= self.cutoff {
                    table.enqueue(command.text(1).unwrap_or_default());
                }
            }
            table.history_done = count < BATCH;
        }
        Ok(())
    }

    // Outer None: out of byte budget, try again next poll. Inner None: not an assistant message worth keeping.
    fn read(&mut self, table: &Table, id: &str) -> Result<Option<Option<Telemetry>>, ReadError> {
        let name = table.name;
        let bytes = {
            let mut size = self.connection.prepare(&format!(
                "SELECT length(CAST(data AS BLOB)) FROM \"{name}\" WHERE id=?1"
            ))?;
            size.bind_text(1, id)?;
            if !size.step()? || size.is_null(0) {
                return Ok(Some(None));
            }
            size.int(0)
        };
        if bytes > MAX_JSON_BYTES {
            return Ok(Some(None));
        }
        // A legacy message's parts are read after it, so room for them is reserved up front.
        if bytes + if table.current() { 0 } else { MAX_JSON_BYTES } > self.bytes_remaining {
            return Ok(None);
        }
        self.bytes_remaining -= bytes;
        // SQLite extracts the few numeric and id fields; message text never leaves the database.
        let (session, updated, json) = {
            let mut command = self.connection.prepare(&format!(
                "SELECT session_id,time_updated,json_object('time',json_extract(data,'$.time'),'model',json_extract(data,'$.model'),'modelID',json_extract(data,'$.modelID'),'providerID',json_extract(data,'$.providerID'),'tokens',json_extract(data,'$.tokens'),'finish',json_extract(data,'$.finish'),'error',CASE WHEN json_type(data,'$.error') NOT IN ('null') THEN 1 ELSE NULL END,'summary',json_extract(data,'$.summary')) FROM \"{name}\" WHERE id=?1 AND json_valid(data)=1 AND {}",
                if table.current() { "type='assistant'" } else { "json_extract(data,'$.role')='assistant'" }
            ))?;
            command.bind_text(1, id)?;
            if !command.step()? {
                return Ok(Some(None));
            }
            (
                command.text(0).unwrap_or_default(),
                milliseconds(&command, 1),
                command.text(2).unwrap_or_default(),
            )
        };
        let data: Value = serde_json::from_str(&json)?;
        let mut parts = Vec::new();
        let mut complete = true;
        if table.current() {
            let mut content = self.connection.prepare(&format!(
                "SELECT json_object('type',json_extract(value,'$.type'),'time',json_extract(value,'$.time'),'state',json_object('status',json_extract(value,'$.state.status'))) FROM \"{name}\",json_each(data,'$.content') WHERE \"{name}\".id=?1 LIMIT 129"
            ))?;
            content.bind_text(1, id)?;
            while content.step()? {
                if parts.len() == 128 {
                    complete = false;
                    break;
                }
                parts.push(Part::read(
                    &serde_json::from_str(&content.text(0).unwrap_or_default())?,
                    None,
                ));
            }
        } else if self.has_part {
            let mut headers: Vec<(String, i64)> = Vec::new();
            {
                let mut command = self.connection.prepare("SELECT id,length(CAST(data AS BLOB)) FROM part WHERE message_id=?1 ORDER BY id LIMIT 129")?;
                command.bind_text(1, id)?;
                while command.step()? {
                    headers.push((command.text(0).unwrap_or_default(), command.int(1)));
                }
            }
            if headers.len() > 128 {
                complete = false;
            }
            let mut part_bytes = 0;
            for (part_id, size) in headers.iter().take(128) {
                if *size > MAX_JSON_BYTES || part_bytes + size > MAX_JSON_BYTES {
                    complete = false;
                    continue;
                }
                if *size > self.bytes_remaining {
                    return Ok(None);
                }
                self.bytes_remaining -= size;
                part_bytes += size;
                let mut command = self.connection.prepare(
                    "SELECT time_created,json_object('type',json_extract(data,'$.type'),'time',json_extract(data,'$.time'),'synthetic',json_extract(data,'$.synthetic'),'ignored',json_extract(data,'$.ignored'),'state',json_object('status',json_extract(data,'$.state.status'))) FROM part WHERE id=?1 AND json_valid(data)=1",
                )?;
                command.bind_text(1, part_id)?;
                if !command.step()? {
                    complete = false;
                    continue;
                }
                parts.push(Part::read(
                    &serde_json::from_str(&command.text(1).unwrap_or_default())?,
                    milliseconds(&command, 0),
                ));
            }
        } else {
            complete = false;
        }
        Ok(Some(Telemetry::read(
            id,
            &session,
            &data,
            &parts,
            table.current(),
            updated,
            complete,
        )))
    }
}
