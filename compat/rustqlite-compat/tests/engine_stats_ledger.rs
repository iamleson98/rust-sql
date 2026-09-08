//! Engine-stats ledger regression (C-ABI surface — the exact path the
//! backend app drives: sqlx-sqlite → sqlite3_* → rustqlite engine).
//!
//! Pins the three fixes found from the k6 03-chat DB stats card:
//! 1. `PRAGMA cache_size=-65536` (64 MiB) must resolve to a 64 MiB
//!    capacity — the old KiB÷bytes conversion produced 16 pages.
//! 2. `engine_stats().files[*].total_changes` must report row mutations
//!    from ANY thread (it was a thread-local that read 0 on the stats
//!    thread).
//! 3. The transaction ledger must count IMPLICIT auto-commit write
//!    transactions (an all-zero ledger while 15k writes execute was
//!    misleading).

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::ptr;

extern crate sqlite3 as compat;

extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        ppdb: *mut *mut compat::sqlite3,
        flags: c_int,
        zvfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
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
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;

struct Db(*mut compat::sqlite3);
impl Drop for Db {
    fn drop(&mut self) {
        unsafe { sqlite3_close(self.0) };
    }
}

fn open(path: &std::path::Path) -> Db {
    let cpath = CString::new(path.to_str().unwrap()).unwrap();
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe {
        sqlite3_open_v2(
            cpath.as_ptr(),
            &mut db,
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
            ptr::null(),
        )
    };
    assert_eq!(rc, SQLITE_OK, "open failed");
    Db(db)
}

/// Run one statement to DONE via prepare + step (the sqlx execution shape).
fn run(db: &Db, sql: &str) {
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
    assert_eq!(
        rc,
        SQLITE_OK,
        "prepare {sql:?} failed: {}",
        errmsg(db.0)
    );
    let rc = unsafe { sqlite3_step(stmt) };
    unsafe { sqlite3_finalize(stmt) };
    assert!(
        rc == SQLITE_DONE || rc == SQLITE_ROW,
        "step {sql:?} returned {rc}: {}",
        errmsg(db.0)
    );
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

fn file_stats(name_part: &str) -> compat::EngineFileStats {
    let stats = compat::engine_stats();
    stats
        .files
        .into_iter()
        .find(|f| f.name.contains(name_part))
        .expect("engine file registered in stats")
}

#[test]
fn engine_stats_ledger_and_cache_capacity_through_c_abi() {
    let tag = format!("ledger-{}", std::process::id());
    let path = std::env::temp_dir().join(format!("compat-{tag}.db"));
    let _ = std::fs::remove_file(&path);
    let db = open(&path);

    // ── 1. cache_size KiB semantics through the C-ABI statement path ──
    run(&db, "PRAGMA cache_size=-65536");
    let f = file_stats(&tag);
    assert!(f.page_size > 0, "page size reported");
    let cap_bytes = f.cache_capacity_pages as u64 * f.page_size as u64;
    assert_eq!(
        cap_bytes,
        65536 * 1024,
        "PRAGMA cache_size=-65536 must resolve to 64 MiB (got {} pages x {} B = {} B; the old bug gave 16 pages)",
        f.cache_capacity_pages,
        f.page_size,
        cap_bytes
    );

    // ── 2. auto-commit writes count as implicit transactions ──
    let before = compat::engine_stats();
    let before_begun = before.transactions_begun;
    let before_committed = before.transactions_committed;

    run(&db, "CREATE TABLE t (a INTEGER)"); // 1 implicit txn
    for i in 1..=5i64 {
        run(&db, &format!("INSERT INTO t VALUES ({i})")); // 5 implicit txns
    }

    let after = compat::engine_stats();
    assert_eq!(after.transactions_begun - before_begun, 6);
    assert_eq!(after.transactions_committed - before_committed, 6);

    // ── 3. explicit transaction: BEGIN/COMMIT counted once each ──
    run(&db, "BEGIN");
    run(&db, "INSERT INTO t VALUES (100)"); // in-txn: no implicit count
    run(&db, "COMMIT");
    let after2 = compat::engine_stats();
    assert_eq!(after2.transactions_begun - after.transactions_begun, 1);
    assert_eq!(after2.transactions_committed - after.transactions_committed, 1);

    // ── 4. rollback: begun + rolled_back ──
    run(&db, "BEGIN");
    run(&db, "INSERT INTO t VALUES (101)");
    run(&db, "ROLLBACK");
    let after3 = compat::engine_stats();
    assert_eq!(after3.transactions_begun - after2.transactions_begun, 1);
    assert_eq!(
        after3.transactions_rolled_back - after2.transactions_rolled_back,
        1
    );
    assert_eq!(
        after3.transactions_committed - after2.transactions_committed,
        0
    );

    // ── 5. total_changes: 6 committed rows + 1 rolled-back statement
    // (SQLite's total_changes counts statement completions).
    // READ FROM A DIFFERENT THREAD — the stats endpoint's thread never
    // executes statements; the old thread-local counter read 0 there.
    let tag_owned = tag.clone();
    let cross_thread = std::thread::spawn(move || file_stats(&tag_owned))
        .join()
        .expect("stats thread");
    assert_eq!(cross_thread.total_changes, 7);
    assert_eq!(cross_thread.cache_capacity_pages, f.cache_capacity_pages);

    // ── 6. the per-file ledger matches the process totals' movement ──
    let f_now = file_stats(&tag);
    assert!(f_now.live_connections >= 1);
    assert_eq!(f_now.total_changes, 7);

    drop(db);
    let _ = std::fs::remove_file(&path);
}
