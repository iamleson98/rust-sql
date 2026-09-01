//! SQLite C ABI conformance tests for the rustqlite-compat layer.
//!
//! These call the `sqlite3_*` symbols exactly the way C programs (and
//! sqlx's worker thread) do — raw pointers, 1-based binds, step/column
//! lifetimes — and pin the observable behavior to SQLite's documented
//! semantics: result codes, extended error codes, tail offsets,
//! column-name timing, transaction state, and connection bookkeeping.
//!
//! The compat crate is linked as an rlib, so the symbols resolve directly.

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::ptr;

// The compat library's ABI (crate name `sqlite3`).
// The compat library (lib name `sqlite3`) exports the sqlite3_* symbols.
extern crate sqlite3 as compat;

// Declare the surface we drive (matching sqlite3.h exactly).
extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        ppdb: *mut *mut compat::sqlite3,
        flags: c_int,
        zvfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_errcode(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_prepare_v3(
        db: *mut compat::sqlite3,
        zsql: *const c_char,
        nbyte: c_int,
        flags: c_int,
        ppstmt: *mut *mut compat::sqlite3_stmt,
        pztail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_step(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_reset(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut compat::sqlite3_stmt, idx: c_int, v: i64) -> c_int;
    fn sqlite3_bind_text64(
        stmt: *mut compat::sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        len: u64,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
        encoding: u8,
    ) -> c_int;
    fn sqlite3_bind_null(stmt: *mut compat::sqlite3_stmt, idx: c_int) -> c_int;
    fn sqlite3_column_count(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_column_name(stmt: *mut compat::sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_column_type(stmt: *mut compat::sqlite3_stmt, i: c_int) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_text(stmt: *mut compat::sqlite3_stmt, i: c_int) -> *const u8;
    fn sqlite3_column_value(
        stmt: *mut compat::sqlite3_stmt,
        i: c_int,
    ) -> *mut compat::sqlite3_value;
    fn sqlite3_value_int64(v: *const compat::sqlite3_value) -> i64;
    fn sqlite3_value_type(v: *const compat::sqlite3_value) -> c_int;
    fn sqlite3_value_dup(v: *const compat::sqlite3_value) -> *mut compat::sqlite3_value;
    fn sqlite3_value_free(v: *mut compat::sqlite3_value);
    fn sqlite3_changes(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_total_changes(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_last_insert_rowid(db: *mut compat::sqlite3) -> i64;
    fn sqlite3_get_autocommit(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_threadsafe() -> c_int;
    fn sqlite3_stmt_readonly(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_sql(stmt: *mut compat::sqlite3_stmt) -> *const c_char;
    fn sqlite3_bind_parameter_count(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_parameter_name(stmt: *mut compat::sqlite3_stmt, idx: c_int) -> *const c_char;
    fn sqlite3_busy_timeout(db: *mut compat::sqlite3, ms: c_int) -> c_int;
    fn sqlite3_extended_result_codes(db: *mut compat::sqlite3, on: c_int) -> c_int;
    fn sqlite3_exec(
        db: *mut compat::sqlite3,
        sql: *const c_char,
        cb: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_BUSY: c_int = 5;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_CONSTRAINT: c_int = 19;
const SQLITE_CONSTRAINT_UNIQUE: c_int = 2067;
const SQLITE_CONSTRAINT_NOTNULL: c_int = 1299;
const SQLITE_OPEN_READWRITE: c_int = 0x2;
const SQLITE_OPEN_CREATE: c_int = 0x4;
const SQLITE_OPEN_MEMORY: c_int = 0x80;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_NULL: c_int = 5;

struct Db(*mut compat::sqlite3);
impl Drop for Db {
    fn drop(&mut self) {
        unsafe { sqlite3_close(self.0) };
    }
}
struct St(*mut compat::sqlite3_stmt);
impl Drop for St {
    fn drop(&mut self) {
        unsafe { sqlite3_finalize(self.0) };
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Open an in-memory database with the flags sqlx uses.
fn open_memory() -> Db {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    let rc = unsafe {
        sqlite3_open_v2(
            name.as_ptr(),
            &mut db,
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_MEMORY,
            ptr::null(),
        )
    };
    assert_eq!(rc, SQLITE_OK, "open_v2 failed");
    unsafe { sqlite3_extended_result_codes(db, 1) };
    unsafe { sqlite3_busy_timeout(db, 5000) };
    Db(db)
}

/// Prepare one statement (NUL-terminated).
fn prepare(db: &Db, sql: &str) -> (St, usize) {
    let csql = CString::new(sql).unwrap();
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe {
        sqlite3_prepare_v3(
            db.0,
            csql.as_ptr(),
            -1,
            0,
            &mut stmt,
            &mut tail,
        )
    };
    assert_eq!(rc, SQLITE_OK, "prepare failed for {:?}: {}", sql, unsafe {
        cstr(sqlite3_errmsg(db.0))
    });
    let consumed = if tail.is_null() {
        csql.as_ptr() as usize + sql.len()
    } else {
        tail as usize - csql.as_ptr() as usize
    };
    (St(stmt), consumed)
}

fn exec(db: &Db, sql: &str) {
    let csql = CString::new(sql).unwrap();
    let rc = unsafe {
        sqlite3_exec(db.0, csql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
    };
    assert_eq!(rc, SQLITE_OK, "exec({:?}) failed: {}", sql, unsafe {
        cstr(sqlite3_errmsg(db.0))
    });
}

/// Step a SELECT to completion, collecting rows of i64 triples.
fn step_all_text(stmt: &mut St) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    loop {
        let rc = unsafe { sqlite3_step(stmt.0) };
        match rc {
            SQLITE_ROW => {
                let n = unsafe { sqlite3_column_count(stmt.0) };
                let mut row = Vec::with_capacity(n as usize);
                for i in 0..n {
                    let p = unsafe { sqlite3_column_text(stmt.0, i) };
                    if p.is_null() {
                        row.push(String::new());
                    } else {
                        let bytes = unsafe { CStr::from_ptr(p as *const c_char) };
                        row.push(bytes.to_string_lossy().into_owned());
                    }
                }
                out.push(row);
            }
            SQLITE_DONE => break,
            other => panic!("step returned {}", other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn abi_lifecycle_open_close() {
    let db = open_memory();
    assert!(!db.0.is_null());
    unsafe { sqlite3_close(db.0) };
    // Db::drop would double-close; forget it (already closed).
    std::mem::forget(db);
}

#[test]
fn abi_libversion_shape() {
    let v = cstr(unsafe { sqlite3_libversion() });
    let parts: Vec<u32> = v.split('.').map(|p| p.parse().unwrap()).collect();
    assert_eq!(parts.len() >= 3, true, "version must be X.Y.Z: {}", v);
    assert_eq!(unsafe { sqlite3_threadsafe() }, 1);
}

#[test]
fn abi_column_names_available_before_first_step() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (a INTEGER, b TEXT)");
    // Materialized plan (aggregate) — names must STILL be present at
    // prepare time (sqlx reads column_name before stepping).
    let (mut stmt, _) = prepare(&db, "SELECT a, COUNT(*), b AS label FROM t GROUP BY b");
    let n = unsafe { sqlite3_column_count(stmt.0) };
    assert_eq!(n, 3, "COUNT(*) included");
    let c0 = cstr(unsafe { sqlite3_column_name(stmt.0, 0) });
    let c1 = cstr(unsafe { sqlite3_column_name(stmt.0, 1) });
    let c2 = cstr(unsafe { sqlite3_column_name(stmt.0, 2) });
    assert_eq!(c0, "a");
    assert_eq!(c1, "COUNT(*)");
    assert_eq!(c2, "label", "alias must win");
    // A second prepare of the same shape must be stable after reset.
    unsafe { sqlite3_reset(stmt.0) };
    let n2 = unsafe { sqlite3_column_count(stmt.0) };
    assert_eq!(n2, 3, "column_count survives reset");
}

#[test]
fn abi_dml_without_returning_has_zero_columns() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
    let (stmt, _) = prepare(&db, "INSERT INTO t (v) VALUES (5)");
    let n = unsafe { sqlite3_column_count(stmt.0) };
    assert_eq!(n, 0, "INSERT without RETURNING reports 0 columns");
}

#[test]
fn abi_dml_with_returning_reports_columns_at_prepare() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
    let (stmt, _) = prepare(&db, "INSERT INTO t (v) VALUES (5) RETURNING id, v * 2 AS dbl");
    let n = unsafe { sqlite3_column_count(stmt.0) };
    assert_eq!(n, 2);
    let c0 = cstr(unsafe { sqlite3_column_name(stmt.0, 0) });
    let c1 = cstr(unsafe { sqlite3_column_name(stmt.0, 1) });
    assert_eq!(c0, "id");
    assert_eq!(c1, "dbl");
}

#[test]
fn abi_step_bind_roundtrip() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    exec(&db, "INSERT INTO t (name) VALUES ('one'), ('two')");

    let (mut stmt, _) = prepare(&db, "SELECT id, name FROM t WHERE id >= ? ORDER BY id");
    let rc = unsafe { sqlite3_bind_int64(stmt.0, 1, 1) };
    assert_eq!(rc, SQLITE_OK);
    let rows = step_all_text(&mut stmt);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], "one");
    assert_eq!(rows[1][1], "two");
}

#[test]
fn abi_reset_reexecutes_with_new_binds() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    exec(&db, "INSERT INTO t (name) VALUES ('a'), ('b'), ('c')");
    let (mut stmt, _) = prepare(&db, "SELECT name FROM t WHERE id = ?");
    for (idx, want) in [(1i64, "a"), (2, "b"), (3, "c")] {
        unsafe {
            let rc = sqlite3_reset(stmt.0);
            assert_eq!(rc, SQLITE_OK);
            sqlite3_bind_int64(stmt.0, 1, idx);
        }
        let rows = step_all_text(&mut stmt);
        assert_eq!(rows.len(), 1, "id={} should match 1 row", idx);
        assert_eq!(rows[0][0], want);
    }
}

#[test]
fn abi_named_parameters() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (x INTEGER)");
    let (stmt, _) = prepare(&db, "INSERT INTO t (x) VALUES (:val)");
    assert_eq!(unsafe { sqlite3_bind_parameter_count(stmt.0) }, 1);
    let name = cstr(unsafe { sqlite3_bind_parameter_name(stmt.0, 1) });
    assert_eq!(name, ":val", "SQLite reports the name WITH its sigil");
    // positional ? reports NULL
    let (stmt2, _) = prepare(&db, "SELECT x FROM t WHERE x = ?");
    assert!(unsafe { sqlite3_bind_parameter_name(stmt2.0, 1) }.is_null());
}

#[test]
fn abi_multi_statement_tail() {
    let db = open_memory();
    let script = "CREATE TABLE m (x); INSERT INTO m VALUES (1); INSERT INTO m VALUES (2);";
    let mut remaining = script.to_string();
    let mut stmts = 0;
    while !remaining.trim().is_empty() {
        let csql = CString::new(remaining.clone()).unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe {
            sqlite3_prepare_v3(db.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail)
        };
        assert_eq!(rc, SQLITE_OK);
        let consumed = if tail.is_null() {
            remaining.len()
        } else {
            tail as usize - csql.as_ptr() as usize
        };
        if !stmt.is_null() {
            let rc = unsafe { sqlite3_step(stmt) };
            assert!(rc == SQLITE_DONE || rc == SQLITE_ROW, "step rc={}", rc);
            unsafe { sqlite3_finalize(stmt) };
            stmts += 1;
        }
        remaining = remaining[consumed.min(remaining.len())..].to_string();
    }
    assert_eq!(stmts, 3, "three statements in the script");
    let (mut q, _) = prepare(&db, "SELECT SUM(x) FROM m");
    let rows = step_all_text(&mut q);
    assert_eq!(rows[0][0], "3");
}

#[test]
fn abi_tail_handles_semicolons_inside_strings() {
    let db = open_memory();
    exec(&db, "CREATE TABLE s (v TEXT)");
    let script = "INSERT INTO s VALUES ('a;b'); SELECT COUNT(*) FROM s";
    let csql = CString::new(script).unwrap();
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe { sqlite3_prepare_v3(db.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
    assert_eq!(rc, SQLITE_OK);
    // The first statement ends right AFTER the string-literal semicolon
    // (pzTail = first byte past the statement — no trailing whitespace).
    let consumed = tail as usize - csql.as_ptr() as usize;
    assert_eq!(&script[..consumed], "INSERT INTO s VALUES ('a;b');");
    unsafe { sqlite3_step(stmt) };
    unsafe { sqlite3_finalize(stmt) };
}

#[test]
fn abi_changes_and_last_rowid() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)");
    for i in 1..=3 {
        let (mut stmt, _) = prepare(&db, "INSERT INTO t (v) VALUES (?)");
        unsafe { sqlite3_bind_int64(stmt.0, 1, i * 10) };
        let rc = unsafe { sqlite3_step(stmt.0) };
        assert_eq!(rc, SQLITE_DONE);
        assert_eq!(unsafe { sqlite3_changes(db.0) }, 1);
        assert_eq!(unsafe { sqlite3_last_insert_rowid(db.0) }, i);
    }
    assert_eq!(unsafe { sqlite3_total_changes(db.0) }, 3);
    let (mut stmt, _) = prepare(&db, "UPDATE t SET v = v + 1 WHERE id <= 2");
    unsafe { sqlite3_step(stmt.0) };
    assert_eq!(unsafe { sqlite3_changes(db.0) }, 2, "UPDATE changes = 2");
}

#[test]
fn abi_transaction_autocommit_states() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (x INTEGER)");
    assert_eq!(unsafe { sqlite3_get_autocommit(db.0) }, 1, "autocommit on");

    // BEGIN via the prepared path (how sqlx does it).
    let (mut begin, _) = prepare(&db, "BEGIN");
    unsafe { sqlite3_step(begin.0) };
    assert_eq!(unsafe { sqlite3_get_autocommit(db.0) }, 0, "in transaction");

    // Nested BEGIN must fail like SQLite.
    let (mut nested, _) = prepare(&db, "BEGIN");
    let rc = unsafe { sqlite3_step(nested.0) };
    assert_eq!(rc, SQLITE_ERROR, "nested BEGIN error code");
    let msg = unsafe { cstr(sqlite3_errmsg(db.0)) };
    assert!(
        msg.contains("within a transaction"),
        "SQLite message shape: {}",
        msg
    );

    let (mut commit, _) = prepare(&db, "COMMIT");
    unsafe { sqlite3_step(commit.0) };
    assert_eq!(unsafe { sqlite3_get_autocommit(db.0) }, 1, "back to autocommit");
}

#[test]
fn abi_constraint_extended_error_codes() {
    let db = open_memory();
    exec(&db, "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)");
    exec(&db, "INSERT INTO u (email) VALUES ('a@b')");

    // UNIQUE violation -> 2067, message shape matches SQLite.
    let (mut stmt, _) = prepare(&db, "INSERT INTO u (email) VALUES ('a@b')");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_UNIQUE, "SQLITE_CONSTRAINT_UNIQUE (2067)");
    let msg = unsafe { cstr(sqlite3_errmsg(db.0)) };
    assert_eq!(msg, "UNIQUE constraint failed: u.email");
    assert_eq!(unsafe { sqlite3_errcode(db.0) }, SQLITE_CONSTRAINT_UNIQUE);

    // NOT NULL violation -> 527.
    let (mut stmt, _) = prepare(&db, "INSERT INTO u (id, email) VALUES (2, NULL)");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_NOTNULL, "SQLITE_CONSTRAINT_NOTNULL (527)");
    let msg = unsafe { cstr(sqlite3_errmsg(db.0)) };
    assert_eq!(msg, "NOT NULL constraint failed: u.email");
    let _ = SQLITE_CONSTRAINT; // base code imported for reference
}

#[test]
fn abi_stmt_readonly_and_sql() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (x INTEGER)");
    let (sel, _) = prepare(&db, "SELECT x FROM t");
    assert_eq!(unsafe { sqlite3_stmt_readonly(sel.0) }, 1, "SELECT is readonly");
    let (ins, _) = prepare(&db, "INSERT INTO t VALUES (1)");
    assert_eq!(unsafe { sqlite3_stmt_readonly(ins.0) }, 0, "INSERT is not");
    let sql = cstr(unsafe { sqlite3_sql(ins.0) });
    assert_eq!(sql, "INSERT INTO t VALUES (1)");
}

#[test]
fn abi_column_value_objects() {
    // sqlite3_column_value + dup/free — the exact path sqlx uses to build
    // rows (SqliteRow::current).
    let db = open_memory();
    exec(&db, "CREATE TABLE t (a INTEGER, b TEXT)");
    exec(&db, "INSERT INTO t VALUES (42, 'hi')");
    let (mut stmt, _) = prepare(&db, "SELECT a, b FROM t");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_ROW);
    let v0 = unsafe { sqlite3_column_value(stmt.0, 0) };
    let v1 = unsafe { sqlite3_column_value(stmt.0, 1) };
    assert!(!v0.is_null() && !v1.is_null());
    assert_eq!(unsafe { sqlite3_value_type(v0) }, SQLITE_INTEGER);
    assert_eq!(unsafe { sqlite3_value_type(v1) }, SQLITE_TEXT);
    assert_eq!(unsafe { sqlite3_value_int64(v0) }, 42);
    // dup survives stepping onward (sqlx dups every column).
    let d0 = unsafe { sqlite3_value_dup(v0) };
    assert!(!d0.is_null());
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_DONE);
    assert_eq!(unsafe { sqlite3_value_int64(d0) }, 42, "dup outlives the row");
    unsafe { sqlite3_value_free(d0) };
}

#[test]
fn abi_column_types_and_int_coercion() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (i INTEGER, f REAL, s TEXT)");
    exec(&db, "INSERT INTO t VALUES (7, 2.5, 'txt')");
    let (mut stmt, _) = prepare(&db, "SELECT i, f, s, NULL FROM t");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_ROW);
    assert_eq!(unsafe { sqlite3_column_type(stmt.0, 0) }, SQLITE_INTEGER);
    assert_eq!(unsafe { sqlite3_column_type(stmt.0, 1) }, SQLITE_FLOAT);
    assert_eq!(unsafe { sqlite3_column_type(stmt.0, 2) }, SQLITE_TEXT);
    assert_eq!(unsafe { sqlite3_column_type(stmt.0, 3) }, SQLITE_NULL);
}

#[test]
fn abi_bind_text_and_null() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (s TEXT)");
    let (mut stmt, _) = prepare(&db, "INSERT INTO t VALUES (?)");
    let val = CString::new("hello").unwrap();
    let rc = unsafe {
        sqlite3_bind_text64(
            stmt.0,
            1,
            val.as_ptr(),
            5,
            None,
            3, // SQLITE_UTF8
        )
    };
    assert_eq!(rc, SQLITE_OK);
    unsafe { sqlite3_step(stmt.0) };
    // NULL bind
    let (mut stmt, _) = prepare(&db, "INSERT INTO t VALUES (?)");
    unsafe { sqlite3_bind_null(stmt.0, 1) };
    unsafe { sqlite3_step(stmt.0) };
    let (mut q, _) = prepare(&db, "SELECT COUNT(*), COUNT(s) FROM t");
    let rows = step_all_text(&mut q);
    assert_eq!(rows[0][0], "2", "two rows");
    assert_eq!(rows[0][1], "1", "one non-NULL s");
}

#[test]
fn abi_cross_connection_shared_file() {
    // Two connections on the same file see one engine — committed data
    // from A is immediately visible to B (this is what makes sqlx pools
    // coherent on rustqlite).
    let path = std::env::temp_dir().join(format!("compat-cc-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cpath = CString::new(path.to_str().unwrap()).unwrap();

    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;
    unsafe {
        assert_eq!(sqlite3_open_v2(cpath.as_ptr(), &mut a, flags, ptr::null()), SQLITE_OK);
        assert_eq!(sqlite3_open_v2(cpath.as_ptr(), &mut b, flags, ptr::null()), SQLITE_OK);
    }
    let (a, b) = (Db(a), Db(b));

    exec(&a, "CREATE TABLE t (x INTEGER)");
    exec(&a, "INSERT INTO t VALUES (1), (2)");
    // B sees it immediately.
    let (mut q, _) = prepare(&b, "SELECT SUM(x) FROM t");
    let rows = step_all_text(&mut q);
    assert_eq!(rows[0][0], "3");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn abi_transaction_conflict_yields_busy_then_succeeds() {
    // Connection B's BEGIN while A holds the engine tx -> BUSY; after A
    // commits, B's BEGIN succeeds (busy_timeout retry).
    let path = std::env::temp_dir().join(format!("compat-tx-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cpath = CString::new(path.to_str().unwrap()).unwrap();
    let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;
    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    unsafe {
        assert_eq!(sqlite3_open_v2(cpath.as_ptr(), &mut a, flags, ptr::null()), SQLITE_OK);
        assert_eq!(sqlite3_open_v2(cpath.as_ptr(), &mut b, flags, ptr::null()), SQLITE_OK);
        // Short timeout on B so the test is fast; A holds a tx.
        sqlite3_busy_timeout(b, 150);
    }
    let (a, b) = (Db(a), Db(b));
    exec(&a, "CREATE TABLE t (x INTEGER)");
    exec(&a, "INSERT INTO t VALUES (1)");

    exec(&a, "BEGIN");
    exec(&a, "UPDATE t SET x = 2");

    // B's BEGIN hits BUSY (timeout 150 ms).
    let (mut stmt, _) = prepare(&b, "BEGIN");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, 5, "SQLITE_BUSY while A holds the transaction");

    // A commits; B retries and succeeds (a successful BEGIN/COMMIT step
    // returns SQLITE_DONE — SQLITE_OK never comes out of sqlite3_step).
    exec(&a, "COMMIT");
    let (mut stmt, _) = prepare(&b, "BEGIN");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_DONE);
    exec(&b, "COMMIT");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn abi_failed_open_reports_cantopen_with_usable_errmsg() {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = CString::new("/nonexistent-dir-xyz/nope.db").unwrap();
    let rc = unsafe {
        sqlite3_open_v2(
            name.as_ptr(),
            &mut db,
            SQLITE_OPEN_READWRITE, // no CREATE
            ptr::null(),
        )
    };
    assert_eq!(rc, 14, "SQLITE_CANTOPEN");
    assert!(!db.is_null(), "handle exists for errmsg (SQLite behavior)");
    let msg = unsafe { cstr(sqlite3_errmsg(db)) };
    assert_eq!(msg, "unable to open database file");
    unsafe { sqlite3_close(db) };
}

#[test]
fn abi_uri_mode_memory_private() {
    // file::memory:?cache=private — two opens = two SEPARATE in-memory
    // databases (SQLite semantics).
    let uri = CString::new("file:compatmem1?mode=memory").unwrap();
    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | 0x40; // URI
    unsafe {
        assert_eq!(sqlite3_open_v2(uri.as_ptr(), &mut a, flags, ptr::null()), SQLITE_OK);
        assert_eq!(sqlite3_open_v2(uri.as_ptr(), &mut b, flags, ptr::null()), SQLITE_OK);
    }
    let (a, b) = (Db(a), Db(b));
    exec(&a, "CREATE TABLE t (x INTEGER)");
    exec(&a, "INSERT INTO t VALUES (1)");
    let (mut q, _) = prepare(&b, "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='t'");
    // B should NOT see A's private memory table... but note: our engine
    // treats plain mode=memory as per-open private engines.
    let rows = step_all_text(&mut q);
    assert_eq!(rows[0][0], "0", "private memory databases are isolated");
}

#[test]
fn abi_pragmas_via_prepared_statements() {
    // sqlx sends `PRAGMA foreign_keys = ON` through the prepare+step path
    // (pragma_string). Verify write pragmas execute and read pragmas
    // return rows.
    let db = open_memory();
    let (mut stmt, _) = prepare(&db, "PRAGMA foreign_keys = ON");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_DONE);

    let (mut q, _) = prepare(&db, "PRAGMA foreign_keys");
    let rows = step_all_text(&mut q);
    assert_eq!(rows.len(), 1, "read pragma returns one row");
    assert_eq!(rows[0][0], "1");

    let (mut q2, _) = prepare(&db, "PRAGMA page_size");
    let rows = step_all_text(&mut q2);
    assert!(!rows.is_empty());
    let sz: i64 = rows[0][0].parse().unwrap();
    assert!(sz >= 512 && sz <= 65536, "page_size in range: {}", sz);
}

// ===========================================================================
// UPDATE constraint semantics through the C ABI (sqlite3_step path —
// exactly what sqlx drives)
// ===========================================================================

const SQLITE_MISMATCH: c_int = 20;
const SQLITE_CONSTRAINT_FOREIGNKEY: c_int = 787;

#[test]
fn abi_update_unique_violation_code_and_errmsg() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };

    let (mut st, _) = prepare(&db, "UPDATE t SET v = 'a' WHERE id = 2");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_UNIQUE, "extended UNIQUE code");
    // errmsg must be byte-exact (no engine prefix) — sqlx pattern-matches it.
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }.to_string_lossy().into_owned();
    assert_eq!(msg, "UNIQUE constraint failed: t.v");
    // errcode persists until reset (SQLite semantics).
    assert_eq!(unsafe { sqlite3_errcode(db.0) }, SQLITE_CONSTRAINT_UNIQUE);
    let rc2 = unsafe { sqlite3_reset(st.0) };
    assert_eq!(rc2, SQLITE_CONSTRAINT_UNIQUE, "reset re-reports the error");
}

#[test]
fn abi_update_atomic_abort_keeps_table_unchanged() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");

    // Multi-row UPDATE where row 3 violates: SQLite aborts the whole
    // statement — rows 1-2 must NOT be half-updated.
    // First shift every value (unique — succeeds), then attempt a bulk
    // collapse to one value: row 2 conflicts, the statement aborts, and
    // NO row keeps the half-applied value.
    let (mut st, _) = prepare(&db, "UPDATE t SET v = 'x' || id");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_DONE, "x1/x2/x3 are distinct");
    let (mut st2, _) = prepare(&db, "UPDATE t SET v = 'same'");
    let rc2 = unsafe { sqlite3_step(st2.0) };
    assert_eq!(rc2, SQLITE_CONSTRAINT_UNIQUE, "all rows collapse onto 'same'");
    // Table unchanged after the abort.
    let (mut st3, _) = prepare(&db, "SELECT COUNT(*) FROM t WHERE v IN ('x1','x2','x3')");
    assert_eq!(unsafe { sqlite3_step(st3.0) }, SQLITE_ROW);
    let n = unsafe { sqlite3_column_int64(st3.0, 0) };
    assert_eq!(n, 3, "statement aborted atomically — no partial updates");
}

#[test]
fn abi_update_or_ignore_via_step_and_changes() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");

    let (mut st, _) = prepare(&db, "UPDATE OR IGNORE t SET v = 'a'");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_DONE, "OR IGNORE never errors on conflicts");
    // Only row 1 keeps 'a'... rows 2,3 skip; changes() = 1 (SQLite counts
    // only applied rows).
    let changes = unsafe { sqlite3_changes(db.0) };
    assert_eq!(changes, 1, "changes() counts only applied rows");

    let (mut st2, _) = prepare(&db, "SELECT COUNT(*) FROM t WHERE v = 'a'");
    assert_eq!(unsafe { sqlite3_step(st2.0) }, SQLITE_ROW);
    let n = unsafe { sqlite3_column_int64(st2.0, 0) };
    assert_eq!(n, 1, "skipped rows keep their old values");
}

#[test]
fn abi_update_rowid_move_via_step() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    let (mut st, _) = prepare(&db, "UPDATE t SET id = 10 WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_DONE);

    let (mut st2, _) = prepare(&db, "SELECT id, v FROM t ORDER BY id");
    let rows = step_all_text(&mut st2);
    assert_eq!(rows, vec![vec!["2".to_string(), "b".to_string()], vec!["10".to_string(), "a".to_string()]]);

    // Moving to a taken rowid: SQLITE_CONSTRAINT with t.id in the message.
    let (mut st3, _) = prepare(&db, "UPDATE t SET id = 2 WHERE id = 10");
    let rc = unsafe { sqlite3_step(st3.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_UNIQUE);
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }.to_string_lossy().into_owned();
    assert_eq!(msg, "UNIQUE constraint failed: t.id");
}

#[test]
fn abi_update_null_to_rowid_alias_is_mismatch() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&db, "INSERT INTO t VALUES (1, 'a')");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };
    let (mut st, _) = prepare(&db, "UPDATE t SET id = NULL WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_MISMATCH, "SQLite reports SQLITE_MISMATCH");
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }.to_string_lossy().into_owned();
    assert_eq!(msg, "datatype mismatch");
}

#[test]
fn abi_update_fk_violation_extended_code() {
    let db = open_memory();
    exec(&db, "PRAGMA foreign_keys = ON");
    exec(&db, "CREATE TABLE p (id INTEGER PRIMARY KEY)");
    exec(&db, "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INT REFERENCES p(id))");
    exec(&db, "INSERT INTO p VALUES (1)");
    exec(&db, "INSERT INTO c VALUES (1, 1)");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };
    let (mut st, _) = prepare(&db, "UPDATE c SET pid = 99 WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_FOREIGNKEY, "extended FK code");
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }.to_string_lossy().into_owned();
    assert_eq!(msg, "FOREIGN KEY constraint failed");
}

#[test]
fn abi_update_collated_unique_nocase() {
    let db = open_memory();
    exec(&db, "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE UNIQUE)");
    exec(&db, "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta')");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };
    let (mut st, _) = prepare(&db, "UPDATE s SET tag = 'ALPHA' WHERE id = 2");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_UNIQUE, "NOCASE folds 'ALPHA' onto 'Alpha'");
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }.to_string_lossy().into_owned();
    assert_eq!(msg, "UNIQUE constraint failed: s.tag");
}

#[test]
fn abi_update_returning_rows_and_changes() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    let (mut st, _) = prepare(&db, "UPDATE t SET v = v || '!' RETURNING id, v");
    // Column names available at prepare time (SQLite reports them then).
    let name0 = unsafe { CStr::from_ptr(sqlite3_column_name(st.0, 0)) }.to_string_lossy().into_owned();
    let name1 = unsafe { CStr::from_ptr(sqlite3_column_name(st.0, 1)) }.to_string_lossy().into_owned();
    assert_eq!((name0.as_str(), name1.as_str()), ("id", "v"));
    let rows = step_all_text(&mut st);
    assert_eq!(rows, vec![vec!["1".to_string(), "a!".to_string()], vec!["2".to_string(), "b!".to_string()]]);
    assert_eq!(unsafe { sqlite3_changes(db.0) }, 2);
}
