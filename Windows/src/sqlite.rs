//! Just enough SQLite to read other programs' databases, using the copy the operating system ships
//! (winsqlite3.dll on Windows). Nothing is bundled and nothing is ever written.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fmt;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct SqliteError(pub String);

impl fmt::Display for SqliteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SqliteError {}

type Db = *mut c_void;
type Stmt = *mut c_void;

struct Api {
    open_v2: unsafe extern "system" fn(*const c_char, *mut Db, c_int, *const c_char) -> c_int,
    close: unsafe extern "system" fn(Db) -> c_int,
    busy_timeout: unsafe extern "system" fn(Db, c_int) -> c_int,
    errmsg: unsafe extern "system" fn(Db) -> *const c_char,
    prepare_v2:
        unsafe extern "system" fn(Db, *const c_char, c_int, *mut Stmt, *mut *const c_char) -> c_int,
    finalize: unsafe extern "system" fn(Stmt) -> c_int,
    step: unsafe extern "system" fn(Stmt) -> c_int,
    bind_int64: unsafe extern "system" fn(Stmt, c_int, i64) -> c_int,
    bind_text: unsafe extern "system" fn(Stmt, c_int, *const c_char, c_int, isize) -> c_int,
    column_type: unsafe extern "system" fn(Stmt, c_int) -> c_int,
    column_int64: unsafe extern "system" fn(Stmt, c_int) -> i64,
    column_text: unsafe extern "system" fn(Stmt, c_int) -> *const u8,
    column_blob: unsafe extern "system" fn(Stmt, c_int) -> *const u8,
    column_bytes: unsafe extern "system" fn(Stmt, c_int) -> c_int,
}

#[cfg(windows)]
mod library {
    use std::ffi::{c_char, c_void};
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    pub const NAMES: &[&[u8]] = &[b"winsqlite3.dll\0"];
    pub unsafe fn open(name: *const c_char) -> *mut c_void {
        LoadLibraryA(name)
    }
    pub unsafe fn symbol(module: *mut c_void, name: *const c_char) -> *mut c_void {
        GetProcAddress(module, name)
    }
}

#[cfg(not(windows))]
mod library {
    use std::ffi::{c_char, c_int, c_void};
    extern "C" {
        fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    pub const NAMES: &[&[u8]] = &[
        b"libsqlite3.dylib\0",
        b"libsqlite3.so.0\0",
        b"libsqlite3.so\0",
    ];
    pub unsafe fn open(name: *const c_char) -> *mut c_void {
        dlopen(name, 2)
    }
    pub unsafe fn symbol(module: *mut c_void, name: *const c_char) -> *mut c_void {
        dlsym(module, name)
    }
}

// The target type of each transmute is the struct field it initialises; spelling fourteen signatures twice would only invite a mismatch.
#[allow(clippy::missing_transmute_annotations)]
fn api() -> Result<&'static Api, SqliteError> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let module = library::NAMES.iter().map(|name| library::open(name.as_ptr().cast())).find(|module| !module.is_null())?;
        // Each field's function type is taken from the struct, so a symbol can only be used with its own signature.
        macro_rules! load {
            ($($field:ident),*) => {
                Api { $($field: {
                    let address = library::symbol(module, concat!("sqlite3_", stringify!($field), "\0").as_ptr().cast());
                    if address.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, _>(address)
                }),* }
            };
        }
        Some(load!(open_v2, close, busy_timeout, errmsg, prepare_v2, finalize, step, bind_int64, bind_text, column_type, column_int64, column_text, column_blob, column_bytes))
    })
    .as_ref()
    .ok_or_else(|| SqliteError("The system SQLite library is unavailable.".into()))
}

const OK: c_int = 0;
const ROW: c_int = 100;
const DONE: c_int = 101;
const NULL: c_int = 5;
const OPEN_READONLY: c_int = 0x1;
const OPEN_URI: c_int = 0x40;
// Tells SQLite to copy bound text before the call returns.
const TRANSIENT: isize = -1;

pub struct Connection {
    api: &'static Api,
    db: Db,
}

// The handle is used by one thread at a time; the library itself is built thread-safe.
unsafe impl Send for Connection {}

impl Connection {
    /// Opens a database for reading only. A `file:` name is a SQLite URI and may carry parameters.
    pub fn open_readonly(name: &str) -> Result<Connection, SqliteError> {
        let api = api()?;
        let text =
            CString::new(name).map_err(|_| SqliteError("Database path contains a NUL.".into()))?;
        let mut db: Db = std::ptr::null_mut();
        let flags = OPEN_READONLY
            | if name.starts_with("file:") {
                OPEN_URI
            } else {
                0
            };
        let code = unsafe { (api.open_v2)(text.as_ptr(), &mut db, flags, std::ptr::null()) };
        // A failed open still hands back a handle that owns the error message and must be closed.
        let connection = Connection { api, db };
        if code != OK {
            return Err(connection.error(code));
        }
        // Another program may hold the file briefly; wait for it rather than fail at once.
        unsafe { (api.busy_timeout)(db, 1000) };
        Ok(connection)
    }

    fn error(&self, code: c_int) -> SqliteError {
        let message = if self.db.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr((self.api.errmsg)(self.db)) }
                .to_string_lossy()
                .into_owned()
        };
        SqliteError(format!("SQLite error {code}: {message}"))
    }

    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>, SqliteError> {
        let text = CString::new(sql).map_err(|_| SqliteError("SQL contains a NUL.".into()))?;
        let mut statement: Stmt = std::ptr::null_mut();
        let code = unsafe {
            (self.api.prepare_v2)(
                self.db,
                text.as_ptr(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        };
        if code != OK || statement.is_null() {
            return Err(self.error(code));
        }
        Ok(Statement {
            connection: self,
            statement,
        })
    }

    /// The first column of the first row as an integer; zero when there is no row.
    pub fn scalar(&self, sql: &str) -> Result<i64, SqliteError> {
        let mut statement = self.prepare(sql)?;
        Ok(if statement.step()? {
            statement.int(0)
        } else {
            0
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe { (self.api.close)(self.db) };
        }
    }
}

pub struct Statement<'a> {
    connection: &'a Connection,
    statement: Stmt,
}

impl Statement<'_> {
    fn check(&self, code: c_int) -> Result<(), SqliteError> {
        if code == OK {
            Ok(())
        } else {
            Err(self.connection.error(code))
        }
    }
    /// Parameters are numbered from one, matching `?1` in the SQL.
    pub fn bind_int(&mut self, index: i32, value: i64) -> Result<(), SqliteError> {
        self.check(unsafe { (self.connection.api.bind_int64)(self.statement, index, value) })
    }
    pub fn bind_text(&mut self, index: i32, value: &str) -> Result<(), SqliteError> {
        self.check(unsafe {
            (self.connection.api.bind_text)(
                self.statement,
                index,
                value.as_ptr().cast(),
                value.len() as c_int,
                TRANSIENT,
            )
        })
    }
    /// Advances to the next row. False once the rows are exhausted.
    pub fn step(&mut self) -> Result<bool, SqliteError> {
        match unsafe { (self.connection.api.step)(self.statement) } {
            ROW => Ok(true),
            DONE => Ok(false),
            code => Err(self.connection.error(code)),
        }
    }
    pub fn is_null(&self, column: i32) -> bool {
        unsafe { (self.connection.api.column_type)(self.statement, column) == NULL }
    }
    pub fn int(&self, column: i32) -> i64 {
        unsafe { (self.connection.api.column_int64)(self.statement, column) }
    }
    fn bytes(
        &self,
        column: i32,
        read: unsafe extern "system" fn(Stmt, c_int) -> *const u8,
    ) -> Option<Vec<u8>> {
        if self.is_null(column) {
            return None;
        }
        unsafe {
            // The pointer must be fetched before the length: fetching may convert the value.
            let pointer = read(self.statement, column);
            let length = (self.connection.api.column_bytes)(self.statement, column) as usize;
            Some(if pointer.is_null() || length == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(pointer, length).to_vec()
            })
        }
    }
    /// The column as text, whatever its stored type; None for NULL.
    pub fn text(&self, column: i32) -> Option<String> {
        self.bytes(column, self.connection.api.column_text)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
    pub fn blob(&self, column: i32) -> Option<Vec<u8>> {
        self.bytes(column, self.connection.api.column_blob)
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        unsafe { (self.connection.api.finalize)(self.statement) };
    }
}
