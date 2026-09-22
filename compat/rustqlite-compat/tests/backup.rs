//! sqlite3_backup_* C ABI tests — the online-backup family driven the
//! way C applications use it (init → step loop → finish):
//!
//! - File → File: the destination FILE becomes an exact copy of the
//!   source (reopened and verified through a fresh connection, plus the
//!   source staying readable).
//! - :memory: → :memory: and the cross directions (file ↔ memory).
//! - Backup REPLACES the destination's prior content.
//! - Progress accessors: pagecount = source pages at init; remaining
//!   drops to 0; step after DONE keeps returning DONE; finish returns
//!   OK.
//! - init error paths: destination inside a transaction, the same
//!   handle for source and destination, a non-main schema name.
//! - A WAL-mode source's committed state is fully captured.
//! - The backup keeps the SOURCE engine alive after its connection
//!   closes (the handle holds the engine, not the connection).

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
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_backup_init(
        dest: *mut compat::sqlite3,
        dest_name: *const c_char,
        source: *mut compat::sqlite3,
        source_name: *const c_char,
    ) -> *mut compat::sqlite3_backup;
    fn sqlite3_backup_step(backup: *mut compat::sqlite3_backup, n_page: c_int) -> c_int;
    fn sqlite3_backup_remaining(backup: *mut compat::sqlite3_backup) -> c_int;
    fn sqlite3_backup_pagecount(backup: *mut compat::sqlite3_backup) -> c_int;
    fn sqlite3_backup_finish(backup: *mut compat::sqlite3_backup) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

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
    assert_eq!(unsafe { sqlite3_finalize(stmt) }, SQLITE_OK);
    v
}

fn open_db(path: &str) -> *mut compat::sqlite3 {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(path);
    assert_eq!(
        unsafe { sqlite3_open(name.as_ptr(), &mut db as *mut *mut compat::sqlite3) },
        SQLITE_OK,
        "open {path}"
    );
    db
}

/// The classic online-backup loop: step until DONE, then finish.
fn run_backup(dest: *mut compat::sqlite3, src: *mut compat::sqlite3) -> c_int {
    let p = unsafe { sqlite3_backup_init(dest, ptr::null(), src, ptr::null()) };
    assert!(!p.is_null(), "backup_init: {}", err_of(dest));
    loop {
        let rc = unsafe { sqlite3_backup_step(p, -1) };
        assert!(
            rc == SQLITE_DONE || rc == SQLITE_OK || rc == 5,
            "backup_step rc={rc}"
        );
        if rc == SQLITE_DONE {
            break;
        }
    }
    unsafe { sqlite3_backup_finish(p) }
}

fn temp_db(tag: &str) -> String {
    let pid = std::process::id();
    format!("/tmp/compat_backup_{tag}_{pid}.db")
}

/// File → File: exact copy at the destination path, source untouched.
#[test]
fn backup_file_to_file() {
    let src_path = temp_db("src");
    let dst_path = temp_db("dst");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);

    let src = open_db(&src_path);
    assert_eq!(
        exec(src, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"),
        SQLITE_OK
    );
    for i in 0..500 {
        assert_eq!(
            exec(src, &format!("INSERT INTO t VALUES ({i}, 'row{i}')")),
            SQLITE_OK
        );
    }

    let dst = open_db(&dst_path);
    assert_eq!(run_backup(dst, src), SQLITE_OK);

    // The destination connection sees the copy immediately.
    assert_eq!(query_int(dst, "SELECT count(*) FROM t"), 500);
    // (text equality via a count probe — the int helper only reads ints)
    assert_eq!(
        query_int(dst, "SELECT count(*) FROM t WHERE v = 'row499'"),
        1
    );

    // The SOURCE is untouched and still readable.
    assert_eq!(query_int(src, "SELECT count(*) FROM t"), 500);

    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);

    // A FRESH connection to the destination path sees the copy (the
    // file itself, not just the swapped engine).
    let back = open_db(&dst_path);
    assert_eq!(query_int(back, "SELECT count(*) FROM t"), 500);
    assert_eq!(
        query_int(back, "SELECT count(*) FROM t WHERE v = 'row0'"),
        1
    );
    assert_eq!(unsafe { sqlite3_close(back) }, SQLITE_OK);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// Backup REPLACES the destination's prior content entirely (SQLite
/// semantics: the dest ends up as a copy of the source, extras gone).
#[test]
fn backup_replaces_destination_content() {
    let src_path = temp_db("repl_src");
    let dst_path = temp_db("repl_dst");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);

    let src = open_db(&src_path);
    assert_eq!(
        exec(src, "CREATE TABLE keep (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(exec(src, "INSERT INTO keep VALUES (1)"), SQLITE_OK);

    let dst = open_db(&dst_path);
    assert_eq!(
        exec(dst, "CREATE TABLE old (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(exec(dst, "INSERT INTO old VALUES (1)"), SQLITE_OK);

    assert_eq!(run_backup(dst, src), SQLITE_OK);
    assert_eq!(query_int(dst, "SELECT count(*) FROM keep"), 1);
    // The destination's old table is GONE.
    assert_eq!(
        query_int(dst, "SELECT count(*) FROM sqlite_master WHERE name = 'old'"),
        0
    );

    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// :memory: → :memory: (the sqlx test-database pattern: back a live
/// in-memory database up, restore into another).
#[test]
fn backup_memory_to_memory() {
    let src = open_db(":memory:");
    let dst = open_db(":memory:");
    assert_eq!(
        exec(src, "CREATE TABLE m (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(exec(src, "INSERT INTO m VALUES (7)"), SQLITE_OK);

    assert_eq!(run_backup(dst, src), SQLITE_OK);
    assert_eq!(query_int(dst, "SELECT count(*) FROM m"), 1);
    assert_eq!(query_int(dst, "SELECT id FROM m"), 7);
    assert_eq!(query_int(src, "SELECT count(*) FROM m"), 1);

    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);
}

/// File → :memory: and :memory: → File (both cross directions).
#[test]
fn backup_cross_directions() {
    // File → memory.
    let src_path = temp_db("cross_src");
    let _ = std::fs::remove_file(&src_path);
    let src = open_db(&src_path);
    assert_eq!(
        exec(src, "CREATE TABLE x (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(exec(src, "INSERT INTO x VALUES (42)"), SQLITE_OK);
    let mem = open_db(":memory:");
    assert_eq!(run_backup(mem, src), SQLITE_OK);
    assert_eq!(query_int(mem, "SELECT id FROM x"), 42);
    assert_eq!(unsafe { sqlite3_close(mem) }, SQLITE_OK);

    // Memory → file.
    let mem2 = open_db(":memory:");
    assert_eq!(
        exec(mem2, "CREATE TABLE y (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(exec(mem2, "INSERT INTO y VALUES (13)"), SQLITE_OK);
    let dst_path = temp_db("cross_dst");
    let _ = std::fs::remove_file(&dst_path);
    let dst = open_db(&dst_path);
    assert_eq!(run_backup(dst, mem2), SQLITE_OK);
    assert_eq!(query_int(dst, "SELECT id FROM y"), 13);
    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(mem2) }, SQLITE_OK);

    // The file on disk carries the memory database (fresh open).
    let back = open_db(&dst_path);
    assert_eq!(query_int(back, "SELECT id FROM y"), 13);
    assert_eq!(unsafe { sqlite3_close(back) }, SQLITE_OK);

    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// Progress accessors + step/finish semantics: pagecount at init,
/// remaining draining, DONE idempotent, finish OK.
#[test]
fn backup_progress_accessors() {
    let src_path = temp_db("prog_src");
    let dst_path = temp_db("prog_dst");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);

    let src = open_db(&src_path);
    assert_eq!(
        exec(src, "CREATE TABLE p (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    for i in 0..100 {
        assert_eq!(exec(src, &format!("INSERT INTO p VALUES ({i})")), SQLITE_OK);
    }
    let dst = open_db(&dst_path);

    let p = unsafe { sqlite3_backup_init(dst, ptr::null(), src, ptr::null()) };
    assert!(!p.is_null());
    let pages = unsafe { sqlite3_backup_pagecount(p) };
    assert!(pages > 0, "source has pages");
    let rem = unsafe { sqlite3_backup_remaining(p) };
    assert_eq!(rem, pages, "nothing copied yet");

    // A zero page budget copies nothing (SQLite's nPage==0 step).
    assert_eq!(unsafe { sqlite3_backup_step(p, 0) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_backup_remaining(p) }, pages);

    // The one-shot copy: DONE, remaining 0, DONE again.
    assert_eq!(unsafe { sqlite3_backup_step(p, -1) }, SQLITE_DONE);
    assert_eq!(unsafe { sqlite3_backup_remaining(p) }, 0);
    assert_eq!(unsafe { sqlite3_backup_pagecount(p) }, pages);
    assert_eq!(unsafe { sqlite3_backup_step(p, -1) }, SQLITE_DONE);
    assert_eq!(unsafe { sqlite3_backup_finish(p) }, SQLITE_OK);

    assert_eq!(query_int(dst, "SELECT count(*) FROM p"), 100);
    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// init error paths, exactly SQLite's: destination in a transaction,
/// the same handle both ways, a non-main schema name.
#[test]
fn backup_init_errors() {
    let src_path = temp_db("err_src");
    let dst_path = temp_db("err_dst");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);

    let src = open_db(&src_path);
    assert_eq!(
        exec(src, "CREATE TABLE t (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    let dst = open_db(&dst_path);

    // Same handle both ways.
    let p = unsafe { sqlite3_backup_init(dst, ptr::null(), dst, ptr::null()) };
    assert!(p.is_null());
    assert_eq!(err_of(dst), "source and destination must be distinct");

    // Non-main schema name.
    let bad = cstr("temp");
    let p = unsafe { sqlite3_backup_init(dst, bad.as_ptr(), src, ptr::null()) };
    assert!(p.is_null());

    // Destination inside a transaction.
    assert_eq!(exec(dst, "BEGIN"), SQLITE_OK);
    let p = unsafe { sqlite3_backup_init(dst, ptr::null(), src, ptr::null()) };
    assert!(p.is_null());
    assert_eq!(err_of(dst), "destination database is in use");
    assert_eq!(exec(dst, "ROLLBACK"), SQLITE_OK);

    // After the errors the destination still works and a proper backup
    // succeeds (the error paths left no state behind).
    assert_eq!(run_backup(dst, src), SQLITE_OK);
    assert_eq!(query_int(dst, "SELECT count(*) FROM t"), 0); // source table empty

    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// A WAL-mode source's COMMITTED state is fully captured (the image
/// reflects committed frames, not just the main-file bytes).
#[test]
fn backup_wal_mode_source() {
    let src_path = temp_db("wal_src");
    let dst_path = temp_db("wal_dst");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);

    let src = open_db(&src_path);
    assert_eq!(exec(src, "PRAGMA journal_mode = WAL"), SQLITE_OK);
    assert_eq!(
        exec(src, "CREATE TABLE w (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    // Committed transaction + autocommit rows after it.
    assert_eq!(exec(src, "BEGIN"), SQLITE_OK);
    for i in 0..50 {
        assert_eq!(exec(src, &format!("INSERT INTO w VALUES ({i})")), SQLITE_OK);
    }
    assert_eq!(exec(src, "COMMIT"), SQLITE_OK);
    assert_eq!(exec(src, "INSERT INTO w VALUES (999)"), SQLITE_OK);

    let dst = open_db(&dst_path);
    assert_eq!(run_backup(dst, src), SQLITE_OK);
    assert_eq!(query_int(dst, "SELECT count(*) FROM w"), 51);
    assert_eq!(query_int(dst, "SELECT count(*) FROM w WHERE id = 999"), 1);

    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);

    let back = open_db(&dst_path);
    assert_eq!(query_int(back, "SELECT count(*) FROM w"), 51);
    assert_eq!(unsafe { sqlite3_close(back) }, SQLITE_OK);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
    let _ = std::fs::remove_file(format!("{src_path}-wal"));
    let _ = std::fs::remove_file(format!("{dst_path}-wal"));
}

/// The backup handle keeps the SOURCE ENGINE alive after its connection
/// closes (SQLite: the backup owns the database, not the connection) —
/// the step after close still sees the data.
#[test]
fn backup_source_connection_closes_early() {
    let src_path = temp_db("alive_src");
    let dst_path = temp_db("alive_dst");
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);

    let src = open_db(&src_path);
    assert_eq!(
        exec(src, "CREATE TABLE a (id INTEGER PRIMARY KEY)"),
        SQLITE_OK
    );
    assert_eq!(exec(src, "INSERT INTO a VALUES (5)"), SQLITE_OK);
    let dst = open_db(&dst_path);

    let p = unsafe { sqlite3_backup_init(dst, ptr::null(), src, ptr::null()) };
    assert!(!p.is_null());
    // Close the source connection BEFORE stepping.
    assert_eq!(unsafe { sqlite3_close(src) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_backup_step(p, -1) }, SQLITE_DONE);
    assert_eq!(unsafe { sqlite3_backup_finish(p) }, SQLITE_OK);
    assert_eq!(query_int(dst, "SELECT id FROM a"), 5);
    assert_eq!(unsafe { sqlite3_close(dst) }, SQLITE_OK);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}
