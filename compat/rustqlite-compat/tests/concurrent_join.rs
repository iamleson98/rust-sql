//! BEGIN CONCURRENT through the C ABI — the multi-writer regime as a
//! stock C application drives it (sqlite3_open/exec/prepare/step only):
//!
//! - IMPLICIT JOIN: a plain autocommit DML statement on connection B
//!   while connection A holds a BEGIN CONCURRENT transaction JOINS the
//!   regime as a one-statement concurrent transaction (page shadows,
//!   row-level first-committer-wins, group-commit fsync) — no BUSY, no
//!   wait, durable immediately. (Real SQLite's begin_concurrent branch
//!   returns SQLITE_BUSY here; the join is the engine's surplus.)
//! - NO DIRY READS: B's SELECT during A's regime serves the COMMITTED
//!   view — never A's uncommitted shadows. A reads its own writes.
//! - FIRST-COMMITTER-WINS: the joiner commits the hot row first; A's
//!   COMMIT surfaces SQLITE_BUSY_SNAPSHOT (517) — retriable, the
//!   transaction already rolled back engine-side, autocommit restored.
//! - Regime discipline: plain BEGIN / DDL still wait (BUSY at timeout
//!   0) while the regime is active; two concurrent owners interleave
//!   (TRUE multi-writer); an abandoned concurrent transaction rolls
//!   back at close.

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::ptr;

extern crate sqlite3 as compat;

extern "C" {
    fn sqlite3_open(filename: *const c_char, ppdb: *mut *mut compat::sqlite3) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut compat::sqlite3,
        sql: *const c_char,
        cb: *mut c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut compat::sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        stmt: *mut *mut compat::sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut compat::sqlite3_stmt, idx: c_int, v: i64) -> c_int;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_get_autocommit(db: *mut compat::sqlite3) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_BUSY: c_int = 5;
/// SQLITE_BUSY_SNAPSHOT — the begin_concurrent branch's retriable
/// commit-conflict code.
const SQLITE_BUSY_SNAPSHOT: c_int = 517;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn exec(db: *mut compat::sqlite3, sql: &str) -> c_int {
    let sql_c = cstr(sql);
    let mut errmsg: *mut c_char = ptr::null_mut();
    unsafe {
        sqlite3_exec(
            db,
            sql_c.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut errmsg,
        )
    }
}

fn err_of(db: *mut compat::sqlite3) -> String {
    unsafe {
        CStr::from_ptr(sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned()
    }
}

/// First column of the first row of a query, as i64 (the count(*) probe
/// shape). Finalizes the statement.
fn query_int(db: *mut compat::sqlite3, sql: &str) -> i64 {
    let sql_c = cstr(sql);
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe {
        sqlite3_prepare_v2(
            db,
            sql_c.as_ptr(),
            -1,
            &mut stmt as *mut *mut compat::sqlite3_stmt,
            &mut tail as *mut *const c_char,
        )
    };
    if rc != SQLITE_OK {
        panic!("prepare failed ({rc}): {} — sql: {sql}", err_of(db));
    }
    let step = unsafe { sqlite3_step(stmt) };
    assert_eq!(
        step,
        SQLITE_ROW,
        "expected a row for: {sql} ({})",
        err_of(db)
    );
    let v = unsafe { sqlite3_column_int64(stmt, 0) };
    let fin = unsafe { sqlite3_finalize(stmt) };
    assert_eq!(fin, SQLITE_OK, "finalize: {}", err_of(db));
    v
}

fn open_db(path: &str) -> *mut compat::sqlite3 {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(path);
    let rc = unsafe { sqlite3_open(name.as_ptr(), &mut db as *mut *mut compat::sqlite3) };
    assert_eq!(rc, SQLITE_OK, "open {path}");
    db
}

/// Open + WAL mode. BEGIN CONCURRENT requires journal_mode=WAL exactly
/// like SQLite's begin_concurrent branch — a fresh file starts in DELETE
/// mode, so every concurrent-regime test arms WAL first (the standard
/// practice for any C application opting into the branch).
fn open_wal_db(path: &str) -> *mut compat::sqlite3 {
    let db = open_db(path);
    assert_eq!(
        exec(db, "PRAGMA journal_mode = WAL"),
        SQLITE_OK,
        "journal_mode=WAL: {}",
        err_of(db)
    );
    db
}

fn temp_db(tag: &str) -> String {
    let pid = std::process::id();
    format!("/tmp/compat_cw_join_{tag}_{pid}.db")
}

/// Baseline sanity: the regime exists through the C ABI at all — BEGIN
/// CONCURRENT, one INSERT, COMMIT, durable across a reopen.
#[test]
fn concurrent_txn_commit_durable_through_c_abi() {
    let path = temp_db("baseline");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"),
        SQLITE_OK
    );
    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (1, 'a')"), SQLITE_OK);
    // Own view, mid-transaction.
    assert_eq!(query_int(a, "SELECT count(*) FROM t"), 1);
    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    assert_eq!(query_int(a, "SELECT count(*) FROM t"), 1);
    // Autocommit restored.
    assert_eq!(unsafe { sqlite3_get_autocommit(a) }, 1);
    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);

    let b = open_wal_db(&path);
    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 1);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);
}

fn sqlite3_close_wrapper(db: *mut compat::sqlite3) -> c_int {
    unsafe { sqlite3_close(db) }
}

/// THE flagship: a plain AUTOCOMMIT DML on B while A holds a concurrent
/// transaction does not wait — it JOINS the regime (one-statement
/// concurrent transaction), commits first-committer-wins, and is
/// durable immediately. A third reader sees B's row mid-regime but
/// never A's uncommitted one.
#[test]
fn autocommit_dml_joins_concurrent_regime() {
    let path = temp_db("join");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    let reader = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (1)"), SQLITE_OK);

    // B's plain autocommit INSERT: joins, no BUSY (busy_timeout is 0 —
    // a waiting writer would fail immediately).
    assert_eq!(
        exec(b, "INSERT INTO t VALUES (2)"),
        SQLITE_OK,
        "{}",
        err_of(b)
    );

    // Mid-regime visibility: the reader (and B) see ONLY B's committed
    // row — A's shadow pages are A-private.
    assert_eq!(query_int(reader, "SELECT count(*) FROM t"), 1);
    assert_eq!(query_int(reader, "SELECT count(*) FROM t WHERE id = 2"), 1);
    assert_eq!(query_int(reader, "SELECT count(*) FROM t WHERE id = 1"), 0);
    // B is in autocommit before and after its joined statement.
    assert_eq!(unsafe { sqlite3_get_autocommit(b) }, 1);

    // B's row is durable the moment its statement returns: a fresh
    // handle reads it while A's transaction is still open.
    let f = open_db(&path);
    assert_eq!(query_int(f, "SELECT count(*) FROM t WHERE id = 2"), 1);
    assert_eq!(sqlite3_close_wrapper(f), SQLITE_OK);

    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    assert_eq!(query_int(reader, "SELECT count(*) FROM t"), 2);

    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(reader), SQLITE_OK);

    let back = open_db(&path);
    assert_eq!(query_int(back, "SELECT count(*) FROM t"), 2);
    assert_eq!(sqlite3_close_wrapper(back), SQLITE_OK);
}

/// The join commits the hot row FIRST; A's COMMIT loses
/// first-committer-wins and surfaces SQLITE_BUSY_SNAPSHOT (517) —
/// retriable, transaction already rolled back, autocommit restored.
#[test]
fn joiner_same_row_first_committer_wins() {
    let path = temp_db("conflict");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (7)"), SQLITE_OK);

    // B joins and commits id=7 first.
    assert_eq!(
        exec(b, "INSERT INTO t VALUES (7)"),
        SQLITE_OK,
        "{}",
        err_of(b)
    );

    // A's commit conflicts: 517, not a plain BUSY.
    let rc = exec(a, "COMMIT");
    assert_eq!(
        rc,
        SQLITE_BUSY_SNAPSHOT,
        "expected 517, got {rc}: {}",
        err_of(a)
    );
    // The failed concurrent COMMIT ended the transaction: autocommit
    // restored, and a retry (different row) works as plain DML.
    assert_eq!(unsafe { sqlite3_get_autocommit(a) }, 1);
    assert_eq!(
        exec(a, "INSERT INTO t VALUES (8)"),
        SQLITE_OK,
        "{}",
        err_of(a)
    );

    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 2);
    assert_eq!(query_int(b, "SELECT count(*) FROM t WHERE id = 7"), 1);

    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);

    let back = open_db(&path);
    assert_eq!(query_int(back, "SELECT count(*) FROM t"), 2);
    assert_eq!(sqlite3_close_wrapper(back), SQLITE_OK);
}

/// The owner's own reads see its uncommitted writes; a foreign reader
/// never does (the identity arming keeps one handle from being mistaken
/// for the regime's anonymous owner — the pre-join dirty-read hole).
#[test]
fn owner_reads_own_writes_foreign_reader_committed_view() {
    let path = temp_db("visibility");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (1)"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (2)"), SQLITE_OK);

    // A's own view (both a query-form and a streaming SELECT).
    assert_eq!(query_int(a, "SELECT count(*) FROM t"), 2);
    assert_eq!(query_int(a, "SELECT count(*) FROM t WHERE id <= 2"), 2);
    // B's committed view: empty.
    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 0);

    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 2);

    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);
}

/// Regime discipline through the C ABI: plain BEGIN and DDL wait out a
/// concurrent regime (BUSY at busy_timeout 0) — the regimes cannot mix,
/// and DDL cannot join.
#[test]
fn plain_begin_and_ddl_wait_out_the_regime() {
    let path = temp_db("discipline");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (1)"), SQLITE_OK);

    // Plain BEGIN during the regime: BUSY (no wait budget).
    assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY, "{}", err_of(b));
    // DDL during the regime: BUSY.
    assert_eq!(
        exec(b, "CREATE TABLE u (id INTEGER PRIMARY KEY)"),
        SQLITE_BUSY,
        "{}",
        err_of(b)
    );

    // After the drain, both proceed.
    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    assert_eq!(exec(b, "BEGIN"), SQLITE_OK, "{}", err_of(b));
    assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
    assert_eq!(
        exec(b, "CREATE TABLE u (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);
}

/// TRUE multi-writer through the C ABI: two connections hold concurrent
/// transactions at once, interleave writes, commit independently.
#[test]
fn two_concurrent_owners_multi_writer() {
    let path = temp_db("multiwriter");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE ta (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(
        exec(a, "CREATE TABLE tb (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    // Both own concurrent transactions simultaneously — the point of
    // the regime (a plain BEGIN would have been BUSY here).
    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(b, "BEGIN CONCURRENT"), SQLITE_OK, "{}", err_of(b));

    assert_eq!(exec(a, "INSERT INTO ta VALUES (1)"), SQLITE_OK);
    assert_eq!(exec(b, "INSERT INTO tb VALUES (1)"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO ta VALUES (2)"), SQLITE_OK);
    assert_eq!(exec(b, "INSERT INTO tb VALUES (2)"), SQLITE_OK);

    // Each sees only its own uncommitted rows.
    assert_eq!(query_int(a, "SELECT count(*) FROM ta"), 2);
    assert_eq!(query_int(a, "SELECT count(*) FROM tb"), 0);
    assert_eq!(query_int(b, "SELECT count(*) FROM tb"), 2);
    assert_eq!(query_int(b, "SELECT count(*) FROM ta"), 0);

    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    assert_eq!(exec(b, "COMMIT"), SQLITE_OK);

    assert_eq!(query_int(a, "SELECT count(*) FROM ta"), 2);
    assert_eq!(query_int(a, "SELECT count(*) FROM tb"), 2);

    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);

    let back = open_db(&path);
    assert_eq!(query_int(back, "SELECT count(*) FROM ta"), 2);
    assert_eq!(query_int(back, "SELECT count(*) FROM tb"), 2);
    assert_eq!(sqlite3_close_wrapper(back), SQLITE_OK);
}

/// An abandoned concurrent transaction (close without COMMIT) rolls
/// back — SQLite's at-close auto-rollback, extended to the regime; the
/// released hold must not wedge later writers.
#[test]
fn abandoned_concurrent_txn_rolls_back_on_close() {
    let path = temp_db("abandon");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (1)"), SQLITE_OK);
    // Close mid-transaction: implicit ROLLBACK.
    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);

    // The regime is gone: B reads the committed (empty) view and writes
    // plain autocommit DML without any stale-hold BUSY.
    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 0);
    assert_eq!(
        exec(b, "INSERT INTO t VALUES (5)"),
        SQLITE_OK,
        "{}",
        err_of(b)
    );
    // And a fresh BEGIN CONCURRENT works again.
    assert_eq!(exec(b, "BEGIN CONCURRENT"), SQLITE_OK, "{}", err_of(b));
    assert_eq!(exec(b, "INSERT INTO t VALUES (6)"), SQLITE_OK);
    assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 2);

    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);

    let back = open_db(&path);
    assert_eq!(query_int(back, "SELECT count(*) FROM t"), 2);
    assert_eq!(sqlite3_close_wrapper(back), SQLITE_OK);
}

/// The streaming path (prepare + step) joins too — the sqlx/sea-orm
/// route through the C ABI drives DML exactly this way.
#[test]
fn joined_streaming_statement_dml() {
    let path = temp_db("streaming");
    let _ = std::fs::remove_file(&path);
    let a = open_wal_db(&path);
    let b = open_wal_db(&path);
    assert_eq!(
        exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );

    assert_eq!(exec(a, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES (1)"), SQLITE_OK);

    // B's INSERT via prepare+bind+step: joins the regime, DONE in one
    // step.
    let sql_c = cstr("INSERT INTO t VALUES (?)");
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe {
        sqlite3_prepare_v2(
            b,
            sql_c.as_ptr(),
            -1,
            &mut stmt as *mut *mut compat::sqlite3_stmt,
            &mut tail as *mut *const c_char,
        )
    };
    assert_eq!(rc, SQLITE_OK, "{}", err_of(b));
    assert_eq!(unsafe { sqlite3_bind_int64(stmt, 1, 2) }, SQLITE_OK);
    let step = unsafe { sqlite3_step(stmt) };
    assert_eq!(step, SQLITE_DONE, "{}", err_of(b));
    assert_eq!(unsafe { sqlite3_finalize(stmt) }, SQLITE_OK);

    // B's row committed immediately (join), A's still shadowed.
    let reader = open_wal_db(&path);
    assert_eq!(query_int(reader, "SELECT count(*) FROM t WHERE id = 2"), 1);
    assert_eq!(query_int(reader, "SELECT count(*) FROM t WHERE id = 1"), 0);
    assert_eq!(sqlite3_close_wrapper(reader), SQLITE_OK);

    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    assert_eq!(query_int(b, "SELECT count(*) FROM t"), 2);

    assert_eq!(sqlite3_close_wrapper(a), SQLITE_OK);
    assert_eq!(sqlite3_close_wrapper(b), SQLITE_OK);
}
