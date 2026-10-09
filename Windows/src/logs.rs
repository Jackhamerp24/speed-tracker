use crate::discovery::{self, LogSource, Pattern};
use crate::domain::LiveCall;
use crate::history::BoundedLines;
use crate::parsers::{DeepSeekLogParser, LogRecord, SessionLogParser};
use crate::time::Time;
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const WEEK: f64 = 7.0 * 86_400.0;
const TEN_MINUTES: f64 = 600.0;

/// Names an I/O failure the way the status line reports it.
pub(crate) fn error_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::InvalidData => "InvalidDataException",
        io::ErrorKind::PermissionDenied => "UnauthorizedAccessException",
        _ => "IOException",
    }
}

fn corrupt(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Decodes a Zstandard file that may still be growing: plain or concatenated frames.
///
/// A block is decoded only once all of it has reached the disk, so a half-written tail is left for
/// the next poll instead of failing. Inside an unfinished frame the decoder holds back its window, so
/// text there surfaces when later blocks arrive or the frame ends.
struct ZstdTail {
    file: File,
    // Bytes read from the file so far; what a change probe compares against.
    position: u64,
    compressed: Vec<u8>,
    output: VecDeque<u8>,
    decoder: FrameDecoder,
    in_frame: bool,
    checksum: bool,
}

impl ZstdTail {
    fn new(file: File) -> ZstdTail {
        ZstdTail {
            file,
            position: 0,
            compressed: Vec::new(),
            output: VecDeque::new(),
            decoder: FrameDecoder::new(),
            in_frame: false,
            checksum: false,
        }
    }

    // Reads more of the file. False at the current end of the file.
    fn fill(&mut self) -> io::Result<bool> {
        let start = self.compressed.len();
        self.compressed.resize(start + 256 * 1024, 0);
        let count = self.file.read(&mut self.compressed[start..]);
        self.compressed
            .truncate(start + *count.as_ref().unwrap_or(&0));
        let count = count?;
        self.position += count as u64;
        Ok(count > 0)
    }

    // How many bytes the next frame header or block occupies, once enough is buffered to tell.
    fn next_unit(&self) -> io::Result<Option<usize>> {
        let bytes = &self.compressed;
        if self.in_frame {
            if bytes.len() < 3 {
                return Ok(None);
            }
            let header = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
            let size = (header >> 3) as usize;
            let content = match (header >> 1) & 3 {
                0 | 2 => size,
                // A run-length block stores one byte, however long it expands.
                1 => 1,
                _ => return Err(corrupt("reserved Zstandard block type")),
            };
            let last = header & 1 == 1;
            return Ok(Some(
                3 + content + if last && self.checksum { 4 } else { 0 },
            ));
        }
        if bytes.len() < 5 {
            return Ok(None);
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if (0x184D_2A50..=0x184D_2A5F).contains(&magic) {
            if bytes.len() < 8 {
                return Ok(None);
            }
            return Ok(Some(
                8 + u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize,
            ));
        }
        let descriptor = bytes[4];
        let single_segment = descriptor & 0x20 != 0;
        let dictionary = [0, 1, 2, 4][(descriptor & 3) as usize];
        let content_size = match descriptor >> 6 {
            0 => usize::from(single_segment),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        Ok(Some(
            5 + usize::from(!single_segment) + dictionary + content_size,
        ))
    }

    // Decodes the next header or block. False when the file has no complete unit yet.
    fn advance(&mut self) -> io::Result<bool> {
        let needed = loop {
            match self.next_unit()? {
                Some(needed) if self.compressed.len() >= needed => break needed,
                _ => {
                    if !self.fill()? {
                        return Ok(false);
                    }
                }
            }
        };
        let unit = &self.compressed[..needed];
        if self.in_frame {
            let finished = self
                .decoder
                .decode_blocks(unit, BlockDecodingStrategy::UptoBlocks(1))
                .map_err(corrupt)?;
            if let Some(decoded) = self.decoder.collect() {
                self.output.extend(decoded);
            }
            self.in_frame = !finished;
        } else if (0x184D_2A50..=0x184D_2A5F)
            .contains(&u32::from_le_bytes([unit[0], unit[1], unit[2], unit[3]]))
        {
            // A skippable frame carries no session data.
        } else {
            self.checksum = unit[4] & 0x04 != 0;
            self.decoder.reset(unit).map_err(corrupt)?;
            self.in_frame = true;
        }
        self.compressed.drain(..needed);
        Ok(true)
    }
}

impl Read for ZstdTail {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.output.is_empty() {
            if !self.advance()? {
                return Ok(0);
            }
        }
        let count = buffer.len().min(self.output.len());
        for (slot, byte) in buffer.iter_mut().zip(self.output.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }
}

enum Source {
    Plain { file: File, position: u64 },
    Zstd(Box<ZstdTail>),
}

impl Source {
    fn position(&self) -> u64 {
        match self {
            Source::Plain { position, .. } => *position,
            Source::Zstd(tail) => tail.position,
        }
    }
}

impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Source::Plain { file, position } => {
                let count = file.read(buffer)?;
                *position += count as u64;
                Ok(count)
            }
            Source::Zstd(tail) => tail.read(buffer),
        }
    }
}

// Counts what a poll decoded, so the watcher's byte budget is charged even when a read fails midway.
struct Counted<'a> {
    source: &'a mut Source,
    read: u64,
}

impl Read for Counted<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.source.read(buffer)?;
        self.read += count as u64;
        Ok(count)
    }
}

// `session.jsonl`, `session.v3.jsonl` or either with `.zstd`: Some(None) for the unversioned name,
// Some(Some(digits)) for a versioned one, None for any other file.
fn generation_name(name: &str) -> Option<Option<&str>> {
    let rest = name.strip_prefix("session")?;
    let rest = rest.strip_suffix(".zstd").unwrap_or(rest);
    let rest = rest.strip_suffix(".jsonl")?;
    if rest.is_empty() {
        return Some(None);
    }
    let digits = rest.strip_prefix(".v")?;
    (!digits.is_empty()
        && !digits.starts_with('0')
        && digits.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(Some(digits))
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}

struct Tail {
    path: PathBuf,
    key: String,
    parser: SessionLogParser,
    last_growth: Time,
    ready: bool,
    failed: bool,
    source: Option<Source>,
    lines: BoundedLines,
    previous_size: u64,
    previous_write: Option<SystemTime>,
    creation: Option<SystemTime>,
    // A seeded DeepSeek log is read once to find where inherited events end, then again for telemetry.
    preflight: bool,
    expected_version: Option<i32>,
    // The last bytes read, kept to notice a file rewritten in place to the same length.
    probe: [u8; 64],
    probe_count: usize,
    probe_offset: u64,
}

impl Tail {
    fn new(path: PathBuf, harness: &str, now: Time) -> Tail {
        let deepseek = harness == "DeepSeek CLI";
        let expected_version = if deepseek {
            generation_name(file_name(&path))
                .and_then(|digits| digits.map_or(Some(0), |digits| digits.parse().ok()))
        } else {
            None
        };
        let parser = if deepseek {
            SessionLogParser::DeepSeek(DeepSeekLogParser::new(None, expected_version))
        } else {
            SessionLogParser::for_harness(harness)
        };
        Tail {
            key: path.to_string_lossy().into_owned(),
            path,
            parser,
            last_growth: now,
            ready: !deepseek,
            failed: false,
            source: None,
            lines: BoundedLines::new(),
            previous_size: 0,
            previous_write: None,
            creation: None,
            preflight: deepseek,
            expected_version,
            probe: [0; 64],
            probe_count: 0,
            probe_offset: 0,
        }
    }

    fn open(&mut self) -> io::Result<()> {
        let file = File::open(&self.path)?;
        self.source = Some(if file_name(&self.path).ends_with(".zstd") {
            Source::Zstd(Box::new(ZstdTail::new(file)))
        } else {
            Source::Plain { file, position: 0 }
        });
        Ok(())
    }

    fn fresh_parser(&self, inherited: Option<i64>) -> SessionLogParser {
        match self.parser {
            SessionLogParser::DeepSeek(_) => {
                SessionLogParser::DeepSeek(DeepSeekLogParser::new(inherited, self.expected_version))
            }
            _ => SessionLogParser::for_harness(self.parser.harness()),
        }
    }

    fn reset(&mut self) {
        self.source = None;
        self.lines = BoundedLines::new();
        self.parser = self.fresh_parser(None);
        self.preflight = self.parser.as_deepseek().is_some();
        self.ready = !self.preflight;
        self.failed = false;
        self.probe_count = 0;
    }

    fn mark_failed(&mut self, corrupt: bool) {
        self.failed = true;
        self.ready = false;
        if !corrupt {
            self.source = None;
            return;
        }
        // Keep the probe, so a corrupt file is re-read only once it has been rewritten.
        let _ = self.capture_probe();
    }

    fn read_probe(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut current = File::open(&self.path)?;
        current.seek(SeekFrom::Start(self.probe_offset))?;
        let mut count = 0;
        while count < buffer.len() {
            match current.read(&mut buffer[count..])? {
                0 => break,
                read => count += read,
            }
        }
        Ok(count)
    }

    fn capture_probe(&mut self) -> io::Result<()> {
        let Some(position) = self
            .source
            .as_ref()
            .map(Source::position)
            .filter(|position| *position > 0)
        else {
            return Ok(());
        };
        self.probe_count = position.min(self.probe.len() as u64) as usize;
        self.probe_offset = position - self.probe_count as u64;
        let mut bytes = [0u8; 64];
        self.probe_count = self.read_probe(&mut bytes[..self.probe_count])?;
        self.probe = bytes;
        Ok(())
    }

    fn poll(&mut self, now: Time, budget: &mut u64, result: &mut Vec<LogRecord>) -> io::Result<()> {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            self.mark_failed(false);
            return Ok(());
        };
        let (length, modified, created) = (
            metadata.len(),
            metadata.modified().ok(),
            metadata.created().ok(),
        );
        let mut changed = length < self.previous_size
            || (length == self.previous_size && modified != self.previous_write)
            || (self.creation.is_some() && self.creation != created);
        if !changed && self.probe_count > 0 && length >= self.probe_offset + self.probe_count as u64
        {
            let mut bytes = [0u8; 64];
            let count = self.read_probe(&mut bytes[..self.probe_count])?;
            changed = count != self.probe_count || bytes[..count] != self.probe[..count];
        }
        if changed || (self.failed && self.source.is_none()) {
            self.reset();
        }
        self.creation = created;
        if self.failed {
            return Ok(());
        }
        if self.source.is_none() {
            self.open()?;
        }
        let grew = length != self.previous_size || modified != self.previous_write;
        self.previous_size = length;
        self.previous_write = modified;

        let (parser, preflight, ready) = (&mut self.parser, &mut self.preflight, &mut self.ready);
        let mut counted = Counted {
            source: self.source.as_mut().expect("opened above"),
            read: 0,
        };
        let drained = self.lines.drain(
            &mut counted,
            &mut |line| {
                // An oversize or malformed line is skipped; the session carries on after it.
                let Some(entry) =
                    line.and_then(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                else {
                    return;
                };
                result.extend(parser.ingest(&entry));
                if let Some(deepseek) = parser.as_deepseek() {
                    *preflight = deepseek.needs_inherited_scan();
                    *ready = deepseek.has_supported_header() && !*preflight;
                }
            },
            (*budget).min(4 * 1024 * 1024),
        );
        *budget = budget.saturating_sub(counted.read);
        let reached_end = drained?;

        if self.preflight && reached_end {
            if let Some(cut) = self
                .parser
                .as_deepseek()
                .and_then(DeepSeekLogParser::inherited_cut)
            {
                // Find the last tagged inherited boundary before admitting any ancestor telemetry.
                self.source = None;
                self.lines = BoundedLines::new();
                self.parser = self.fresh_parser(Some(cut));
                self.preflight = false;
                self.ready = false;
                self.probe_count = 0;
                self.open()?;
            }
        }
        self.capture_probe()?;
        if grew {
            if let Some(modified) = modified {
                self.last_growth = Time::from_system(modified);
            }
        }
        if self.ready && reached_end && !grew {
            result.extend(self.parser.flush(now.since(self.last_growth)));
        }
        Ok(())
    }
}

/// Finds and tails the JSONL session files of every log-covered harness except opencode and
/// Antigravity, which keep databases and have their own watchers.
pub struct SessionLogWatcher {
    sources: Vec<LogSource>,
    tails: BTreeMap<String, Tail>,
    errors: BTreeMap<String, String>,
    discovery_errors: BTreeMap<String, String>,
    last_scan: Time,
    // Polling resumes where the last pass stopped, so a byte budget cannot starve later files.
    poll_cursor: usize,
    tail_order: Vec<String>,
    recent_keys: HashSet<String>,
    key_order: VecDeque<String>,
}

impl SessionLogWatcher {
    pub fn new(sources: &[LogSource]) -> SessionLogWatcher {
        SessionLogWatcher {
            sources: sources
                .iter()
                .filter(|source| !matches!(source.harness.as_str(), "opencode" | "Antigravity"))
                .cloned()
                .collect(),
            tails: BTreeMap::new(),
            errors: BTreeMap::new(),
            discovery_errors: BTreeMap::new(),
            last_scan: Time::MIN,
            poll_cursor: 0,
            tail_order: Vec::new(),
            recent_keys: HashSet::new(),
            key_order: VecDeque::new(),
        }
    }

    pub fn errors(&self) -> &BTreeMap<String, String> {
        &self.errors
    }

    /// Whether this harness keeps session logs here in a format that was read successfully.
    pub fn has_logs(&self, harness: &str) -> bool {
        self.tails.values().any(|tail| {
            tail.ready
                && !tail.failed
                && tail.parser.harness() == harness
                && tail.parser.state().supported
        })
    }

    /// Sessions whose log says a response is due.
    pub fn active(&self, now: Time) -> Vec<LiveCall> {
        self.tails
            .values()
            .filter(|tail| tail.ready && !tail.failed && tail.parser.state().awaiting_response)
            .filter_map(|tail| {
                let state = tail.parser.state();
                let start = state.last_request_at.filter(|start| {
                    *start <= now
                        && now.since(*start) < TEN_MINUTES
                        && now.since(tail.last_growth) < TEN_MINUTES
                })?;
                let phase = if state.first_token_at.is_none() {
                    "Waiting"
                } else if state.first_visible_at.is_none() {
                    "Thinking"
                } else {
                    "Streaming"
                };
                Some(LiveCall {
                    id: tail.key.clone(),
                    harness: tail.parser.harness().to_string(),
                    model: state
                        .current_model
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    provider: state.current_provider.clone(),
                    phase: phase.into(),
                    started_at: start,
                    last_activity: tail.last_growth,
                    ttft: state.first_token_at.map(|first| first.since(start)),
                    rate: None,
                    output_tokens: None,
                    estimated: false,
                })
            })
            .collect()
    }

    pub fn poll(&mut self, now: Time) -> Vec<LogRecord> {
        if now.since(self.last_scan) >= 4.0 {
            self.scan(now);
            self.last_scan = now;
        }
        let mut records = Vec::new();
        self.errors = self.discovery_errors.clone();
        let mut budget: u64 = 16 * 1024 * 1024;
        let count = self.tail_order.len();
        let mut visited = 0;
        while visited < count && budget > 0 {
            let key = &self.tail_order[(self.poll_cursor + visited) % count];
            visited += 1;
            let Some(tail) = self.tails.get_mut(key) else {
                continue;
            };
            if let Err(error) = tail.poll(now, &mut budget, &mut records) {
                tail.mark_failed(error.kind() == io::ErrorKind::InvalidData);
                self.errors.insert(
                    tail.parser.harness().to_string(),
                    format!("Session telemetry unavailable: {}.", error_name(&error)),
                );
            }
        }
        if count > 0 {
            self.poll_cursor = (self.poll_cursor + visited) % count;
        }
        for tail in self.tails.values() {
            let message = if tail.failed {
                Some("Session telemetry unavailable until its source is repaired.".to_string())
            } else {
                tail.parser.state().limitation.clone()
            };
            if let Some(message) = message {
                self.errors
                    .entry(tail.parser.harness().to_string())
                    .or_insert(message);
            }
        }
        // A file read again from the start repeats its records; remember recent keys to drop them.
        records.retain(|record| {
            if !self.recent_keys.insert(record.key.clone()) {
                return false;
            }
            self.key_order.push_back(record.key.clone());
            if self.key_order.len() > 8192 {
                if let Some(oldest) = self.key_order.pop_front() {
                    self.recent_keys.remove(&oldest);
                }
            }
            true
        });
        records
    }

    fn scan(&mut self, now: Time) {
        self.discovery_errors.clear();
        for source in &self.sources {
            let deepseek = source.harness == "DeepSeek CLI";
            let found: Vec<PathBuf> = if deepseek {
                // A session folder holds one file per format generation; only the newest is current.
                let mut folders: BTreeMap<PathBuf, Vec<(PathBuf, i32)>> = BTreeMap::new();
                for path in discovery::files(&source.root, Pattern::DeepSeekSession) {
                    let Some(digits) = generation_name(file_name(&path)) else {
                        continue;
                    };
                    let version = digits.map_or(0, |digits| digits.parse().unwrap_or(i32::MAX));
                    folders
                        .entry(path.parent().map(Path::to_path_buf).unwrap_or_default())
                        .or_default()
                        .push((path, version));
                }
                let mut selected = Vec::new();
                for generations in folders.into_values() {
                    let highest = generations
                        .iter()
                        .map(|(_, version)| *version)
                        .max()
                        .unwrap_or(0);
                    let mut candidates = generations
                        .into_iter()
                        .filter(|(_, version)| *version == highest);
                    let (Some((path, _)), None) = (candidates.next(), candidates.next()) else {
                        self.discovery_errors.insert(
                            source.harness.clone(),
                            "Ambiguous DeepSeek raw/compressed generation; no telemetry guessed."
                                .into(),
                        );
                        continue;
                    };
                    if highest > 4 {
                        self.discovery_errors.insert(
                            source.harness.clone(),
                            format!("Unsupported DeepSeek session generation v{highest}."),
                        );
                        continue;
                    }
                    selected.push(path);
                }
                let retained: HashSet<&PathBuf> = selected.iter().collect();
                self.tails.retain(|_, tail| {
                    !(tail.parser.harness() == source.harness
                        && tail.path.starts_with(&source.root)
                        && !retained.contains(&tail.path))
                });
                selected
            } else {
                discovery::files(&source.root, Pattern::Jsonl).collect()
            };
            for path in found {
                let key = path.to_string_lossy().into_owned();
                if self.tails.contains_key(&key) {
                    continue;
                }
                let Ok(modified) =
                    std::fs::metadata(&path).and_then(|metadata| metadata.modified())
                else {
                    continue;
                };
                if Time::from_system(modified) < now.add_seconds(-WEEK) {
                    continue;
                }
                self.tails
                    .insert(key, Tail::new(path, &source.harness, now));
            }
        }
        self.tails
            .retain(|_, tail| now.since(tail.last_growth) <= WEEK);
        self.tail_order = self.tails.keys().cloned().collect();
    }
}
