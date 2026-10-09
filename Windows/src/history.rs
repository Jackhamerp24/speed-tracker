use crate::domain::{DashboardFilter, RequestRecord};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

type Listener = Box<dyn Fn(&Arc<RequestRecord>) + Send + Sync>;

/// Append-only history.jsonl: one finished call per line.
pub struct HistoryStore {
    home: PathBuf,
    path: PathBuf,
    state: Mutex<State>,
    listeners: Mutex<Vec<Listener>>,
}

#[derive(Default)]
struct State {
    records: Vec<Arc<RequestRecord>>,
    keys: HashSet<String>,
    // The file as last read; a different size or write time means someone else changed it.
    loaded: Option<(u64, Option<SystemTime>)>,
    skipped_lines: usize,
}

/// Where config and history live: `SPEEDTRACKER_HOME` when set, so test runs stay out of real data.
pub fn default_home() -> PathBuf {
    match std::env::var_os("SPEEDTRACKER_HOME").filter(|value| !value.is_empty()) {
        Some(home) => PathBuf::from(home),
        None => {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("SpeedTracker")
        }
    }
}

impl HistoryStore {
    pub fn new(home: Option<&Path>) -> HistoryStore {
        let home = home.map(Path::to_path_buf).unwrap_or_else(default_home);
        let home = std::path::absolute(&home).unwrap_or(home);
        HistoryStore {
            path: home.join("history.jsonl"),
            home,
            state: Mutex::default(),
            listeners: Mutex::default(),
        }
    }
    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn skipped_lines(&self) -> usize {
        self.state.lock().unwrap().skipped_lines
    }
    /// Calls `listener` after each record is stored, outside the store's lock.
    pub fn on_recorded(&self, listener: impl Fn(&Arc<RequestRecord>) + Send + Sync + 'static) {
        self.listeners.lock().unwrap().push(Box::new(listener));
    }

    pub fn read(&self, filter: Option<&DashboardFilter>) -> io::Result<Vec<Arc<RequestRecord>>> {
        let mut state = self.state.lock().unwrap();
        self.load(&mut state)?;
        Ok(state
            .records
            .iter()
            .filter(|record| filter.is_none_or(|filter| filter.includes(record)))
            .cloned()
            .collect())
    }

    /// Stores a record unless one with the same key is already there. False means it was a duplicate.
    pub fn append(&self, record: RequestRecord) -> io::Result<bool> {
        let record = Arc::new(record);
        {
            let mut state = self.state.lock().unwrap();
            self.load(&mut state)?;
            let key = record.dedup_key();
            if state.keys.contains(&key) {
                return Ok(false);
            }
            let mut line = serde_json::to_vec(record.as_ref()).map_err(io::Error::other)?;
            line.push(b'\n');
            std::fs::create_dir_all(&self.home)?;
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(windows)]
            {
                // Readers may share the file; a second writer may not.
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(1);
            }
            let mut file = options.open(&self.path)?;
            let length = file.metadata()?.len();
            // A valid last object without a newline must not merge with the next append.
            if length > 0 {
                file.seek(SeekFrom::Start(length - 1))?;
                let mut last = [0u8; 1];
                file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    file.write_all(b"\n")?;
                }
            }
            file.seek(SeekFrom::End(0))?;
            file.write_all(&line)?;
            file.sync_all()?;
            let metadata = file.metadata()?;
            drop(file);
            state.loaded = Some((
                metadata.len(),
                std::fs::metadata(&self.path)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
            ));
            state.keys.insert(key);
            state.records.push(Arc::clone(&record));
        }
        for listener in self.listeners.lock().unwrap().iter() {
            listener(&record);
        }
        Ok(true)
    }

    fn load(&self, state: &mut State) -> io::Result<()> {
        if self.path.is_dir() {
            return Err(io::Error::other("History path is a directory."));
        }
        let metadata = std::fs::metadata(&self.path).ok();
        let stamp = (
            metadata.as_ref().map_or(0, |metadata| metadata.len()),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok()),
        );
        if state.loaded == Some(stamp) {
            return Ok(());
        }
        let (mut records, mut seen, mut skipped) = (Vec::new(), HashSet::new(), 0);
        if metadata.is_some() {
            let mut file = std::fs::File::open(&self.path)?;
            let mut lines = BoundedLines::new();
            let mut consume = |line: Option<&[u8]>| {
                let record = line
                    .and_then(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
                    .and_then(|value| RequestRecord::from_json(&value));
                match record {
                    Some(record) => {
                        if seen.insert(record.dedup_key()) {
                            records.push(Arc::new(record));
                        }
                    }
                    None => skipped += 1,
                }
            };
            lines.drain(&mut file, &mut consume, u64::MAX)?;
            lines.finish(&mut consume);
        }
        state.records = records;
        state.keys = seen;
        state.skipped_lines = skipped;
        state.loaded = Some(stamp);
        Ok(())
    }
}

/// Splits a byte stream into lines without ever holding more than one megabyte of a line.
/// Bounds both telemetry tails and history lines; oversize prompt/tool rows are never retained.
pub struct BoundedLines {
    chunk: Box<[u8]>,
    pending: Vec<u8>,
    oversized: bool,
}

impl Default for BoundedLines {
    fn default() -> Self {
        Self::new()
    }
}

impl BoundedLines {
    pub const MAXIMUM: usize = 1024 * 1024;

    pub fn new() -> BoundedLines {
        BoundedLines {
            chunk: vec![0u8; 64 * 1024].into_boxed_slice(),
            pending: Vec::new(),
            oversized: false,
        }
    }

    /// Reads up to `budget` bytes, handing each complete line to `consume` (None for a line that was
    /// too long to keep). True when the stream ended; false when the budget ran out first.
    pub fn drain(
        &mut self,
        stream: &mut dyn Read,
        consume: &mut dyn FnMut(Option<&[u8]>),
        mut budget: u64,
    ) -> io::Result<bool> {
        while budget > 0 {
            let limit = (self.chunk.len() as u64).min(budget) as usize;
            let count = stream.read(&mut self.chunk[..limit])?;
            if count == 0 {
                return Ok(true);
            }
            budget -= count as u64;
            let mut start = 0;
            for index in 0..count {
                if self.chunk[index] != b'\n' {
                    continue;
                }
                self.add(start, index);
                self.consume(consume);
                start = index + 1;
            }
            self.add(start, count);
        }
        Ok(false)
    }

    fn add(&mut self, start: usize, end: usize) {
        if self.oversized {
            return;
        }
        if end - start > Self::MAXIMUM - self.pending.len() {
            self.oversized = true;
            self.pending.clear();
        } else {
            self.pending.extend_from_slice(&self.chunk[start..end]);
        }
    }

    fn consume(&mut self, consume: &mut dyn FnMut(Option<&[u8]>)) {
        if self.oversized {
            consume(None);
        } else if !self.pending.is_empty() {
            consume(Some(&self.pending));
        }
        self.pending.clear();
        self.oversized = false;
    }

    /// Hands over a last line that had no newline after it.
    pub fn finish(&mut self, consume: &mut dyn FnMut(Option<&[u8]>)) {
        if self.oversized || !self.pending.is_empty() {
            self.consume(consume);
        }
    }
}
