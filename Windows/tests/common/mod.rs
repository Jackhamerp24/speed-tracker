//! Fixtures shared by the integration tests: scratch folders, session-log lines, Zstandard frames,
//! a scripted network sampler, and a writable SQLite connection for building databases.
#![allow(dead_code)]

use serde_json::Value;
use speedtracker::domain::{FlowSample, FlowSampler};
use speedtracker::time::{Time, TICKS_PER_MILLISECOND};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A folder under the system temp directory, removed when dropped.
pub struct Scratch {
    pub root: PathBuf,
}

impl Scratch {
    pub fn new(label: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("speedtracker-test-{label}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).expect("create scratch folder");
        Scratch { root }
    }
    pub fn path(&self, relative: &str) -> PathBuf {
        relative.split('/').fold(self.root.clone(), |joined, part| joined.join(part))
    }
    /// Writes a file, replacing it if present, creating its folders.
    pub fn put(&self, relative: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("file has a folder")).expect("create folders");
        std::fs::write(&path, content).expect("write fixture");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Appends to a file, creating it and its folders.
pub fn append(path: &Path, content: impl AsRef<[u8]>) {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().expect("file has a folder")).expect("create folders");
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).expect("open for append");
    file.write_all(content.as_ref()).expect("append fixture");
}

/// One JSON document per line, each ending in a newline, as session logs are written.
pub fn lines(rows: &[Value]) -> String {
    rows.iter().map(|row| format!("{row}\n")).collect()
}

/// `2026-10-08T12:34:56.789Z`: the instant to the millisecond.
pub fn iso(time: Time) -> String {
    let (year, month, day, hour, minute, second, ticks) = time.civil();
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z", ticks / TICKS_PER_MILLISECOND)
}

/// Epoch milliseconds, as harnesses that log numbers write them.
pub fn ms(time: Time) -> i64 {
    time.unix_ms()
}

/// One complete Zstandard frame holding `text`.
pub fn frame(text: &str) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(text.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest)
}

#[track_caller]
pub fn near(actual: Option<f64>, expected: f64, tolerance: f64, label: &str) {
    assert!(actual.is_some_and(|value| (value - expected).abs() <= tolerance), "{label}: expected ≈{expected}, got {actual:?}");
}

/// A network sampler whose counters the test sets by hand.
#[derive(Default)]
pub struct FakeSampler {
    samples: Mutex<Vec<FlowSample>>,
}

impl FakeSampler {
    fn key(sample: &FlowSample) -> &str {
        sample.connection_id.as_deref().unwrap_or(&sample.host)
    }
    pub fn get(&self, connection: &str) -> Option<FlowSample> {
        self.samples.lock().unwrap().iter().find(|sample| Self::key(sample) == connection).cloned()
    }
    /// Sets a connection's current counters.
    pub fn push(&self, sample: FlowSample) {
        let mut samples = self.samples.lock().unwrap();
        match samples.iter_mut().find(|known| Self::key(known) == Self::key(&sample)) {
            Some(known) => *known = sample,
            None => samples.push(sample),
        }
    }
    pub fn remove(&self, connection: &str) {
        self.samples.lock().unwrap().retain(|sample| Self::key(sample) != connection);
    }
}

impl FlowSampler for FakeSampler {
    fn available(&self) -> bool {
        true
    }
    fn status(&self) -> String {
        "Deterministic test sampler.".into()
    }
    fn sample(&self) -> Vec<FlowSample> {
        self.samples.lock().unwrap().clone()
    }
}

pub fn sample(pid: u32, harness: &str, host: &str, received: u64, sent: u64, connection: &str) -> FlowSample {
    FlowSample { pid, harness: harness.into(), host: host.into(), received, sent, connection_id: Some(connection.into()) }
}

/// A writable SQLite connection, for building the databases other programs would have written.
/// The app itself only ever opens databases read-only, so this lives with the tests.
pub mod sqlite {
    use std::ffi::{c_char, c_int, c_void, CString};
    use std::path::Path;

    #[cfg(windows)]
    mod library {
        use std::ffi::{c_char, c_void};
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const c_char) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        }
        pub unsafe fn symbol(name: &[u8]) -> *mut c_void {
            let module = LoadLibraryA(b"winsqlite3.dll\0".as_ptr().cast());
            assert!(!module.is_null(), "winsqlite3.dll is part of Windows");
            GetProcAddress(module, name.as_ptr().cast())
        }
    }

    #[cfg(not(windows))]
    mod library {
        use std::ffi::{c_char, c_int, c_void};
        extern "C" {
            fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
            fn dlsym(module: *mut c_void, name: *const c_char) -> *mut c_void;
        }
        pub unsafe fn symbol(name: &[u8]) -> *mut c_void {
            let module = [&b"libsqlite3.dylib\0"[..], b"libsqlite3.so.0\0"].iter().map(|library| dlopen(library.as_ptr().cast(), 2)).find(|module| !module.is_null()).expect("system SQLite");
            dlsym(module, name.as_ptr().cast())
        }
    }

    macro_rules! sqlite {
        ($name:literal as $signature:ty) => {{
            let address = library::symbol(concat!($name, "\0").as_bytes());
            assert!(!address.is_null(), concat!($name, " is exported"));
            std::mem::transmute::<*mut c_void, $signature>(address)
        }};
    }

    pub enum Param<'a> {
        Text(&'a str),
        Int(i64),
        Blob(&'a [u8]),
    }

    pub struct Db(*mut c_void);

    impl Db {
        /// Opens the database for writing, creating it if needed.
        pub fn create(path: &Path) -> Db {
            std::fs::create_dir_all(path.parent().expect("database has a folder")).expect("create folders");
            let name = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let mut db = std::ptr::null_mut();
            unsafe {
                let open = sqlite!("sqlite3_open_v2" as unsafe extern "system" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int);
                // READWRITE | CREATE
                assert_eq!(open(name.as_ptr(), &mut db, 0x2 | 0x4, std::ptr::null()), 0, "open {}", path.display());
            }
            Db(db)
        }

        /// Runs a statement with parameters `?1`, `?2`, … bound in order.
        pub fn run(&self, sql: &str, params: &[Param]) {
            unsafe {
                let prepare = sqlite!("sqlite3_prepare_v2" as unsafe extern "system" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int);
                let bind_text = sqlite!("sqlite3_bind_text" as unsafe extern "system" fn(*mut c_void, c_int, *const c_char, c_int, isize) -> c_int);
                let bind_int = sqlite!("sqlite3_bind_int64" as unsafe extern "system" fn(*mut c_void, c_int, i64) -> c_int);
                let bind_blob = sqlite!("sqlite3_bind_blob" as unsafe extern "system" fn(*mut c_void, c_int, *const c_void, c_int, isize) -> c_int);
                let step = sqlite!("sqlite3_step" as unsafe extern "system" fn(*mut c_void) -> c_int);
                let finalize = sqlite!("sqlite3_finalize" as unsafe extern "system" fn(*mut c_void) -> c_int);
                let text = CString::new(sql).unwrap();
                let mut rest = text.as_ptr();
                // A string may hold several statements; parameters belong to the first that takes any.
                while *rest != 0 {
                    let mut statement = std::ptr::null_mut();
                    assert_eq!(prepare(self.0, rest, -1, &mut statement, &mut rest), 0, "prepare: {sql}");
                    if statement.is_null() {
                        continue;
                    }
                    for (index, param) in params.iter().enumerate() {
                        let index = index as c_int + 1;
                        // -1: SQLite copies the value before the call returns.
                        let code = match param {
                            Param::Text(value) => bind_text(statement, index, value.as_ptr().cast(), value.len() as c_int, -1),
                            Param::Int(value) => bind_int(statement, index, *value),
                            Param::Blob(value) => bind_blob(statement, index, value.as_ptr().cast(), value.len() as c_int, -1),
                        };
                        assert_eq!(code, 0, "bind ?{index}: {sql}");
                    }
                    let mut code = step(statement);
                    // 100: a row, as PRAGMA statements return.
                    while code == 100 {
                        code = step(statement);
                    }
                    assert_eq!(code, 101, "run: {sql}");
                    finalize(statement);
                }
            }
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            unsafe {
                let close = sqlite!("sqlite3_close" as unsafe extern "system" fn(*mut c_void) -> c_int);
                close(self.0);
            }
        }
    }
}
