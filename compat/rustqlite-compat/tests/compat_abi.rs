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
    fn sqlite3_bind_blob64(
        stmt: *mut compat::sqlite3_stmt,
        idx: c_int,
        val: *const std::os::raw::c_void,
        len: u64,
        destructor: Option<unsafe extern "C" fn(*mut std::os::raw::c_void)>,
    ) -> c_int;
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
    fn sqlite3_column_blob(
        stmt: *mut compat::sqlite3_stmt,
        i: c_int,
    ) -> *const std::os::raw::c_void;
    fn sqlite3_column_bytes(stmt: *mut compat::sqlite3_stmt, i: c_int) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_text(stmt: *mut compat::sqlite3_stmt, i: c_int) -> *const u8;
    fn sqlite3_column_value(
        stmt: *mut compat::sqlite3_stmt,
        i: c_int,
    ) -> *mut compat::sqlite3_value;
    fn sqlite3_value_int64(v: *const compat::sqlite3_value) -> i64;
    fn sqlite3_value_type(v: *const compat::sqlite3_value) -> c_int;
    fn sqlite3_value_dup(v: *const compat::sqlite3_value) -> *mut compat::sqlite3_value;
    fn sqlite3_value_bytes(v: *const compat::sqlite3_value) -> c_int;
    fn sqlite3_value_text(v: *const compat::sqlite3_value) -> *const u8;
    fn sqlite3_value_blob(v: *const compat::sqlite3_value) -> *const std::os::raw::c_void;
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
        cb: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
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
    let rc = unsafe { sqlite3_prepare_v3(db.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
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
    let rc = unsafe { sqlite3_exec(db.0, csql.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) };
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
    assert!(parts.len() >= 3, "version must be X.Y.Z: {}", v);
    assert_eq!(unsafe { sqlite3_threadsafe() }, 1);
}

#[test]
fn abi_column_names_available_before_first_step() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (a INTEGER, b TEXT)");
    // Materialized plan (aggregate) — names must STILL be present at
    // prepare time (sqlx reads column_name before stepping).
    let (stmt, _) = prepare(&db, "SELECT a, COUNT(*), b AS label FROM t GROUP BY b");
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
    let (stmt, _) = prepare(
        &db,
        "INSERT INTO t (v) VALUES (5) RETURNING id, v * 2 AS dbl",
    );
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
        let rc = unsafe { sqlite3_prepare_v3(db.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
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
        let (stmt, _) = prepare(&db, "INSERT INTO t (v) VALUES (?)");
        unsafe { sqlite3_bind_int64(stmt.0, 1, i * 10) };
        let rc = unsafe { sqlite3_step(stmt.0) };
        assert_eq!(rc, SQLITE_DONE);
        assert_eq!(unsafe { sqlite3_changes(db.0) }, 1);
        assert_eq!(unsafe { sqlite3_last_insert_rowid(db.0) }, i);
    }
    assert_eq!(unsafe { sqlite3_total_changes(db.0) }, 3);
    let (stmt, _) = prepare(&db, "UPDATE t SET v = v + 1 WHERE id <= 2");
    unsafe { sqlite3_step(stmt.0) };
    assert_eq!(unsafe { sqlite3_changes(db.0) }, 2, "UPDATE changes = 2");
}

#[test]
fn abi_transaction_autocommit_states() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (x INTEGER)");
    assert_eq!(unsafe { sqlite3_get_autocommit(db.0) }, 1, "autocommit on");

    // BEGIN via the prepared path (how sqlx does it).
    let (begin, _) = prepare(&db, "BEGIN");
    unsafe { sqlite3_step(begin.0) };
    assert_eq!(unsafe { sqlite3_get_autocommit(db.0) }, 0, "in transaction");

    // Nested BEGIN must fail like SQLite.
    let (nested, _) = prepare(&db, "BEGIN");
    let rc = unsafe { sqlite3_step(nested.0) };
    assert_eq!(rc, SQLITE_ERROR, "nested BEGIN error code");
    let msg = unsafe { cstr(sqlite3_errmsg(db.0)) };
    assert!(
        msg.contains("within a transaction"),
        "SQLite message shape: {}",
        msg
    );

    let (commit, _) = prepare(&db, "COMMIT");
    unsafe { sqlite3_step(commit.0) };
    assert_eq!(
        unsafe { sqlite3_get_autocommit(db.0) },
        1,
        "back to autocommit"
    );
}

#[test]
fn abi_constraint_extended_error_codes() {
    let db = open_memory();
    exec(
        &db,
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)",
    );
    exec(&db, "INSERT INTO u (email) VALUES ('a@b')");

    // UNIQUE violation -> 2067, message shape matches SQLite.
    let (stmt, _) = prepare(&db, "INSERT INTO u (email) VALUES ('a@b')");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(
        rc, SQLITE_CONSTRAINT_UNIQUE,
        "SQLITE_CONSTRAINT_UNIQUE (2067)"
    );
    let msg = unsafe { cstr(sqlite3_errmsg(db.0)) };
    assert_eq!(msg, "UNIQUE constraint failed: u.email");
    assert_eq!(unsafe { sqlite3_errcode(db.0) }, SQLITE_CONSTRAINT_UNIQUE);

    // NOT NULL violation -> 527.
    let (stmt, _) = prepare(&db, "INSERT INTO u (id, email) VALUES (2, NULL)");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(
        rc, SQLITE_CONSTRAINT_NOTNULL,
        "SQLITE_CONSTRAINT_NOTNULL (527)"
    );
    let msg = unsafe { cstr(sqlite3_errmsg(db.0)) };
    assert_eq!(msg, "NOT NULL constraint failed: u.email");
    let _ = SQLITE_CONSTRAINT; // base code imported for reference
}

#[test]
fn abi_stmt_readonly_and_sql() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (x INTEGER)");
    let (sel, _) = prepare(&db, "SELECT x FROM t");
    assert_eq!(
        unsafe { sqlite3_stmt_readonly(sel.0) },
        1,
        "SELECT is readonly"
    );
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
    let (stmt, _) = prepare(&db, "SELECT a, b FROM t");
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
    assert_eq!(
        unsafe { sqlite3_value_int64(d0) },
        42,
        "dup outlives the row"
    );
    unsafe { sqlite3_value_free(d0) };
}

#[test]
fn abi_column_types_and_int_coercion() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (i INTEGER, f REAL, s TEXT)");
    exec(&db, "INSERT INTO t VALUES (7, 2.5, 'txt')");
    let (stmt, _) = prepare(&db, "SELECT i, f, s, NULL FROM t");
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
    let (stmt, _) = prepare(&db, "INSERT INTO t VALUES (?)");
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
    let (stmt, _) = prepare(&db, "INSERT INTO t VALUES (?)");
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
        assert_eq!(
            sqlite3_open_v2(cpath.as_ptr(), &mut a, flags, ptr::null()),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_open_v2(cpath.as_ptr(), &mut b, flags, ptr::null()),
            SQLITE_OK
        );
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
fn abi_relative_path_fresh_db_single_engine() {
    // Regression (sqlx pool + FRESH database, relative path — the
    // sea-orm-migration failure): the engine-map key used to be
    // `canonicalize(path)` with a fallback to the RAW path, which only
    // applies while the file does not exist. The first open of
    // "rel.db" keyed its engine as "rel.db"; that open CREATES the file
    // on disk, so the pool's second connection canonicalized to
    // "/abs/rel.db", missed the map, and built a second engine whose
    // catalog predated the first engine's DDL. CREATE TABLE on
    // connection A followed by CREATE INDEX on connection B then
    // failed with "not found: table: …". The key is now the canonical
    // parent directory + file name — independent of file existence.
    let rel = format!("compat-rel-{}.db", std::process::id());
    let _ = std::fs::remove_file(&rel);
    let cpath = CString::new(rel.clone()).unwrap();

    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;
    unsafe {
        assert_eq!(
            sqlite3_open_v2(cpath.as_ptr(), &mut a, flags, ptr::null()),
            SQLITE_OK
        );
        // B opens AFTER the first open created the file on disk — the
        // exact pool timeline that used to split the engines.
        assert_eq!(
            sqlite3_open_v2(cpath.as_ptr(), &mut b, flags, ptr::null()),
            SQLITE_OK
        );
    }
    let (a, b) = (Db(a), Db(b));

    // The exact failing sequence from the migration log: DDL through A,
    // then DDL through B (a pooled statement may land on either).
    exec(
        &a,
        "CREATE TABLE \"user\" (id INTEGER PRIMARY KEY, status TEXT)",
    );
    exec(
        &b,
        "CREATE INDEX \"User_status_idx\" ON \"user\" (\"status\")",
    );
    exec(&a, "INSERT INTO \"user\" VALUES (1, 'active')");
    let (mut q, _) = prepare(&b, "SELECT COUNT(*) FROM \"user\"");
    let rows = step_all_text(&mut q);
    assert_eq!(
        rows[0][0], "1",
        "committed DDL + rows visible across connections on a fresh database"
    );

    // "./<name>" and "<name>" must resolve to the SAME engine: the key
    // unifies them through the canonicalized parent directory.
    let dotted = format!("./{rel}");
    let cdotted = CString::new(dotted).unwrap();
    let mut c: *mut compat::sqlite3 = ptr::null_mut();
    unsafe {
        assert_eq!(
            sqlite3_open_v2(cdotted.as_ptr(), &mut c, flags, ptr::null()),
            SQLITE_OK
        );
    }
    let c = Db(c);
    let (mut q, _) = prepare(&c, "SELECT COUNT(*) FROM \"user\"");
    let rows = step_all_text(&mut q);
    assert_eq!(
        rows[0][0], "1",
        "\"./name\" shares the engine with \"name\""
    );

    let _ = std::fs::remove_file(&rel);
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
        assert_eq!(
            sqlite3_open_v2(cpath.as_ptr(), &mut a, flags, ptr::null()),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_open_v2(cpath.as_ptr(), &mut b, flags, ptr::null()),
            SQLITE_OK
        );
        // Short timeout on B so the test is fast; A holds a tx.
        sqlite3_busy_timeout(b, 150);
    }
    let (a, b) = (Db(a), Db(b));
    exec(&a, "CREATE TABLE t (x INTEGER)");
    exec(&a, "INSERT INTO t VALUES (1)");

    exec(&a, "BEGIN");
    exec(&a, "UPDATE t SET x = 2");

    // B's BEGIN hits BUSY (timeout 150 ms).
    let (stmt, _) = prepare(&b, "BEGIN");
    let rc = unsafe { sqlite3_step(stmt.0) };
    assert_eq!(rc, SQLITE_BUSY, "SQLITE_BUSY while A holds the transaction");

    // A commits; B retries and succeeds (a successful BEGIN/COMMIT step
    // returns SQLITE_DONE — SQLITE_OK never comes out of sqlite3_step).
    exec(&a, "COMMIT");
    let (stmt, _) = prepare(&b, "BEGIN");
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
        assert_eq!(
            sqlite3_open_v2(uri.as_ptr(), &mut a, flags, ptr::null()),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_open_v2(uri.as_ptr(), &mut b, flags, ptr::null()),
            SQLITE_OK
        );
    }
    let (a, b) = (Db(a), Db(b));
    exec(&a, "CREATE TABLE t (x INTEGER)");
    exec(&a, "INSERT INTO t VALUES (1)");
    let (mut q, _) = prepare(
        &b,
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='t'",
    );
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
    let (stmt, _) = prepare(&db, "PRAGMA foreign_keys = ON");
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
    assert!((512..=65536).contains(&sz), "page_size in range: {}", sz);
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
    exec(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
    );
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };

    let (st, _) = prepare(&db, "UPDATE t SET v = 'a' WHERE id = 2");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_UNIQUE, "extended UNIQUE code");
    // errmsg must be byte-exact (no engine prefix) — sqlx pattern-matches it.
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(msg, "UNIQUE constraint failed: t.v");
    // errcode persists until reset (SQLite semantics).
    assert_eq!(unsafe { sqlite3_errcode(db.0) }, SQLITE_CONSTRAINT_UNIQUE);
    let rc2 = unsafe { sqlite3_reset(st.0) };
    assert_eq!(rc2, SQLITE_CONSTRAINT_UNIQUE, "reset re-reports the error");
}

#[test]
fn abi_update_atomic_abort_keeps_table_unchanged() {
    let db = open_memory();
    exec(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
    );
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");

    // Multi-row UPDATE where row 3 violates: SQLite aborts the whole
    // statement — rows 1-2 must NOT be half-updated.
    // First shift every value (unique — succeeds), then attempt a bulk
    // collapse to one value: row 2 conflicts, the statement aborts, and
    // NO row keeps the half-applied value.
    let (st, _) = prepare(&db, "UPDATE t SET v = 'x' || id");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_DONE, "x1/x2/x3 are distinct");
    let (st2, _) = prepare(&db, "UPDATE t SET v = 'same'");
    let rc2 = unsafe { sqlite3_step(st2.0) };
    assert_eq!(
        rc2, SQLITE_CONSTRAINT_UNIQUE,
        "all rows collapse onto 'same'"
    );
    // Table unchanged after the abort.
    let (st3, _) = prepare(&db, "SELECT COUNT(*) FROM t WHERE v IN ('x1','x2','x3')");
    assert_eq!(unsafe { sqlite3_step(st3.0) }, SQLITE_ROW);
    let n = unsafe { sqlite3_column_int64(st3.0, 0) };
    assert_eq!(n, 3, "statement aborted atomically — no partial updates");
}

#[test]
fn abi_update_or_ignore_via_step_and_changes() {
    let db = open_memory();
    exec(
        &db,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
    );
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')");

    let (st, _) = prepare(&db, "UPDATE OR IGNORE t SET v = 'a'");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_DONE, "OR IGNORE never errors on conflicts");
    // Only row 1 keeps 'a'... rows 2,3 skip; changes() = 1 (SQLite counts
    // only applied rows).
    let changes = unsafe { sqlite3_changes(db.0) };
    assert_eq!(changes, 1, "changes() counts only applied rows");

    let (st2, _) = prepare(&db, "SELECT COUNT(*) FROM t WHERE v = 'a'");
    assert_eq!(unsafe { sqlite3_step(st2.0) }, SQLITE_ROW);
    let n = unsafe { sqlite3_column_int64(st2.0, 0) };
    assert_eq!(n, 1, "skipped rows keep their old values");
}

#[test]
fn abi_update_rowid_move_via_step() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    let (st, _) = prepare(&db, "UPDATE t SET id = 10 WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_DONE);

    let (mut st2, _) = prepare(&db, "SELECT id, v FROM t ORDER BY id");
    let rows = step_all_text(&mut st2);
    assert_eq!(
        rows,
        vec![
            vec!["2".to_string(), "b".to_string()],
            vec!["10".to_string(), "a".to_string()]
        ]
    );

    // Moving to a taken rowid: SQLITE_CONSTRAINT with t.id in the message.
    let (st3, _) = prepare(&db, "UPDATE t SET id = 2 WHERE id = 10");
    let rc = unsafe { sqlite3_step(st3.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_UNIQUE);
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(msg, "UNIQUE constraint failed: t.id");
}

#[test]
fn abi_update_null_to_rowid_alias_is_mismatch() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&db, "INSERT INTO t VALUES (1, 'a')");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };
    let (st, _) = prepare(&db, "UPDATE t SET id = NULL WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_MISMATCH, "SQLite reports SQLITE_MISMATCH");
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(msg, "datatype mismatch");
}

#[test]
fn abi_update_fk_violation_extended_code() {
    let db = open_memory();
    exec(&db, "PRAGMA foreign_keys = ON");
    exec(&db, "CREATE TABLE p (id INTEGER PRIMARY KEY)");
    exec(
        &db,
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INT REFERENCES p(id))",
    );
    exec(&db, "INSERT INTO p VALUES (1)");
    exec(&db, "INSERT INTO c VALUES (1, 1)");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };
    let (st, _) = prepare(&db, "UPDATE c SET pid = 99 WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_CONSTRAINT_FOREIGNKEY, "extended FK code");
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(msg, "FOREIGN KEY constraint failed");
}

#[test]
fn abi_update_collated_unique_nocase() {
    let db = open_memory();
    exec(
        &db,
        "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE UNIQUE)",
    );
    exec(&db, "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta')");

    unsafe { sqlite3_extended_result_codes(db.0, 1) };
    let (st, _) = prepare(&db, "UPDATE s SET tag = 'ALPHA' WHERE id = 2");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(
        rc, SQLITE_CONSTRAINT_UNIQUE,
        "NOCASE folds 'ALPHA' onto 'Alpha'"
    );
    let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db.0)) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(msg, "UNIQUE constraint failed: s.tag");
}

#[test]
fn abi_update_returning_rows_and_changes() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&db, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");

    let (mut st, _) = prepare(&db, "UPDATE t SET v = v || '!' RETURNING id, v");
    // Column names available at prepare time (SQLite reports them then).
    let name0 = unsafe { CStr::from_ptr(sqlite3_column_name(st.0, 0)) }
        .to_string_lossy()
        .into_owned();
    let name1 = unsafe { CStr::from_ptr(sqlite3_column_name(st.0, 1)) }
        .to_string_lossy()
        .into_owned();
    assert_eq!((name0.as_str(), name1.as_str()), ("id", "v"));
    let rows = step_all_text(&mut st);
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "a!".to_string()],
            vec!["2".to_string(), "b!".to_string()]
        ]
    );
    assert_eq!(unsafe { sqlite3_changes(db.0) }, 2);
}

/// sqlx's in-memory POOL shape: it opens `file:sqlx-in-memory-N` with the
/// SQLITE_OPEN_MEMORY + SHAREDCACHE flags and NO `mode=memory` query
/// parameter. SQLite's openDatabase treats the MEMORY flag as forcing a
/// memdb (isMemdb includes the flag) whose shared-cache key is the raw
/// filename string — the compat layer must do the same, or sqlx pools
/// silently create on-disk files named "sqlx-in-memory-N" in the CWD.
/// Regression test: https://github.com/iamleson98/rust-sql (SQLITE_OPEN_MEMORY).
#[test]
fn abi_sqlx_in_memory_pool_flag_semantics() {
    const SQLITE_OPEN_URI: c_int = 0x40;
    const SQLITE_OPEN_NOMUTEX: c_int = 0x8000;
    const SQLITE_OPEN_SHAREDCACHE: c_int = 0x20000;
    let sqlx_flags = SQLITE_OPEN_URI
        | SQLITE_OPEN_READWRITE
        | SQLITE_OPEN_CREATE
        | SQLITE_OPEN_MEMORY
        | SQLITE_OPEN_NOMUTEX
        | SQLITE_OPEN_SHAREDCACHE;

    let open_sqlx_memory = |name: &str| -> Db {
        let mut db: *mut compat::sqlite3 = ptr::null_mut();
        let cname = CString::new(name).unwrap();
        let rc = unsafe { sqlite3_open_v2(cname.as_ptr(), &mut db, sqlx_flags, ptr::null()) };
        assert_eq!(rc, SQLITE_OK, "open_v2({name}) failed");
        unsafe { sqlite3_extended_result_codes(db, 1) };
        Db(db)
    };

    // Two connections to the same name share ONE engine (sqlx pool shape).
    let a = open_sqlx_memory("file:sqlx-in-memory-0");
    exec(&a, "CREATE TABLE pool (id INTEGER PRIMARY KEY, v TEXT)");
    exec(&a, "INSERT INTO pool VALUES (1, 'shared')");

    let b = open_sqlx_memory("file:sqlx-in-memory-0");
    let (mut st, _) = prepare(&b, "SELECT v FROM pool WHERE id = 1");
    let rows = step_all_text(&mut st);
    assert_eq!(
        rows,
        vec![vec!["shared".to_string()]],
        "pool connections must share one in-memory engine"
    );

    // A different name is a DIFFERENT database (per-pool isolation): the
    // table created above must not exist there.
    let c = open_sqlx_memory("file:sqlx-in-memory-1");
    let (mut st, _) = prepare(&c, "SELECT COUNT(*) FROM sqlite_master WHERE name = 'pool'");
    let rows = step_all_text(&mut st);
    assert_eq!(
        rows,
        vec![vec!["0".to_string()]],
        "distinct memory names must be isolated"
    );

    // No on-disk file may be created for the MEMORY-flag path.
    for name in ["sqlx-in-memory-0", "sqlx-in-memory-1"] {
        assert!(
            !std::path::Path::new(name).exists(),
            "MEMORY-flag open must not create a disk file ({name})"
        );
    }
}

/// sqlite3_column_blob must return a pointer into STATEMENT-OWNED storage:
/// the blob Value is cloned out of the engine, so a pointer into the
/// clone dangles the moment it drops. sqlx-sqlite reads blob columns via
/// column_blob + column_bytes exactly this way. Regression test for the
/// use-after-free that made blobs read back as heap garbage.
#[test]
fn abi_column_blob_survives_the_value_clone() {
    let db = open_memory();
    exec(&db, "CREATE TABLE b (id INTEGER PRIMARY KEY, v BLOB)");
    exec(&db, "INSERT INTO b VALUES (1, X'0A0B0C'), (2, x'deadbeef')");

    let (mut st, _) = prepare(&db, "SELECT v FROM b WHERE id = 1");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW);
    let p = unsafe { sqlite3_column_blob(st.0, 0) };
    assert!(
        !p.is_null(),
        "column_blob must return a pointer for non-empty blobs"
    );
    let len = unsafe { sqlite3_column_bytes(st.0, 0) };
    assert_eq!(len, 3);
    let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, len as usize).to_vec() };
    assert_eq!(
        bytes,
        vec![0x0A, 0x0B, 0x0C],
        "blob bytes must round-trip (no dangling clone)"
    );
}

/// Column NAMES for `SELECT * FROM t WHERE …` must be unqualified ("id"),
/// not table-qualified ("t.id") — sea-orm's FromRow looks columns up by
/// their bare name. Probe used to diagnose name shape.
#[test]
fn abi_probe_select_star_column_names() {
    let db = open_memory();
    exec(
        &db,
        "CREATE TABLE scheduled_job (id TEXT PRIMARY KEY, enabled BOOLEAN, next_run_at TEXT)",
    );
    exec(
        &db,
        "INSERT INTO scheduled_job VALUES ('x', 1, '2026-09-01')",
    );
    let (mut st, _) = prepare(
        &db,
        "SELECT * FROM scheduled_job WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= '2026-09-02'",
    );
    let n = unsafe { sqlite3_column_count(st.0) };
    for i in 0..n {
        let name = unsafe { cstr(sqlite3_column_name(st.0, i)) };
        eprintln!("column[{i}] = {name:?}");
    }
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW);
    let v = unsafe { cstr(sqlite3_column_text(st.0, 0) as *const c_char) };
    assert_eq!(v, "x");
    assert_eq!(n, 3);
    assert_eq!(unsafe { cstr(sqlite3_column_name(st.0, 0)) }, "id");
    assert_eq!(unsafe { cstr(sqlite3_column_name(st.0, 1)) }, "enabled");
    assert_eq!(unsafe { cstr(sqlite3_column_name(st.0, 2)) }, "next_run_at");
}

/// sqlx's ROW-reading path: sqlite3_column_value → Box<Value> handle →
/// value_type / value_bytes / value_blob / value_text. Probe the full
/// chain on TEXT + BLOB columns exactly as sqlx-sqlite 0.8 does.
#[test]
fn abi_probe_value_handle_chain() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (id TEXT PRIMARY KEY, v BLOB)");
    exec(
        &db,
        "INSERT INTO t VALUES ('2f230c53-c600-4700-a7b2-2d661de7d694', X'0A0B0C')",
    );

    let (mut st, _) = prepare(&db, "SELECT id, v FROM t");
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW);

    // sqlx: SqliteValue::new(sqlite3_column_value(stmt, i)) — dups the value.
    let vh0 = unsafe { sqlite3_column_value(st.0, 0) };
    assert!(
        !vh0.is_null(),
        "column_value(0) must not be NULL for a live row"
    );
    let dup0 = unsafe { sqlite3_value_dup(vh0) };
    assert!(!dup0.is_null());

    let ty = unsafe { sqlite3_value_type(dup0) };
    let len = unsafe { sqlite3_value_bytes(dup0) };
    let txt = unsafe { sqlite3_value_text(dup0) };
    eprintln!(
        "id col: type={ty} bytes={len} text_ptr_null={}",
        txt.is_null()
    );
    if !txt.is_null() {
        let s = unsafe { CStr::from_ptr(txt as *const c_char) }
            .to_string_lossy()
            .into_owned();
        eprintln!("id text = {s:?}");
    }
    unsafe { sqlite3_value_free(dup0) };

    let vh1 = unsafe { sqlite3_column_value(st.0, 1) };
    let dup1 = unsafe { sqlite3_value_dup(vh1) };
    let len1 = unsafe { sqlite3_value_bytes(dup1) };
    let blob = unsafe { sqlite3_value_blob(dup1) };
    eprintln!("blob col: bytes={len1} ptr_null={}", blob.is_null());
    unsafe { sqlite3_value_free(dup1) };

    assert_eq!(ty, 3 /* SQLITE_TEXT */);
    assert_eq!(len, 36);
    assert_eq!(len1, 3);
}

/// Replicate sea-orm's exact SELECT shape: qualified column list, bound
/// parameters, a UUID stored as a 16-byte BLOB (sqlx-sqlite binds Uuid
/// as BLOB(16)). Read the row through the column_value → value_dup chain
/// exactly like SqliteRow::current + SqliteValue::new.
#[test]
fn abi_probe_seaorm_select_shape() {
    let db = open_memory();
    exec(&db, "CREATE TABLE \"scheduled_job\" (\"id\" blob NOT NULL PRIMARY KEY, \"job_type\" text NOT NULL UNIQUE, \"enabled\" boolean NOT NULL, \"interval_days\" smallint NOT NULL, \"at_hour\" smallint NOT NULL, \"at_minute\" smallint NOT NULL, \"next_run_at\" text, \"created_at\" text NOT NULL, \"updated_at\" text NOT NULL)");
    // UUID as 16-byte blob, like sqlx binds Uuid.
    exec(&db, "INSERT INTO \"scheduled_job\" VALUES (X'2F230C53C6004700A7B22D661DE7D694', 'due.job', 1, 1, 0, 30, '2026-09-01T18:00:00Z', '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')");

    let sql = "SELECT \"scheduled_job\".\"id\", \"scheduled_job\".\"job_type\", \"scheduled_job\".\"enabled\", \"scheduled_job\".\"interval_days\", \"scheduled_job\".\"at_hour\", \"scheduled_job\".\"at_minute\", \"scheduled_job\".\"next_run_at\", \"scheduled_job\".\"created_at\", \"scheduled_job\".\"updated_at\" FROM \"scheduled_job\" WHERE \"scheduled_job\".\"enabled\" = ? AND \"scheduled_job\".\"next_run_at\" IS NOT NULL AND \"scheduled_job\".\"next_run_at\" <= ?";
    let (mut st, _) = prepare(&db, sql);
    unsafe { sqlite3_bind_int64(st.0, 1, 1) };
    unsafe { sqlite3_bind_text64(st.0, 2, c"2026-09-02T00:00:00Z".as_ptr(), 20, None, 1) };

    let n = unsafe { sqlite3_column_count(st.0) };
    eprintln!("column_count at prepare = {n}");
    for i in 0..n {
        eprintln!("  name[{i}] = {:?}", unsafe {
            cstr(sqlite3_column_name(st.0, i))
        });
    }
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW, "expected a row, got rc={rc}");
    for i in 0..n {
        let vh = unsafe { sqlite3_column_value(st.0, i) };
        if vh.is_null() {
            eprintln!("  value[{i}] = NULL HANDLE (row shorter than column_count!)");
            continue;
        }
        let dup = unsafe { sqlite3_value_dup(vh) };
        let ty = unsafe { sqlite3_value_type(dup) };
        let len = unsafe { sqlite3_value_bytes(dup) };
        eprintln!("  value[{i}] type={ty} len={len}");
        unsafe { sqlite3_value_free(dup) };
    }
    assert_eq!(n, 9, "sea-orm selects 9 columns");
}

/// BARE `SELECT * FROM t` (no WHERE): the shape that made the sqlx worker
/// panic with a NULL sqlite3_column_value. Column_count vs row length.
#[test]
fn abi_probe_bare_select_star() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (a TEXT PRIMARY KEY, b INT, c REAL)");
    exec(&db, "INSERT INTO t VALUES ('x', 1, 2.5)");
    let (mut st, _) = prepare(&db, "SELECT * FROM t");
    let n = unsafe { sqlite3_column_count(st.0) };
    eprintln!("bare star column_count = {n}");
    for i in 0..n {
        eprintln!("  name[{i}] = {:?}", unsafe {
            cstr(sqlite3_column_name(st.0, i))
        });
    }
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW);
    for i in 0..n {
        let vh = unsafe { sqlite3_column_value(st.0, i) };
        eprintln!("  value[{i}] ptr_null={}", vh.is_null());
        if !vh.is_null() {
            let ty = unsafe { sqlite3_value_type(vh) };
            eprintln!("    type={ty}");
        }
    }
    assert_eq!(n, 3, "column_count must match the table's columns");
}

/// Engine-level probe: prepare → step → column_value for a bare
/// `SELECT * FROM t` — bypass the C ABI to see the statement state.
#[test]
fn probe_engine_level_bare_star() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a TEXT PRIMARY KEY, b INT, c REAL)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('x', 1, 2.5)", [])
        .unwrap();

    let mut st = db.prepare("SELECT * FROM t").unwrap();
    eprintln!("engine column_count at prepare = {}", st.column_count());
    let r = st.step().unwrap();
    eprintln!("engine step = {:?}", format!("{:?}", r));
    eprintln!("engine column_count after step = {}", st.column_count());
    eprintln!("engine current_row present = {}", st.row().is_some());
    eprintln!("engine column_value(0) = {:?}", st.column_value(0));
    eprintln!("engine column_text(0) = {:?}", st.column_text(0));
    assert!(matches!(r, rustqlite::StepResult::Row));
    assert!(
        st.column_value(0).is_some(),
        "engine statement must expose row values after a Row step"
    );
}

/// Full app flow: DDL with uuid_text columns, INSERT via bound params
/// (blob16 id like sqlx's Uuid), then the filtered SELECT with bound
/// params. Diagnose the "invalid length: expected 16 bytes, found N"
/// decode failure.
#[test]
fn abi_probe_app_flow_bind_insert_then_filtered_select() {
    let db = open_memory();
    exec(&db, "CREATE TABLE \"scheduled_job\" (\"id\" uuid_text NOT NULL PRIMARY KEY, \"job_type\" varchar(64) NOT NULL UNIQUE, \"enabled\" boolean NOT NULL, \"interval_days\" smallint NOT NULL, \"at_hour\" smallint NOT NULL, \"at_minute\" smallint NOT NULL, \"next_run_at\" text, \"created_at\" text NOT NULL, \"updated_at\" text NOT NULL)");

    // INSERT with binds, exactly like sqlx: Uuid → blob16, bool → int.
    let uuid_bytes: [u8; 16] = [
        0x2F, 0x23, 0x0C, 0x53, 0xC6, 0x00, 0x47, 0x00, 0xA7, 0xB2, 0x2D, 0x66, 0x1D, 0xE7, 0xD6,
        0x94,
    ];
    {
        let (mut st, _) = prepare(&db, "INSERT INTO \"scheduled_job\" (\"id\", \"job_type\", \"enabled\", \"interval_days\", \"at_hour\", \"at_minute\", \"next_run_at\", \"created_at\", \"updated_at\") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)");
        unsafe {
            sqlite3_bind_blob64(st.0, 1, uuid_bytes.as_ptr() as *const c_void, 16, None);
            sqlite3_bind_text64(st.0, 2, c"due.job".as_ptr(), 7, None, 1);
            sqlite3_bind_int64(st.0, 3, 1);
            sqlite3_bind_int64(st.0, 4, 1);
            sqlite3_bind_int64(st.0, 5, 0);
            sqlite3_bind_int64(st.0, 6, 30);
            sqlite3_bind_text64(st.0, 7, c"2026-09-01T18:00:00Z".as_ptr(), 20, None, 1);
            sqlite3_bind_text64(st.0, 8, c"2026-09-01T00:00:00Z".as_ptr(), 20, None, 1);
            sqlite3_bind_text64(st.0, 9, c"2026-09-01T00:00:00Z".as_ptr(), 20, None, 1);
        }
        let rc = unsafe { sqlite3_step(st.0) };
        assert_eq!(rc, SQLITE_DONE, "insert step");
    }

    // The app's filtered SELECT (sea-orm shape, explicit qualified cols).
    let sql = "SELECT \"scheduled_job\".\"id\", \"scheduled_job\".\"job_type\", \"scheduled_job\".\"enabled\", \"scheduled_job\".\"interval_days\", \"scheduled_job\".\"at_hour\", \"scheduled_job\".\"at_minute\", \"scheduled_job\".\"next_run_at\", \"scheduled_job\".\"created_at\", \"scheduled_job\".\"updated_at\" FROM \"scheduled_job\" WHERE \"scheduled_job\".\"enabled\" = 1 AND \"scheduled_job\".\"next_run_at\" IS NOT NULL AND \"scheduled_job\".\"next_run_at\" <= '2026-09-02T00:00:00Z'";
    let (mut st, _) = prepare(&db, sql);
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW, "expected the row");
    let n = unsafe { sqlite3_column_count(st.0) };
    for i in 0..n {
        let vh = unsafe { sqlite3_column_value(st.0, i) };
        assert!(!vh.is_null(), "value[{i}] handle must not be null");
        let ty = unsafe { sqlite3_value_type(vh) };
        let len = unsafe { sqlite3_value_bytes(vh) };
        let txt = unsafe { sqlite3_value_text(vh) };
        let text_repr = if txt.is_null() {
            "-".to_string()
        } else {
            let p = txt as *const c_char;
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        };
        eprintln!("appflow value[{i}] type={ty} len={len} text~{text_repr:?}");
    }
    assert_eq!(n, 9);
    // id must be a 16-byte blob.
    let vh0 = unsafe { sqlite3_column_value(st.0, 0) };
    assert_eq!(unsafe { sqlite3_value_type(vh0) }, 4 /*BLOB*/);
    assert_eq!(unsafe { sqlite3_value_bytes(vh0) }, 16, "uuid blob length");
}

/// COUNT(*) with a WHERE on a BOUND parameter — sea-orm's PaginatorTrait
/// shape. Diagnoses count_runs returning 0.
#[test]
fn abi_probe_count_where_bound_param() {
    let db = open_memory();
    exec(&db, "CREATE TABLE \"job_run\" (\"id\" blob NOT NULL PRIMARY KEY, \"job_type\" text NOT NULL, \"status\" text NOT NULL)");
    exec(&db, "INSERT INTO \"job_run\" VALUES (X'2F230C53C6004700A7B22D661DE7D694', 'osm.import', 'queued')");
    exec(&db, "INSERT INTO \"job_run\" VALUES (X'110C53C6004700A7B22D661DE7D694AA', 'osm.import', 'failed')");

    // Literal WHERE — sanity.
    let (mut st, _) = prepare(
        &db,
        "SELECT COUNT(*) FROM \"job_run\" WHERE \"job_run\".\"job_type\" = 'osm.import'",
    );
    let rc = unsafe { sqlite3_step(st.0) };
    assert_eq!(rc, SQLITE_ROW);
    let lit = unsafe { sqlite3_column_int64(st.0, 0) };
    eprintln!("count with literal = {lit}");

    // Bound param WHERE — the sea-orm shape.
    let (mut st2, _) = prepare(
        &db,
        "SELECT COUNT(*) FROM \"job_run\" WHERE \"job_run\".\"job_type\" = ?",
    );
    unsafe { sqlite3_bind_text64(st2.0, 1, c"osm.import".as_ptr(), 10, None, 1) };
    let rc = unsafe { sqlite3_step(st2.0) };
    assert_eq!(rc, SQLITE_ROW);
    let bound = unsafe { sqlite3_column_int64(st2.0, 0) };
    eprintln!("count with bound param = {bound}");

    // Also: filter on a BLOB pk with a bound blob (find_by_id shape).
    let (mut st3, _) = prepare(
        &db,
        "SELECT COUNT(*) FROM \"job_run\" WHERE \"job_run\".\"id\" = ?",
    );
    let uuid_arr: [u8; 16] = [
        0x2F, 0x23, 0x0C, 0x53, 0xC6, 0x00, 0x47, 0x00, 0xA7, 0xB2, 0x2D, 0x66, 0x1D, 0xE7, 0xD6,
        0x94,
    ];
    unsafe { sqlite3_bind_blob64(st3.0, 1, uuid_arr.as_ptr() as *const c_void, 16, None) };
    let rc = unsafe { sqlite3_step(st3.0) };
    assert_eq!(rc, SQLITE_ROW);
    let byid = unsafe { sqlite3_column_int64(st3.0, 0) };
    eprintln!("count by bound blob id = {byid}");

    assert_eq!(lit, 2);
    assert_eq!(bound, 2, "COUNT with bound param must match literals");
    assert_eq!(byid, 1);
}

/// Engine-API-level: COUNT with a bound TEXT param, and plain SELECT with
/// a bound TEXT param — isolate where bound params stop matching.
#[test]
fn probe_engine_count_bound_param() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a TEXT PRIMARY KEY, b INT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('x', 1)", []).unwrap();
    db.execute("INSERT INTO t VALUES ('y', 2)", []).unwrap();

    // query() with params (api path).
    let n = db
        .query(
            "SELECT COUNT(*) FROM t WHERE a = ?",
            [rustqlite::Value::Text("x".into())],
        )
        .unwrap();
    eprintln!("api query count = {:?}", n);

    // prepared statement + bind.
    let mut st = db.prepare("SELECT COUNT(*) FROM t WHERE a = ?").unwrap();
    st.bind(1, rustqlite::Value::Text("x".into())).unwrap();
    let r = st.step().unwrap();
    eprintln!("prepared step = {r:?}, col0 = {:?}", st.column_value(0));
    assert!(matches!(r, rustqlite::StepResult::Row));
    assert_eq!(st.column_value(0).map(|v| v.as_integer()), Some(1));

    // plain SELECT with bound param.
    let mut st2 = db.prepare("SELECT b FROM t WHERE a = ?").unwrap();
    st2.bind(1, rustqlite::Value::Text("y".into())).unwrap();
    let r2 = st2.step().unwrap();
    eprintln!("select step = {r2:?}, col0 = {:?}", st2.column_value(0));
    assert!(matches!(r2, rustqlite::StepResult::Row));
    assert_eq!(st2.column_value(0).map(|v| v.as_integer()), Some(2));
}

/// Replicate the COMPAT's exact flow: prepare → PRE-EXECUTE (no params,
/// for column names) → reset → bind → step. If the pre-execute poisons
/// the re-execution, this fails like the compat does.
#[test]
fn probe_engine_preexecute_then_bind() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a TEXT PRIMARY KEY, b INT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES ('x', 1)", []).unwrap();
    db.execute("INSERT INTO t VALUES ('y', 2)", []).unwrap();

    let mut st = db.prepare("SELECT COUNT(*) FROM t WHERE a = ?").unwrap();
    // pre-execute with NO params (what the compat does at prepare time)
    let _ = st.step();
    eprintln!("pre-exec count (params unbound) = {:?}", st.column_value(0));
    st.reset();
    // now bind + step (what sqlx does after prepare)
    st.bind(1, rustqlite::Value::Text("x".into())).unwrap();
    let r = st.step().unwrap();
    eprintln!("after bind+step = {r:?}, col0 = {:?}", st.column_value(0));
    assert!(matches!(r, rustqlite::StepResult::Row));
    assert_eq!(
        st.column_value(0).map(|v| v.as_integer()),
        Some(1),
        "pre-execute must not poison the re-run"
    );
}

/// DRIVER-COVERED statement (no pre-execute at prepare) + bound TEXT
/// param: `SELECT a FROM t WHERE a = ?` — FilteredScanDriver fusion.
#[test]
fn abi_probe_driver_covered_bound_text_param() {
    let db = open_memory();
    exec(&db, "CREATE TABLE t (a TEXT PRIMARY KEY, b INT)");
    exec(&db, "INSERT INTO t VALUES ('x', 1), ('y', 2)");

    let (mut st, _) = prepare(&db, "SELECT a, b FROM t WHERE a = ?");
    // column_count at prepare > 0 → driver-covered, NO pre-execute.
    let n = unsafe { sqlite3_column_count(st.0) };
    eprintln!("driver-covered column_count at prepare = {n}");
    unsafe { sqlite3_bind_text64(st.0, 1, c"x".as_ptr(), 1, None, 1) };
    let rc = unsafe { sqlite3_step(st.0) };
    eprintln!("step rc = {rc}");
    if rc == 100 {
        let v = unsafe { sqlite3_value_text(sqlite3_column_value(st.0, 0)) };
        eprintln!(
            "row a = {:?}",
            if v.is_null() {
                "-".into()
            } else {
                unsafe { CStr::from_ptr(v as *const c_char) }
                    .to_string_lossy()
                    .into_owned()
            }
        );
    }
    assert_eq!(rc, 100 /* ROW */);
}

/// Exact replication of the failing app test: job_run table, two rows
/// inserted via BOUND params (blob16 ids, TEXT columns), then
/// COUNT(*) WHERE job_type = ? with a bound param.
#[test]
fn abi_probe_job_run_count_flow() {
    let db = open_memory();
    exec(&db, "CREATE TABLE job_run (id TEXT PRIMARY KEY, job_type TEXT NOT NULL, status TEXT NOT NULL, detail TEXT, error TEXT, started_at TEXT, finished_at TEXT, created_at TEXT NOT NULL)");

    for (id, status) in [
        (
            [
                0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF, 0x00,
            ],
            "failed",
        ),
        (
            [
                0x21u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF, 0x01,
            ],
            "queued",
        ),
    ] {
        let (mut st, _) = prepare(&db, "INSERT INTO job_run (id, job_type, status, detail, error, started_at, finished_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)");
        unsafe {
            sqlite3_bind_blob64(st.0, 1, id.as_ptr() as *const c_void, 16, None);
            sqlite3_bind_text64(st.0, 2, c"osm.import".as_ptr(), 10, None, 1);
            sqlite3_bind_text64(
                st.0,
                3,
                c"failed".as_ptr().add(0),
                status.len() as u64,
                None,
                1,
            );
            sqlite3_bind_null(st.0, 4);
            sqlite3_bind_null(st.0, 5);
            sqlite3_bind_null(st.0, 6);
            sqlite3_bind_null(st.0, 7);
            sqlite3_bind_text64(st.0, 8, c"2026-09-01T18:00:00Z".as_ptr(), 20, None, 1);
        }
        let rc = unsafe { sqlite3_step(st.0) };
        assert_eq!(rc, 101 /* DONE */, "insert must succeed");
        unsafe { sqlite3_reset(st.0) };
    }

    // COUNT with bound param.
    let (mut st2, _) = prepare(&db, "SELECT COUNT(*) FROM job_run WHERE job_type = ?");
    unsafe { sqlite3_bind_text64(st2.0, 1, c"osm.import".as_ptr(), 10, None, 1) };
    let rc = unsafe { sqlite3_step(st2.0) };
    assert_eq!(rc, 100 /* ROW */);
    let n = unsafe { sqlite3_column_int64(st2.0, 0) };
    eprintln!("job_run count with bound param = {n}");
    assert_eq!(n, 2);
}

/// sea-orm's PaginatorTrait::count SQL shape:
/// SELECT COUNT(*) AS num_items FROM (subquery with bound param) AS sub_query
#[test]
fn abi_probe_seaorm_count_subquery_shape() {
    let db = open_memory();
    exec(&db, "CREATE TABLE job_run (id TEXT PRIMARY KEY, job_type TEXT NOT NULL, status TEXT NOT NULL, detail TEXT, error TEXT, started_at TEXT, finished_at TEXT, created_at TEXT NOT NULL)");
    exec(&db, "INSERT INTO job_run (id, job_type, status, created_at) VALUES ('a', 'osm.import', 'failed', '2026-09-01T18:00:00Z')");
    exec(&db, "INSERT INTO job_run (id, job_type, status, created_at) VALUES ('b', 'osm.import', 'queued', '2026-09-01T19:00:00Z')");

    let sql = "SELECT COUNT(*) AS \"num_items\" FROM (SELECT \"job_run\".\"id\" AS \"id\", \"job_run\".\"job_type\" AS \"job_type\", \"job_run\".\"status\" AS \"status\", \"job_run\".\"detail\" AS \"detail\", \"job_run\".\"error\" AS \"error\", \"job_run\".\"started_at\" AS \"started_at\", \"job_run\".\"finished_at\" AS \"finished_at\", \"job_run\".\"created_at\" AS \"created_at\" FROM \"job_run\" WHERE \"job_run\".\"job_type\" = ?) AS \"sub_query\"";
    let (mut st, _) = prepare(&db, sql);
    let n_cols = unsafe { sqlite3_column_count(st.0) };
    let name0 = unsafe { cstr(sqlite3_column_name(st.0, 0)) };
    eprintln!("count-subquery: n_cols={n_cols} name0={name0:?}");
    unsafe { sqlite3_bind_text64(st.0, 1, c"osm.import".as_ptr(), 10, None, 1) };
    let rc = unsafe { sqlite3_step(st.0) };
    eprintln!("step rc = {rc}");
    let count = if rc == 100 {
        unsafe { sqlite3_column_int64(st.0, 0) }
    } else {
        -1
    };
    eprintln!("count via subquery = {count}");
    assert_eq!(rc, 100);
    assert_eq!(count, 2, "subquery count must see the rows");
    assert_eq!(name0, "num_items");
}

/// ENGINE-level: the sea-orm count subquery shape with a bound param.
#[test]
fn probe_engine_count_subquery_param() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE job_run (id TEXT PRIMARY KEY, job_type TEXT NOT NULL, status TEXT NOT NULL, detail TEXT, error TEXT, started_at TEXT, finished_at TEXT, created_at TEXT NOT NULL)", []).unwrap();
    db.execute("INSERT INTO job_run (id, job_type, status, created_at) VALUES ('a', 'osm.import', 'failed', '2026-09-01T18:00:00Z')", []).unwrap();
    db.execute("INSERT INTO job_run (id, job_type, status, created_at) VALUES ('b', 'osm.import', 'queued', '2026-09-01T19:00:00Z')", []).unwrap();

    let sql = "SELECT COUNT(*) AS num_items FROM (SELECT job_run.id AS id, job_run.job_type AS job_type, job_run.status AS status, job_run.detail AS detail, job_run.error AS error, job_run.started_at AS started_at, job_run.finished_at AS finished_at, job_run.created_at AS created_at FROM job_run WHERE job_run.job_type = ?) AS sub_query";
    // api path with params
    let rows = db
        .query(sql, [rustqlite::Value::Text("osm.import".into())])
        .unwrap();
    eprintln!("engine api subquery count = {:?}", rows);

    // prepared + bind
    let mut st = db.prepare(sql).unwrap();
    st.bind(1, rustqlite::Value::Text("osm.import".into()))
        .unwrap();
    let r = st.step().unwrap();
    eprintln!("prepared = {r:?} col0 = {:?}", st.column_value(0));
    assert!(matches!(r, rustqlite::StepResult::Row));
    assert_eq!(
        st.column_value(0).map(|v| v.as_integer()),
        Some(2),
        "engine subquery count with bound param"
    );
}

/// ENGINE-level FULL compat flow replication: prepare → pre-execute (no
/// params) → reset → bind → step, for the SUBQUERY count shape.
#[test]
fn probe_engine_preexec_subquery_flow() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE job_run (id TEXT PRIMARY KEY, job_type TEXT NOT NULL, status TEXT NOT NULL, detail TEXT, error TEXT, started_at TEXT, finished_at TEXT, created_at TEXT NOT NULL)", []).unwrap();
    db.execute("INSERT INTO job_run (id, job_type, status, created_at) VALUES ('a', 'osm.import', 'failed', '2026-09-01T18:00:00Z')", []).unwrap();
    db.execute("INSERT INTO job_run (id, job_type, status, created_at) VALUES ('b', 'osm.import', 'queued', '2026-09-01T19:00:00Z')", []).unwrap();

    let sql = "SELECT COUNT(*) AS num_items FROM (SELECT job_run.id AS id FROM job_run WHERE job_run.job_type = ?) AS sub_query";
    let mut st = db.prepare(sql).unwrap();
    // 1. pre-execute WITHOUT params (what the compat does at prepare)
    let _ = st.step();
    eprintln!("pre-exec (no param) = {:?}", st.column_value(0));
    // 2. reset
    st.reset();
    // 3. bind
    st.bind(1, rustqlite::Value::Text("osm.import".into()))
        .unwrap();
    // 4. step
    let r = st.step().unwrap();
    eprintln!("after bind+step = {r:?} col0 = {:?}", st.column_value(0));
    assert!(matches!(r, rustqlite::StepResult::Row));
    assert_eq!(
        st.column_value(0).map(|v| v.as_integer()),
        Some(2),
        "reset+bind must re-execute with params"
    );
}

/// The worker's dequeue shape: DELETE with IN(subquery containing a bound
/// param) + RETURNING. Proves the DELETE-RETURNING path + param slots.
#[test]
fn abi_probe_delete_in_subquery_returning() {
    let db = open_memory();
    exec(&db, "CREATE TABLE jobs (id TEXT PRIMARY KEY, job_type TEXT NOT NULL, payload TEXT NOT NULL, attempts INTEGER NOT NULL, available_at TEXT NOT NULL, created_at TEXT NOT NULL)");
    exec(&db, "INSERT INTO jobs (id, job_type, payload, attempts, available_at, created_at) VALUES ('j1', 'test.noop', '{}', 0, '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z')");

    let sql = "DELETE FROM \"jobs\" WHERE \"id\" IN (SELECT \"id\" FROM \"jobs\" WHERE \"available_at\" <= ? ORDER BY \"available_at\" ASC LIMIT 1) RETURNING \"id\", \"job_type\", \"payload\", \"attempts\"";
    let (mut st, _) = prepare(&db, sql);
    // param must be discovered (1 slot) and bindable
    unsafe { sqlite3_bind_text64(st.0, 1, c"2026-09-02T00:00:00Z".as_ptr(), 20, None, 1) };
    let rc = unsafe { sqlite3_step(st.0) };
    eprintln!("delete-returning step rc = {rc}");
    if rc == 100 {
        let id = unsafe { sqlite3_value_text(sqlite3_column_value(st.0, 0)) };
        let jt = unsafe { sqlite3_value_text(sqlite3_column_value(st.0, 1)) };
        eprintln!(
            "returned id={:?} job_type={:?}",
            if id.is_null() {
                "-".into()
            } else {
                unsafe { CStr::from_ptr(id as *const c_char) }
                    .to_string_lossy()
                    .into_owned()
            },
            if jt.is_null() {
                "-".into()
            } else {
                unsafe { CStr::from_ptr(jt as *const c_char) }
                    .to_string_lossy()
                    .into_owned()
            }
        );
    }
    assert!(rc == 100, "RETURNING must yield the deleted row");
}
