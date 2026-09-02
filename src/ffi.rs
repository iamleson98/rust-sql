//! SQLite-style C ABI: `rustqlite_open` / `rustqlite_exec` /
//! `rustqlite_prepare_v2` / `rustqlite_bind_*` / `rustqlite_step` /
//! `rustqlite_column_*` plus the extension entry points.
//!
//! Function names, argument order, status codes (0 = OK, 100 = ROW, 101 =
//! DONE), and lifetimes mirror the sqlite3 C API one-for-one, so C
//! programs (and future sqlx-style drivers) can be ported by renaming
//! prefixes. See `docs/FFI.md` for the full mapping table and
//! `include/rustqlite_ext.h` for the extension ABI.
//!
//! # Threading model
//!
//! The connection handle wraps the engine's `Database` in a `RwLock`:
//! `rustqlite_step` on read statements takes the read lock (N concurrent
//! readers, the engine's 8.3x parallel-read win), write statements and
//! `rustqlite_exec` take the write lock. Prepared statements must be
//! finalized before `rustqlite_close` (returns MISUSE + 1 otherwise,
//! like SQLITE_BUSY from sqlite3_close).

use crate::api::Database;
use crate::error::{Error, Result};
use crate::plugin::abi::*;

// Status / value-type codes for C consumers (SQLite-compatible values).
pub use crate::plugin::abi::{RQL_BLOB, RQL_ERROR, RQL_FLOAT, RQL_INTEGER, RQL_MISUSE, RQL_NOMEM, RQL_NULL, RQL_OK, RQL_TEXT};
use crate::types::Value;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::raw::c_uchar;

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// `rustqlite_step` returned a row (SQLITE_ROW).
pub const RQL_ROW: c_int = 100;
/// `rustqlite_step` finished the statement (SQLITE_DONE).
pub const RQL_DONE: c_int = 101;

/// Connection handle.
pub struct RqlConn {
    db: parking_lot::RwLock<Database>,
    last_error: parking_lot::Mutex<CString>,
    /// Live prepared statements (close refuses while > 0).
    live_statements: std::sync::atomic::AtomicUsize,
}

/// Prepared-statement handle.
pub struct RqlStmt {
    conn: *mut RqlConn,
    #[allow(dead_code)]
    sql: CString,
    /// The engine statement. Lifetime-erased: the borrow is valid because
    /// the owning RqlConn outlives this handle (close checks
    /// live_statements) and the Arc keeps the registry alive.
    stmt: Option<StatementErased>,
    /// Text buffers handed out by column_text (freed on the next step).
    text_bufs: Vec<CString>,
    /// True when the statement writes (INSERT/UPDATE/DELETE): steps take
    /// the connection write lock.
    is_write: bool,
    reset_done: bool,
}

/// `Statement<'static>` (lifetime-erased via transmute — see module docs).
type StatementErased = crate::statement::Statement<'static>;

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

/// Open (or create) a database. `path` is UTF-8; `":memory:"` selects the
/// in-memory engine. Returns RQL_OK / RQL_ERROR.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_open(path: *const c_char, ppdb: *mut *mut RqlConn) -> c_int {
    if path.is_null() || ppdb.is_null() {
        return RQL_MISUSE;
    }
    let p = match CStr::from_ptr(path).to_str() {
        Ok(p) => p,
        Err(_) => return set_thread_error_str("path is not valid UTF-8"),
    };
    let db = if p == ":memory:" {
        Database::open_in_memory()
    } else {
        Database::open(p)
    };
    match db {
        Ok(db) => {
            let conn = Box::new(RqlConn {
                db: parking_lot::RwLock::new(db),
                last_error: parking_lot::Mutex::new(CString::new("").unwrap()),
                live_statements: std::sync::atomic::AtomicUsize::new(0),
            });
            *ppdb = Box::into_raw(conn);
            RQL_OK
        }
        Err(e) => set_thread_error_str(&e.to_string()),
    }
}

/// Open an in-memory database.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_open_in_memory(ppdb: *mut *mut RqlConn) -> c_int {
    rustqlite_open(c":memory:".as_ptr(), ppdb)
}

/// Close the connection. Returns RQL_MISUSE when prepared statements are
/// still alive (SQLite's sqlite3_close returns SQLITE_BUSY for the same
/// reason).
#[no_mangle]
pub unsafe extern "C" fn rustqlite_close(db: *mut RqlConn) -> c_int {
    if db.is_null() {
        return RQL_MISUSE;
    }
    let conn = unsafe { &*db };
    if conn
        .live_statements
        .load(std::sync::atomic::Ordering::Acquire)
        > 0
    {
        return RQL_MISUSE;
    }
    unsafe { drop(Box::from_raw(db)) };
    RQL_OK
}

/// Execute zero-or-more statements (no result rows). Multi-statement SQL
/// runs sequentially until the first error.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_exec(db: *mut RqlConn, sql: *const c_char) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(sql) = cstr(sql) else {
        return RQL_MISUSE;
    };
    // Split on ';' like sqlite3_exec. Simple statements only (no
    // triggers with embedded semicolons — same caveat as the shell).
    let mut rc = RQL_OK;
    for part in sql.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut db = conn.db.write();
        // vtab bridge: DDL (CREATE VIRTUAL TABLE) and module creation need
        // the raw pointer during execution.
        let guard = crate::plugin::abi::ThreadDbGuard::install(&mut *db as *mut Database);
        let res = db.execute(part, []);
        drop(guard);
        drop(db);
        if let Err(e) = res {
            rc = set_conn_error(conn, &e.to_string());
            break;
        }
    }
    rc
}

// ---------------------------------------------------------------------------
// Prepared statements
// ---------------------------------------------------------------------------

/// Prepare one statement. `sql` is NUL-terminated UTF-8 (a negative `len`
/// means strlen); `pzTail` (optional) receives the byte offset of the
/// first character past the statement's end (multi-statement stepping).
#[no_mangle]
pub unsafe extern "C" fn rustqlite_prepare_v2(
    db: *mut RqlConn,
    sql: *const c_char,
    len: c_int,
    ppstmt: *mut *mut RqlStmt,
    pz_tail: *mut c_int,
) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(sql_str) = cstr(sql) else {
        return RQL_MISUSE;
    };
    if ppstmt.is_null() {
        return RQL_MISUSE;
    }
    // Find the statement end: the parser consumes trailing semicolons.
    let text: &str = if len >= 0 {
        &sql_str[..(len as usize).min(sql_str.len())]
    } else {
        sql_str
    };
    // Hold the read guard for the whole match: the returned Statement
    // borrows the Database through it (lifetime-erased below).
    let rd = conn.db.read();
    let prepared = rd.prepare(text);
    match prepared {
        Ok(stmt) => {
            let is_write = is_write_statement(text);
            let owned = CString::new(text).unwrap();
            let tail_off = statement_tail_offset(sql_str, text);
            // Lifetime erasure: the RqlConn outlives the statement
            // (rustqlite_close refuses while statements are live).
            let erased: StatementErased = unsafe { std::mem::transmute(stmt) };
            let handle = Box::new(RqlStmt {
                conn: db,
                sql: owned,
                stmt: Some(erased),
                text_bufs: Vec::new(),
                is_write,
                reset_done: false,
            });
            *ppstmt = Box::into_raw(handle);
            conn.live_statements
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            if !pz_tail.is_null() {
                *pz_tail = tail_off as c_int;
            }
            RQL_OK
        }
        Err(e) => set_conn_error(conn, &e.to_string()),
    }
}

/// Finalize a statement (drop it).
#[no_mangle]
pub unsafe extern "C" fn rustqlite_finalize(stmt: *mut RqlStmt) -> c_int {
    if stmt.is_null() {
        return RQL_MISUSE;
    }
    let handle = unsafe { Box::from_raw(stmt) };
    if let Some(conn) = unsafe { handle.conn.as_ref() } {
        conn.live_statements
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
    RQL_OK
}

/// Reset a statement for re-execution (bindings are kept).
#[no_mangle]
pub unsafe extern "C" fn rustqlite_reset(stmt: *mut RqlStmt) -> c_int {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return RQL_MISUSE;
    };
    handle.text_bufs.clear();
    if let Some(s) = handle.stmt.as_mut() {
        s.reset();
    }
    handle.reset_done = false;
    RQL_OK
}

/// Clear all bindings (parameters become NULL).
#[no_mangle]
pub unsafe extern "C" fn rustqlite_clear_bindings(stmt: *mut RqlStmt) -> c_int {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return RQL_MISUSE;
    };
    if let Some(s) = handle.stmt.as_mut() {
        s.clear_bindings();
    }
    RQL_OK
}

/// Step a statement. Returns 100 (ROW), 101 (DONE), or an error code.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_step(stmt: *mut RqlStmt) -> c_int {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return RQL_MISUSE;
    };
    let Some(conn) = (unsafe { handle.conn.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(s) = handle.stmt.as_mut() else {
        return RQL_MISUSE;
    };
    handle.text_bufs.clear();
    // Read statements step under the read lock (parallel readers);
    // writes take the write lock (serialized writers).
    let outcome = if handle.is_write {
        let _w = conn.db.write();
        s.step()
    } else {
        let _r = conn.db.read();
        s.step()
    };
    match outcome {
        Ok(crate::statement::StepResult::Row) => 100,
        Ok(crate::statement::StepResult::Done) => 101,
        Err(e) => set_conn_error(conn, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Parameter binding (1-based, like sqlite3_bind_*)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_int64(stmt: *mut RqlStmt, idx: c_int, v: i64) -> c_int {
    bind_value(stmt, idx, Value::Integer(v))
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_int(stmt: *mut RqlStmt, idx: c_int, v: c_int) -> c_int {
    bind_value(stmt, idx, Value::Integer(v as i64))
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_double(stmt: *mut RqlStmt, idx: c_int, v: f64) -> c_int {
    bind_value(stmt, idx, Value::Real(v))
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_null(stmt: *mut RqlStmt, idx: c_int) -> c_int {
    bind_value(stmt, idx, Value::Null)
}

/// Bind text. `len < 0` uses strlen. `destructor`: 0 = static (the buffer
/// outlives the call), -1 (SQLITE_TRANSIENT) = copy, otherwise called
/// with the pointer after the copy.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_text(
    stmt: *mut RqlStmt,
    idx: c_int,
    s: *const c_char,
    len: c_int,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return RQL_MISUSE;
    };
    let bytes = if s.is_null() {
        Vec::new()
    } else if len < 0 {
        unsafe { CStr::from_ptr(s) }.to_bytes().to_vec()
    } else {
        unsafe { std::slice::from_raw_parts(s as *const u8, len as usize) }.to_vec()
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let rc = bind_value(stmt, idx, Value::Text(text.into()));
    if let Some(d) = destructor {
        if !s.is_null() {
            unsafe { d(s as *mut c_void) };
        }
    }
    let _ = handle;
    rc
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_blob(
    stmt: *mut RqlStmt,
    idx: c_int,
    data: *const c_void,
    len: c_int,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let bytes = if data.is_null() || len <= 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data as *const u8, len as usize) }.to_vec()
    };
    let rc = bind_value(stmt, idx, Value::Blob(bytes));
    if let Some(d) = destructor {
        if !data.is_null() {
            unsafe { d(data as *mut c_void) };
        }
    }
    rc
}

fn bind_value(stmt: *mut RqlStmt, idx: c_int, v: Value) -> c_int {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return RQL_MISUSE;
    };
    let Some(s) = handle.stmt.as_mut() else {
        return RQL_MISUSE;
    };
    match s.bind(idx as usize, v) {
        Ok(()) => RQL_OK,
        Err(e) => set_thread_error_str(&e.to_string()),
    }
}

/// Number of positional parameters.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_parameter_count(stmt: *mut RqlStmt) -> c_int {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return 0;
    };
    handle.stmt.as_ref().map(|s| s.parameter_count()).unwrap_or(0) as c_int
}

/// Index of a named parameter (SQL: `:name`), 0 when unknown.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_bind_parameter_index(
    stmt: *mut RqlStmt,
    name: *const c_char,
) -> c_int {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return 0;
    };
    let Some(n) = cstr(name) else {
        return 0;
    };
    // Named parameters exist beyond the positional count; report 1 when a
    // match exists (callers then use bind_named through the Rust API or a
    // future named-binding C entry point).
    let found = handle
        .stmt
        .as_ref()
        .map(|s| s.parameter_names().iter().any(|p| p.eq_ignore_ascii_case(n.trim_start_matches([':', '@', '$']))))
        .unwrap_or(false);
    if found {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Column access (0-based, like sqlite3_column_*)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_count(stmt: *mut RqlStmt) -> c_int {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return 0;
    };
    handle.stmt.as_ref().map(|s| s.column_count()).unwrap_or(0) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_name(stmt: *mut RqlStmt, i: c_int) -> *const c_char {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return std::ptr::null();
    };
    match handle.stmt.as_ref().and_then(|s| s.column_name(i as usize)) {
        Some(n) => {
            // Stash a NUL-terminated copy valid until the next step.
            let cs = CString::new(n).unwrap_or_default();
            let p = cs.as_ptr();
            std::mem::forget(cs);
            handle.text_bufs.push(unsafe { CString::from_raw(p as *mut c_char) });
            p
        }
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_int64(stmt: *mut RqlStmt, i: c_int) -> i64 {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return 0;
    };
    handle.stmt.as_ref().map(|s| s.column_int(i as usize)).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_double(stmt: *mut RqlStmt, i: c_int) -> f64 {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return 0.0;
    };
    handle
        .stmt
        .as_ref()
        .map(|s| s.column_real(i as usize))
        .unwrap_or(0.0)
}

/// NUL-terminated column text, valid until the next step/reset/finalize.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_text(stmt: *mut RqlStmt, i: c_int) -> *const c_uchar {
    let Some(handle) = (unsafe { stmt.as_mut() }) else {
        return std::ptr::null();
    };
    let text = handle
        .stmt
        .as_ref()
        .and_then(|s| s.column_text(i as usize));
    match text {
        Some(t) => {
            let cs = CString::new(t).unwrap_or_default();
            let p = cs.as_ptr() as *const c_uchar;
            std::mem::forget(cs);
            handle.text_bufs.push(unsafe { CString::from_raw(p as *mut c_char) });
            p
        }
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_blob(stmt: *mut RqlStmt, i: c_int) -> *const c_void {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return std::ptr::null();
    };
    // Blob pointers are valid until the next step (the row lives in the
    // statement) — SQLite's contract.
    match handle
        .stmt
        .as_ref()
        .and_then(|s| s.column_value(i as usize))
    {
        Some(Value::Blob(b)) => b.as_ptr() as *const c_void,
        Some(Value::Text(t)) => t.as_str().as_ptr() as *const c_void,
        _ => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_bytes(stmt: *mut RqlStmt, i: c_int) -> c_int {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return 0;
    };
    match handle
        .stmt
        .as_ref()
        .and_then(|s| s.column_value(i as usize))
    {
        Some(Value::Blob(b)) => b.len() as c_int,
        Some(Value::Text(t)) => t.as_str().as_bytes().len() as c_int,
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_column_type(stmt: *mut RqlStmt, i: c_int) -> c_int {
    let Some(handle) = (unsafe { stmt.as_ref() }) else {
        return RQL_NULL;
    };
    match handle
        .stmt
        .as_ref()
        .and_then(|s| s.column_value(i as usize))
    {
        Some(Value::Null) | None => RQL_NULL,
        Some(Value::Integer(_)) => RQL_INTEGER,
        Some(Value::Real(_)) => RQL_FLOAT,
        Some(Value::Text(_)) => RQL_TEXT,
        Some(Value::Blob(_)) => RQL_BLOB,
    }
}

// ---------------------------------------------------------------------------
// Diagnostics + misc
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn rustqlite_errcode(db: *mut RqlConn) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    conn.last_error.lock().as_bytes().is_empty().then(|| RQL_OK).unwrap_or(RQL_ERROR)
}

/// Last error message (valid until the next call on this connection).
#[no_mangle]
pub unsafe extern "C" fn rustqlite_errmsg(db: *mut RqlConn) -> *const c_char {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return b"\0".as_ptr() as *const c_char;
    };
    conn.last_error.lock().as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_changes(db: *mut RqlConn) -> i64 {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return 0;
    };
    let rd = conn.db.read();
    rd.changes()
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_total_changes(db: *mut RqlConn) -> i64 {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return 0;
    };
    let rd = conn.db.read();
    rd.total_changes()
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_last_insert_rowid(db: *mut RqlConn) -> i64 {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return 0;
    };
    conn.db.read().last_insert_rowid()
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_libversion() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

#[no_mangle]
pub unsafe extern "C" fn rustqlite_threadsafe() -> c_int {
    1
}

/// Get the engine version as a Rust &str.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_source_id() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), " rustqlite C ABI\0").as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------
// Plugin registration (C side)
// ---------------------------------------------------------------------------

/// `sqlite3_create_function` equivalent: scalar (xFunc) or aggregate
/// (xStep + xFinal). `n_arg < 0` = variadic.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_create_function(
    db: *mut RqlConn,
    name: *const c_char,
    n_arg: c_int,
    _e_text_rep: c_int,
    p_app: *mut c_void,
    x_func: Option<unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue)>,
    x_step: Option<unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue)>,
    x_final: Option<unsafe extern "C" fn(*mut RqlContext)>,
) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(name) = cstr(name) else {
        return RQL_MISUSE;
    };
    let mut w = conn.db.write();
    let res = if let Some(xf) = x_func {
        w.create_function_arc(std::sync::Arc::new(CScalar {
            name: name.to_string(),
            n_arg,
            app: p_app,
            x_func: xf,
        }))
    } else if let (Some(xs), Some(xfin)) = (x_step, x_final) {
        w.create_aggregate_arc(std::sync::Arc::new(CAggregate {
            name: name.to_string(),
            n_arg,
            app: p_app,
            x_step: xs,
            x_final: xfin,
        }))
    } else {
        Err(Error::semantic("create_function requires xFunc or xStep+xFinal"))
    };
    drop(w);
    match res {
        Ok(()) => RQL_OK,
        Err(e) => set_conn_error(conn, &e.to_string()),
    }
}

/// `sqlite3_create_collation` equivalent.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_create_collation(
    db: *mut RqlConn,
    name: *const c_char,
    p_app: *mut c_void,
    x_compare: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
    >,
) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(name) = cstr(name) else {
        return RQL_MISUSE;
    };
    let Some(xc) = x_compare else {
        // NULL unregisters (drop the override; built-ins remain).
        return RQL_OK;
    };
    let mut w = conn.db.write();
    let res = w.create_collation_arc(std::sync::Arc::new(CCollation {
        name: name.to_string(),
        app: p_app,
        x_compare: xc,
    }));
    drop(w);
    match res {
        Ok(()) => RQL_OK,
        Err(e) => set_conn_error(conn, &e.to_string()),
    }
}

/// `sqlite3_create_module` equivalent: registers a C vtab module.
#[no_mangle]
pub unsafe extern "C" fn rustqlite_create_module(
    db: *mut RqlConn,
    name: *const c_char,
    module: *const RqlModule,
    p_aux: *mut c_void,
) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(name) = cstr(name) else {
        return RQL_MISUSE;
    };
    if module.is_null() {
        return RQL_MISUSE;
    }
    let mut w = conn.db.write();
    let res = w.create_module_arc(std::sync::Arc::new(CVtabModule::new(
        name,
        module,
        p_aux,
    )));
    drop(w);
    match res {
        Ok(()) => RQL_OK,
        Err(e) => set_conn_error(conn, &e.to_string()),
    }
}

/// `sqlite3_load_extension` equivalent (feature "extension").
#[no_mangle]
pub unsafe extern "C" fn rustqlite_load_extension(
    db: *mut RqlConn,
    path: *const c_char,
    entry: *const c_char,
) -> c_int {
    let Some(conn) = (unsafe { db.as_ref() }) else {
        return RQL_MISUSE;
    };
    let Some(p) = cstr(path) else {
        return RQL_MISUSE;
    };
    let entry = if entry.is_null() {
        None
    } else {
        cstr(entry)
    };
    let mut w = conn.db.write();
    #[cfg(feature = "extension")]
    let res = w.load_extension(std::path::Path::new(p), entry);
    #[cfg(not(feature = "extension"))]
    let res = {
        let _ = (&mut *w, p, entry); // path/entry parsed for arg validation
        Err(crate::error::Error::semantic(
            "extension loading is disabled in this build (enable the `extension` feature)",
        ))
    };
    drop(w);
    match res {
        Ok(()) => RQL_OK,
        Err(e) => set_conn_error(conn, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Internal bridges used by plugin::abi trampolines
// ---------------------------------------------------------------------------

/// The extension handle IS the raw Database pointer.
pub(crate) fn make_extension_handle(db: &mut Database) -> *mut RqlDb {
    db as *mut Database as *mut RqlDb
}

/// Recover a `&mut Database` from an extension handle (valid only during
/// the extension-init borrow).
pub(crate) fn db_from_handle(db: *mut RqlDb) -> Option<&'static mut Database> {
    if db.is_null() {
        return None;
    }
    Some(unsafe { &mut *(db as *mut Database) })
}

/// Execute SQL on a raw extension handle.
pub(crate) fn exec_on_handle(db: *mut RqlDb, sql: &str) -> Result<()> {
    let Some(d) = db_from_handle(db) else {
        return Err(Error::runtime("null handle"));
    };
    let guard = crate::plugin::abi::ThreadDbGuard::install(d as *const Database as *mut Database);
    let res = d.execute(sql, []);
    drop(guard);
    res
}

thread_local! {
    static EXT_ERR: std::cell::RefCell<CString> = std::cell::RefCell::new(
        CString::new("").unwrap()
    );
}

pub(crate) fn errmsg_ptr(_db: *mut RqlDb) -> *const c_char {
    EXT_ERR.with(|e| e.borrow().as_ptr())
}

/// Register a C scalar function onto a Database (trampoline target).
pub(crate) fn register_c_scalar(
    db: &mut Database,
    name: &str,
    n_arg: c_int,
    app: *mut c_void,
    x_func: unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue),
) -> Result<()> {
    db.create_function_arc(std::sync::Arc::new(CScalar {
        name: name.to_string(),
        n_arg,
        app,
        x_func,
    }))
}

pub(crate) fn register_c_aggregate(
    db: &mut Database,
    name: &str,
    n_arg: c_int,
    app: *mut c_void,
    x_step: unsafe extern "C" fn(*mut RqlContext, c_int, *mut *mut RqlValue),
    x_final: unsafe extern "C" fn(*mut RqlContext),
) -> Result<()> {
    db.create_aggregate_arc(std::sync::Arc::new(CAggregate {
        name: name.to_string(),
        n_arg,
        app,
        x_step,
        x_final,
    }))
}

pub(crate) fn register_c_collation(
    db: &mut Database,
    name: &str,
    app: *mut c_void,
    x_compare: unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
) -> Result<()> {
    db.create_collation_arc(std::sync::Arc::new(CCollation {
        name: name.to_string(),
        app,
        x_compare,
    }))
}

pub(crate) fn register_c_module(
    db: &mut Database,
    name: &str,
    module: &RqlModule,
    p_aux: *mut c_void,
) -> Result<()> {
    db.create_module_arc(std::sync::Arc::new(CVtabModule::new(
        name,
        module as *const RqlModule,
        p_aux,
    )))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn set_conn_error(conn: &RqlConn, msg: &str) -> c_int {
    {
        let mut e = conn.last_error.lock();
        *e = CString::new(msg).unwrap_or_default();
    }
    EXT_ERR.with(|e| *e.borrow_mut() = CString::new(msg).unwrap_or_default());
    RQL_ERROR
}

fn set_thread_error_str(msg: &str) -> c_int {
    EXT_ERR.with(|e| *e.borrow_mut() = CString::new(msg).unwrap_or_default());
    RQL_ERROR
}

fn is_write_statement(sql: &str) -> bool {
    let head = sql.trim_start().to_ascii_lowercase();
    head.starts_with("insert")
        || head.starts_with("update")
        || head.starts_with("delete")
        || head.starts_with("replace")
}

/// Byte offset just past the prepared statement (naive: the full text —
/// the engine's prepare consumes exactly one statement; multi-statement
/// callers advance by the returned tail).
fn statement_tail_offset(full: &str, prepared: &str) -> usize {
    let _ = prepared;
    // The engine's parser consumes up to (and including) one semicolon.
    match full.find(';') {
        Some(i) => (i + 1).min(full.len()),
        None => full.len(),
    }
}
