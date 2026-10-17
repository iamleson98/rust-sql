//! Issue #7 harness: multi-handle transactional INSERTs through the C
//! ABI — the production shape (sqlx-sqlite over a pooled C-ABI handle
//! set, PLAIN deferred transactions, no `concurrent_transactions`).
//!
//! The report: 6 concurrent workers doing deferred transactions lost 3
//! of 311 brand rows (visible in the file, 404 to the live session,
//! gone after a pool teardown rollback) and logged nested-BEGIN 500s —
//! state bleeding across pooled connections. This harness drives the
//! same surface deterministically:
//!
//! - FOUR connections to one file DB, each with a 5 s busy_timeout
//!   (sqlx's default), round-based workload: every round, every handle
//!   runs BEGIN → batch of indexed INSERTs → COMMIT.
//! - Mid-transaction INTERLEAVE attempts: while handle A's transaction
//!   is open, handle B tries its own BEGIN + INSERT. The single-writer
//!   slot must serialize it (BUSY now, or served after A's COMMIT) —
//!   it must NEVER silently join A's transaction.
//! - After EVERY round: the full scan count must equal the expected
//!   total, EVERY new key must be reachable through the PK index
//!   (point lookup), the secondary index must see the same rows as the
//!   scan (the #7 symptom class: index entries lost for present rows),
//!   and PRAGMA integrity_check must report "ok".
//! - Abandoned transactions (BEGIN + writes + close with no COMMIT)
//!   must roll back cleanly and leave the pool's other connections
//!   able to begin, write and commit — no stuck in_tx, no leaked
//!   engine transaction, no nested-BEGIN errors afterwards.

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
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut compat::sqlite3_stmt, idx: c_int, v: i64) -> c_int;
    fn sqlite3_bind_text(
        stmt: *mut compat::sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: c_int,
        destructor: isize,
    ) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_text(stmt: *mut compat::sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_busy_timeout(db: *mut compat::sqlite3, ms: c_int) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_BUSY: c_int = 5;
const SQLITE_ERROR: c_int = 1;
/// sqlite3_bind_text with a negative length + SQLITE_TRANSIENT (the
/// conventional ((sqlite3_destructor_type)-1)).
const SQLITE_TRANSIENT: isize = -1;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn exec(db: *mut compat::sqlite3, sql: &str) -> c_int {
    let sql_c = cstr(sql);
    let mut errmsg: *mut c_char = ptr::null_mut();
    let rc = unsafe {
        sqlite3_exec(
            db,
            sql_c.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut errmsg,
        )
    };
    if !errmsg.is_null() {
        // Leaked on purpose-critical paths only; tests are short-lived.
        unsafe { sqlite3_free_compat(errmsg) };
    }
    rc
}

extern "C" {
    fn sqlite3_free(p: *mut c_void);
}

unsafe fn sqlite3_free_compat(p: *mut c_char) {
    sqlite3_free(p as *mut c_void);
}

/// `exec` with the engine's error message (the savepoint tests assert
/// the exact failure classes).
fn exec_msg(db: *mut compat::sqlite3, sql: &str) -> (c_int, String) {
    let rc = exec(db, sql);
    (rc, errmsg(db))
}

fn errmsg(db: *mut compat::sqlite3) -> String {
    unsafe {
        let p = sqlite3_errmsg(db);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// `SELECT COUNT(*)`-style scalar int query.
fn scalar_i64(db: *mut compat::sqlite3, sql: &str) -> i64 {
    let sql_c = cstr(sql);
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql_c.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "prepare {sql}: {}", errmsg(db));
    let rc = unsafe { sqlite3_step(stmt) };
    assert_eq!(rc, SQLITE_ROW, "step {sql}: {}", errmsg(db));
    let v = unsafe { sqlite3_column_int64(stmt, 0) };
    unsafe { sqlite3_finalize(stmt) };
    v
}

fn scalar_text(db: *mut compat::sqlite3, sql: &str) -> String {
    let sql_c = cstr(sql);
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql_c.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "prepare {sql}: {}", errmsg(db));
    let rc = unsafe { sqlite3_step(stmt) };
    assert_eq!(rc, SQLITE_ROW, "step {sql}: {}", errmsg(db));
    let p = unsafe { sqlite3_column_text(stmt, 0) };
    let v = unsafe {
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p as *const c_char)
                .to_string_lossy()
                .into_owned()
        }
    };
    unsafe { sqlite3_finalize(stmt) };
    v
}

fn key(round: usize, w: usize, i: usize) -> String {
    format!("{:04}-{:02}-{:04}", round, w, i)
}

/// One handle's transactional batch: BEGIN, `n` indexed INSERTs, COMMIT.
/// Returns Err(()) with the conn's error message on any failure.
fn tx_batch(db: *mut compat::sqlite3, round: usize, w: usize, n: usize) -> Result<(), String> {
    let rc = exec(db, "BEGIN");
    if rc != SQLITE_OK {
        // The nested-BEGIN 500 class from the report — surface it loudly.
        return Err(format!("BEGIN rc={rc}: {}", errmsg(db)));
    }
    for i in 0..n {
        let k = key(round, w, i);
        let sql = cstr("INSERT INTO brand (id, name, w) VALUES (?1, ?2, ?3)");
        let mut stmt = ptr::null_mut();
        let rc = unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
        if rc != SQLITE_OK {
            return Err(format!("prepare insert: {}", errmsg(db)));
        }
        unsafe {
            sqlite3_bind_text(stmt, 1, cstr(&k).as_ptr(), -1, SQLITE_TRANSIENT);
            sqlite3_bind_text(
                stmt,
                2,
                cstr(&format!("brand-{k}")).as_ptr(),
                -1,
                SQLITE_TRANSIENT,
            );
            sqlite3_bind_int64(stmt, 3, w as i64);
        }
        let rc = unsafe { sqlite3_step(stmt) };
        unsafe { sqlite3_finalize(stmt) };
        if rc != SQLITE_DONE {
            return Err(format!("insert {k} rc={rc}: {}", errmsg(db)));
        }
    }
    let rc = exec(db, "COMMIT");
    if rc != SQLITE_OK {
        return Err(format!("COMMIT rc={rc}: {}", errmsg(db)));
    }
    Ok(())
}

#[test]
fn issue7_multi_handle_interleaved_transactional_inserts() {
    let dir = tempfile_dir();
    let path = cstr(&format!("{}/issue7.db", dir));

    // The writer handle (DDL + round verification) + four worker
    // handles — the pooled sqlx shape.
    let mut worker: Vec<*mut compat::sqlite3> = Vec::new();
    let mut admin: *mut compat::sqlite3 = ptr::null_mut();
    unsafe {
        assert_eq!(
            sqlite3_open(path.as_ptr(), &mut admin),
            SQLITE_OK,
            "{}",
            errmsg(admin)
        );
        exec(
            admin,
            "CREATE TABLE brand (id TEXT PRIMARY KEY, name TEXT, w INTEGER)",
        );
        exec(admin, "CREATE INDEX brand_w ON brand(w)");
        for _ in 0..4 {
            let mut db = ptr::null_mut();
            assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
            // sqlx's default busy timeout.
            sqlite3_busy_timeout(db, 5000);
            worker.push(db);
        }
        sqlite3_busy_timeout(admin, 5000);
    }

    const ROUNDS: usize = 6;
    const BATCH: usize = 20;
    let mut expected: i64 = 0;

    for round in 0..ROUNDS {
        // ── The interleave attempt ── handle 0 opens its transaction
        // and writes HALF its batch; handle 1 then tries to BEGIN and
        // write. Single-writer slot: handle 1 must either BUSY (return
        // immediately) or wait for 0's COMMIT — its rows may NEVER
        // silently ride inside 0's transaction. We accept both, and
        // verify the serialization is honest below.
        let rc = exec(worker[0], "BEGIN");
        assert_eq!(
            rc,
            SQLITE_OK,
            "round {round} w0 BEGIN: {}",
            errmsg(worker[0])
        );
        for i in 0..BATCH / 2 {
            insert_brand(worker[0], &key(round, 0, i), round);
        }

        let mut b_joined_silently = false;
        let rc = exec(worker[1], "BEGIN");
        if rc == SQLITE_OK {
            // Busy-timeout served us AFTER 0's transaction... but 0 has
            // NOT committed yet — its transaction is still open (we hold
            // it mid-batch). If this arm fires, the slot was NOT held:
            // either the gate is broken or BEGIN doesn't take the slot.
            // Flag it; the round verification decides (a silent join
            // shows up as missing/duplicated state below).
            b_joined_silently = true;
        } else {
            // Expected class: SQLITE_BUSY (or an explicit error) — the
            // single-writer slot refuses a second transaction.
            let msg = errmsg(worker[1]);
            assert!(
                rc == SQLITE_BUSY || msg.contains("locked"),
                "round {round}: w1 BEGIN must BUSY while w0's tx is open, got rc={rc}: {msg}"
            );
            assert_ne!(
                rc,
                SQLITE_ERROR,
                "round {round}: w1 BEGIN must not be the nested-BEGIN error (that is the #7 bug class)"
            );
        }
        let _ = b_joined_silently;

        // w0 finishes its batch and commits.
        for i in BATCH / 2..BATCH {
            insert_brand(worker[0], &key(round, 0, i), round);
        }
        let rc = exec(worker[0], "COMMIT");
        assert_eq!(
            rc,
            SQLITE_OK,
            "round {round} w0 COMMIT: {}",
            errmsg(worker[0])
        );
        expected += BATCH as i64;

        // ── The concurrent-worker round proper: every handle runs its
        // full transactional batch (serialized by the slot; the
        // busy_timeout rides each out).
        for (w, db) in worker.iter().enumerate().skip(1) {
            tx_batch(*db, round, w, BATCH)
                .unwrap_or_else(|e| panic!("round {round} worker {w}: {e}"));
            expected += BATCH as i64;
        }

        // ── Round gate 1: full scan sees every row.
        let scan = scalar_i64(admin, "SELECT COUNT(*) FROM brand");
        assert_eq!(scan, expected, "round {round}: full scan count");

        // ── Round gate 2: PK index point lookup for EVERY new key.
        for w in 0..4 {
            for i in 0..BATCH {
                let k = key(round, w, i);
                let n = scalar_i64(
                    admin,
                    &format!("SELECT COUNT(*) FROM brand WHERE id = '{k}'"),
                );
                assert_eq!(n, 1, "round {round}: PK point lookup lost {k}");
            }
        }

        // ── Round gate 3: the indexes see exactly the scan's rows.
        let pk_range = scalar_i64(admin, "SELECT COUNT(*) FROM brand WHERE id > '0000'");
        assert_eq!(pk_range, expected, "round {round}: PK index range");
        let sec = scalar_i64(admin, "SELECT COUNT(*) FROM brand WHERE w >= 0");
        assert_eq!(sec, expected, "round {round}: secondary index range");

        // ── Round gate 4: integrity.
        let ic = scalar_text(admin, "PRAGMA integrity_check");
        assert_eq!(ic, "ok", "round {round}: integrity_check");
    }

    // ── The abandoned-transaction class: handle 2 begins and writes,
    // then the handle is CLOSED without COMMIT (a pooled connection
    // torn down mid-transaction — the report's docker service update).
    unsafe {
        let rc = exec(worker[2], "BEGIN");
        assert_eq!(rc, SQLITE_OK, "abandoned BEGIN: {}", errmsg(worker[2]));
        for i in 0..8 {
            insert_brand(worker[2], &key(999, 2, i), 999);
        }
        sqlite3_close(worker[2]);
        worker[2] = ptr::null_mut();
    }
    let scan = scalar_i64(admin, "SELECT COUNT(*) FROM brand");
    assert_eq!(scan, expected, "abandoned tx rows must be rolled back");
    let sec = scalar_i64(admin, "SELECT COUNT(*) FROM brand WHERE w >= 0");
    assert_eq!(sec, expected, "abandoned tx must leave no index entries");

    // The remaining pool stays healthy: fresh BEGIN/INSERT/COMMIT on
    // every surviving handle — no stuck in_tx (the nested-BEGIN 500
    // class), no leaked engine transaction (the BUSY-forever class).
    for (w, db) in worker.iter().enumerate() {
        if w == 2 {
            continue;
        }
        tx_batch(*db, 1000, w, 5).unwrap_or_else(|e| panic!("post-abandon worker {w}: {e}"));
        expected += 5;
    }
    let scan = scalar_i64(admin, "SELECT COUNT(*) FROM brand");
    assert_eq!(scan, expected, "post-abandon scan");
    let sec = scalar_i64(admin, "SELECT COUNT(*) FROM brand WHERE w >= 0");
    assert_eq!(sec, expected, "post-abandon index");
    let ic = scalar_text(admin, "PRAGMA integrity_check");
    assert_eq!(ic, "ok", "post-abandon integrity_check");

    unsafe {
        for db in worker {
            if !db.is_null() {
                sqlite3_close(db);
            }
        }
        sqlite3_close(admin);
    }
    std::fs::remove_dir_all(std::path::Path::new(&dir)).ok();
}

fn insert_brand(db: *mut compat::sqlite3, k: &str, round: usize) {
    let _ = round;
    let sql = cstr("INSERT INTO brand (id, name, w) VALUES (?1, ?2, ?3)");
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "prepare insert: {}", errmsg(db));
    unsafe {
        sqlite3_bind_text(stmt, 1, cstr(k).as_ptr(), -1, SQLITE_TRANSIENT);
        sqlite3_bind_text(
            stmt,
            2,
            cstr(&format!("brand-{k}")).as_ptr(),
            -1,
            SQLITE_TRANSIENT,
        );
        sqlite3_bind_int64(stmt, 3, 2);
    }
    let rc = unsafe { sqlite3_step(stmt) };
    unsafe { sqlite3_finalize(stmt) };
    assert_eq!(rc, SQLITE_DONE, "insert {k}: {}", errmsg(db));
}

fn tempfile_dir() -> String {
    let dir = std::env::temp_dir().join(format!(
        "issue7-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

/// Issue #7 ROOT CAUSE pin: sqlx's NESTED-transaction shape issues
/// `SAVEPOINT` (no BEGIN). SQLite semantics: an outermost SAVEPOINT
/// STARTS a transaction (the engine's `savepoint_txn`); the compat
/// layer must claim the cross-connection slot for it exactly like a
/// plain BEGIN — or the engine's transaction is ANONYMOUS and every
/// other connection's BEGIN fails "cannot start a transaction within
/// a transaction" (the report's 500s) while their DML silently joins
/// it (the swallowed writes, lost at the teardown rollback).
#[test]
fn issue7_savepoint_transaction_takes_the_slot() {
    let dir = tempfile_dir();
    let path = cstr(&format!("{}/sp.db", dir));

    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    unsafe {
        assert_eq!(sqlite3_open(path.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(path.as_ptr(), &mut b), SQLITE_OK);
        sqlite3_busy_timeout(a, 5000);
        sqlite3_busy_timeout(b, 5000);
    }
    let (_, m) = exec_msg(a, "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)");
    assert!(m.is_empty(), "setup: {m}");

    // A opens the sqlx nested-transaction shape and writes.
    let (rc, m) = exec_msg(a, "SAVEPOINT __sqlx_sp_1");
    assert_eq!(rc, SQLITE_OK, "A SAVEPOINT: {m}");
    let (rc, m) = exec_msg(a, "INSERT INTO t (v) VALUES ('a1')");
    assert_eq!(rc, SQLITE_OK, "A INSERT in savepoint: {m}");

    // B's BEGIN must NOT be the nested-BEGIN error (the #7 500 class).
    // With no busy budget on this path it BUSYs immediately — the
    // single-writer slot is held by A's savepoint transaction.
    let (rc, m) = exec_msg(b, "BEGIN");
    assert_eq!(
        rc, SQLITE_BUSY,
        "B BEGIN while A's savepoint-tx is open: rc={rc} {m} (the old bug: rc=1 \
           'cannot start a transaction within a transaction')"
    );

    // B's DML must NEVER silently join A's transaction.
    let (rc, m) = exec_msg(b, "INSERT INTO t (v) VALUES ('b1')");
    assert_eq!(rc, SQLITE_BUSY, "B INSERT must refuse: rc={rc} {m}");

    // A's inner rollback + the outermost RELEASE (sqlx's nested-tx
    // rollback path): the ROLLBACK TO unwinds to the savepoint, the
    // RELEASE of the outermost COMMITS the savepoint-started tx.
    let (rc, m) = exec_msg(a, "ROLLBACK TO __sqlx_sp_1");
    assert_eq!(rc, SQLITE_OK, "A ROLLBACK TO: {m}");
    let (rc, m) = exec_msg(a, "RELEASE __sqlx_sp_1");
    assert_eq!(rc, SQLITE_OK, "A RELEASE: {m}");

    // After A's transaction ended, B can begin, write and commit.
    let (rc, m) = exec_msg(b, "BEGIN");
    assert_eq!(rc, SQLITE_OK, "B BEGIN after A released: {m}");
    let (rc, m) = exec_msg(b, "INSERT INTO t (v) VALUES ('b1')");
    assert_eq!(rc, SQLITE_OK, "B INSERT: {m}");
    let (rc, m) = exec_msg(b, "COMMIT");
    assert_eq!(rc, SQLITE_OK, "B COMMIT: {m}");

    // A's row was rolled back by its own ROLLBACK TO; B's survived.
    let n = scalar_i64(b, "SELECT COUNT(*) FROM t");
    assert_eq!(n, 1, "exactly B's row survives");
    let ic = scalar_text(b, "PRAGMA integrity_check");
    assert_eq!(ic, "ok");

    unsafe {
        sqlite3_close(a);
        sqlite3_close(b);
    }
    std::fs::remove_dir_all(std::path::Path::new(&dir)).ok();
}

/// The nested-in-BEGIN shape: a SAVEPOINT INSIDE a plain BEGIN
/// transaction must not disturb the slot, and RELEASE of it must NOT
/// commit the BEGIN-started transaction.
#[test]
fn issue7_savepoint_nested_in_begin_keeps_tx_open() {
    let dir = tempfile_dir();
    let path = cstr(&format!("{}/sp2.db", dir));
    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    unsafe {
        assert_eq!(sqlite3_open(path.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(path.as_ptr(), &mut b), SQLITE_OK);
        sqlite3_busy_timeout(a, 5000);
        sqlite3_busy_timeout(b, 5000);
    }
    let (_, m) = exec_msg(a, "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)");
    assert!(m.is_empty(), "setup: {m}");

    let (rc, m) = exec_msg(a, "BEGIN");
    assert_eq!(rc, SQLITE_OK, "A BEGIN: {m}");
    let (rc, m) = exec_msg(a, "SAVEPOINT sp1");
    assert_eq!(rc, SQLITE_OK, "A SAVEPOINT: {m}");
    let (rc, m) = exec_msg(a, "INSERT INTO t (v) VALUES ('x')");
    assert_eq!(rc, SQLITE_OK, "A INSERT: {m}");
    // Releasing the nested savepoint must NOT end the BEGIN transaction:
    // the foreign connection still BUSYs, A's COMMIT still commits.
    let (rc, m) = exec_msg(a, "RELEASE sp1");
    assert_eq!(rc, SQLITE_OK, "A RELEASE nested: {m}");
    let (rc, m) = exec_msg(b, "BEGIN");
    assert_eq!(rc, SQLITE_BUSY, "B BEGIN while A's BEGIN tx open: {m}");
    let (rc, m) = exec_msg(a, "COMMIT");
    assert_eq!(rc, SQLITE_OK, "A COMMIT: {m}");
    let n = scalar_i64(a, "SELECT COUNT(*) FROM t");
    assert_eq!(n, 1, "A's row committed at COMMIT, not at RELEASE");

    unsafe {
        sqlite3_close(a);
        sqlite3_close(b);
    }
    std::fs::remove_dir_all(std::path::Path::new(&dir)).ok();
}
