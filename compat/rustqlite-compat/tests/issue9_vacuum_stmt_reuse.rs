//! Issue #9 harness: statements prepared BEFORE a plain VACUUM on a
//! DELETE-journal file DB must not walk stale root pages after it.
//!
//! The report (pdf-tts's `db rebuild` CI test, sqlx 0.8 over the C ABI):
//! sqlx's per-connection statement cache keeps `sqlite3_stmt` handles
//! alive across a plain `VACUUM`; after the VACUUM rewrites the file and
//! the database is reopened in place, re-running `SELECT COUNT(*) FROM
//! <table>` through the SAME handle failed with
//! `(code: 11) unexpected page type in count: LeafIndex` — the
//! previously-prepared statement kept its pre-VACUUM root.
//!
//! This harness drives the exact surface:
//!
//! - One file DB in DELETE-journal mode (the engine default), one
//!   connection.
//! - `SELECT COUNT(*)` prepared for EVERY table, fully stepped to DONE
//!   (the handles stay alive — sqlx's cache shape).
//! - Bulk-insert + mass-delete so the compact build takes the ROW-LEVEL
//!   path (hollow prefix leaves decline `trees_eligible_for_page_copy`)
//!   — and a second variant that keeps the page-level path.
//! - Plain `VACUUM` on the SAME connection.
//! - `sqlite3_reset` + `sqlite3_step` on the SAME prepared handles: the
//!   counts must match a fresh connection's counts (the report's
//!   workaround shape is the oracle), and `PRAGMA integrity_check` must
//!   report ok.
//! - The memory-store arm (the other `!wal_install` dispatch arm) gets
//!   the same treatment on `:memory:`.

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
    fn sqlite3_reset(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut compat::sqlite3_stmt, idx: c_int, v: i64) -> c_int;
    fn sqlite3_bind_text(
        stmt: *mut compat::sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n: c_int,
        destructor: isize,
    ) -> c_int;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_text(stmt: *mut compat::sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
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
        eprintln!("[exec] {sql} -> {}", unsafe { errmsg_string(errmsg) });
        unsafe { sqlite3_free(errmsg as *mut c_void) };
    }
    rc
}

extern "C" {
    fn sqlite3_free(p: *mut c_void);
}

unsafe fn errmsg_string(p: *mut c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p as *const c_char)
            .to_string_lossy()
            .into_owned()
    }
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

/// Prepare a statement WITHOUT stepping it (DML handles: the sqlx
/// cache shape — prepared, bound, stepped only when the app runs it).
fn prepare_only(db: *mut compat::sqlite3, sql: &str) -> *mut compat::sqlite3_stmt {
    let sql_c = cstr(sql);
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql_c.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "prepare {sql}: {}", errmsg(db));
    stmt
}

/// Prepare + fully step + reset a statement, keeping the handle alive
/// (the sqlx statement-cache shape). Returns the handle.
fn prepare_and_run(db: *mut compat::sqlite3, sql: &str) -> *mut compat::sqlite3_stmt {
    let sql_c = cstr(sql);
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql_c.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "prepare {sql}: {}", errmsg(db));
    let rc = unsafe { sqlite3_step(stmt) };
    assert!(
        rc == SQLITE_ROW || rc == SQLITE_DONE,
        "step {sql}: rc={rc} {}",
        errmsg(db)
    );
    let rc = unsafe { sqlite3_reset(stmt) };
    assert_eq!(rc, SQLITE_OK, "reset {sql}: {}", errmsg(db));
    stmt
}

/// Fresh-connection scalar (the oracle): what the SAME SQL must return
/// through a stale handle after the VACUUM.
fn scalar_i64_fresh(path: &str, sql: &str) -> i64 {
    let path_c = cstr(path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK);
    let sql_c = cstr(sql);
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql_c.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "fresh prepare {sql}: {}", errmsg(db));
    let rc = unsafe { sqlite3_step(stmt) };
    assert_eq!(rc, SQLITE_ROW, "fresh step {sql}: {}", errmsg(db));
    let v = unsafe { sqlite3_column_int64(stmt, 0) };
    unsafe { sqlite3_finalize(stmt) };
    unsafe { sqlite3_close(db) };
    v
}

fn scalar_text_fresh(path: &str, sql: &str) -> String {
    let path_c = cstr(path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK);
    let sql_c = cstr(sql);
    let mut stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql_c.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
    assert_eq!(rc, SQLITE_OK, "fresh prepare {sql}: {}", errmsg(db));
    let rc = unsafe { sqlite3_step(stmt) };
    assert_eq!(rc, SQLITE_ROW, "fresh step {sql}: {}", errmsg(db));
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
    unsafe { sqlite3_close(db) };
    v
}

/// Same-connection scalar through a brand-new statement (diagnostics).
fn scalar_i64_same_conn(db: *mut compat::sqlite3, sql: &str) -> i64 {
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

fn scalar_text_same_conn(db: *mut compat::sqlite3, sql: &str) -> String {
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

/// One full repro round against `path`: prepare counts for every table,
/// churn, VACUUM, then re-run the SAME handles.
///
/// `keep` rows survive of `total` (the mass-delete leaves hollow prefix
/// leaves when `keep` is a small suffix).
fn repro_round(path: &str, label: &str, total: usize, keep: usize) {
    eprintln!("== {label}: {path}");
    let path_c = cstr(path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK, "open {path}");

    // Delete-journal mode, explicitly (the issue's environment; the
    // engine's file default — WAL would take the properly-invalidated
    // install arm).
    let jm = exec(db, "PRAGMA journal_mode=DELETE");
    assert_eq!(jm, SQLITE_OK, "journal_mode=DELETE: {}", errmsg(db));

    // Schema: two tables + a secondary index (the LeafIndex landing pad
    // — count_subtree must never land on an index page).
    assert_eq!(
        exec(
            db,
            "CREATE TABLE events(id INTEGER PRIMARY KEY, k INTEGER, note TEXT)"
        ),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "CREATE INDEX ix_events_k ON events(k)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "CREATE TABLE meta(key TEXT PRIMARY KEY, val TEXT)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "INSERT INTO meta VALUES ('a','1'),('b','2'),('c','3')"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );

    // Bulk insert with scattered keys.
    let mut built = String::from("INSERT INTO events VALUES ");
    for i in 1..=total {
        if i > 1 {
            built.push(',');
        }
        built.push_str(&format!("({},{},'note{}')", i, (i * 7919) % total, i));
    }
    assert_eq!(exec(db, &built), SQLITE_OK, "bulk insert: {}", errmsg(db));

    // sqlx's shape: COUNT for EVERY table, prepared, fully stepped,
    // handles stay alive.
    let c_events = prepare_and_run(db, "SELECT COUNT(*) FROM events");
    let c_meta = prepare_and_run(db, "SELECT COUNT(*) FROM meta");
    // A point-lookup fast-path statement too (root overrides / index
    // roots embed the same pre-VACUUM state).
    let p_lookup = prepare_and_run(db, "SELECT note FROM events WHERE id = 5");

    // Mass-delete the prefix: hollow (0-cell) leaves decline the
    // page-level copy, forcing the ROW-LEVEL rebuild + temp FILE.
    assert_eq!(
        exec(
            db,
            &format!("DELETE FROM events WHERE id <= {}", total - keep)
        ),
        SQLITE_OK,
        "mass delete: {}",
        errmsg(db)
    );
    assert_eq!(exec(db, "DELETE FROM events WHERE id <= 0"), SQLITE_OK);

    // Plain VACUUM on the SAME connection.
    let rc = exec(db, "VACUUM");
    assert_eq!(rc, SQLITE_OK, "VACUUM: {}", errmsg(db));

    // THE ASSERTION: re-run the SAME prepared handles.
    let rc = unsafe { sqlite3_step(c_events) };
    assert_eq!(
        rc,
        SQLITE_ROW,
        "{label}: re-step SELECT COUNT(*) FROM events through the pre-VACUUM handle: {}",
        errmsg(db)
    );
    let got_events = unsafe { sqlite3_column_int64(c_events, 0) };
    let rc2 = unsafe { sqlite3_step(c_meta) };
    assert_eq!(
        rc2,
        SQLITE_ROW,
        "{label}: re-step SELECT COUNT(*) FROM meta through the pre-VACUUM handle: {}",
        errmsg(db)
    );
    let got_meta = unsafe { sqlite3_column_int64(c_meta, 0) };
    let rc3 = unsafe { sqlite3_step(p_lookup) };
    assert!(
        rc3 == SQLITE_ROW || rc3 == SQLITE_DONE,
        "{label}: re-step point lookup: {}",
        errmsg(db)
    );

    // The oracle: fresh-connection counts + integrity.
    let want_events = scalar_i64_fresh(path, "SELECT COUNT(*) FROM events");
    let want_meta = scalar_i64_fresh(path, "SELECT COUNT(*) FROM meta");
    assert_eq!(
        got_events, want_events,
        "{label}: events count through stale handle"
    );
    assert_eq!(
        got_meta, want_meta,
        "{label}: meta count through stale handle"
    );
    let integrity = scalar_text_fresh(path, "PRAGMA integrity_check");
    assert_eq!(integrity, "ok", "{label}: integrity_check after VACUUM");

    unsafe { sqlite3_finalize(c_events) };
    unsafe { sqlite3_finalize(c_meta) };
    unsafe { sqlite3_finalize(p_lookup) };
    unsafe { sqlite3_close(db) };
    eprintln!("== {label}: OK (events={got_events}, meta={got_meta}, integrity ok)");
}

#[test]
fn issue9_prepared_statements_survive_plain_vacuum_delete_journal() {
    let dir = std::env::temp_dir();
    let path = format!("{}/issue9-delete-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&path);
    // Hollow-prefix shape: row-level rebuild path.
    repro_round(&path, "delete-journal/row-level", 20_000, 1_000);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

#[test]
fn issue9_prepared_statements_survive_plain_vacuum_page_level() {
    let dir = std::env::temp_dir();
    let path = format!("{}/issue9-page-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&path);
    // Dense shape (no mass delete): page-level rebuild path, same arm.
    repro_round(&path, "delete-journal/page-level", 5_000, 5_000);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

/// The memory-store arm of the `!wal_install` dispatch: same statement
/// lifecycle on `:memory:`.
#[test]
fn issue9_prepared_statements_survive_plain_vacuum_memory() {
    let path = ":memory:".to_string();
    // In-memory: no file churn, but the same *self = new swap.
    let path_c = cstr(&path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(
        exec(
            db,
            "CREATE TABLE events(id INTEGER PRIMARY KEY, k INTEGER, note TEXT)"
        ),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "CREATE INDEX ix_events_k ON events(k)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "CREATE TABLE meta(key TEXT PRIMARY KEY, val TEXT)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "INSERT INTO meta VALUES ('a','1'),('b','2'),('c','3')"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );

    let mut built = String::from("INSERT INTO events VALUES ");
    for i in 1..=20_000 {
        if i > 1 {
            built.push(',');
        }
        built.push_str(&format!("({},{},'note{}')", i, (i * 7919) % 20_000, i));
    }
    assert_eq!(exec(db, &built), SQLITE_OK, "bulk insert: {}", errmsg(db));

    let c_events = prepare_and_run(db, "SELECT COUNT(*) FROM events");
    let c_meta = prepare_and_run(db, "SELECT COUNT(*) FROM meta");

    assert_eq!(
        exec(db, "DELETE FROM events WHERE id <= 19000"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(exec(db, "VACUUM"), SQLITE_OK, "VACUUM: {}", errmsg(db));

    // Diagnostics kept: a fresh statement's count must also be right
    // (separates a broken new store from stale-handle state).
    assert_eq!(
        scalar_i64_same_conn(db, "SELECT COUNT(*) FROM events"),
        1_000
    );
    assert_eq!(scalar_text_same_conn(db, "PRAGMA integrity_check"), "ok");

    let rc = unsafe { sqlite3_step(c_events) };
    assert_eq!(
        rc,
        SQLITE_ROW,
        "memory: re-step SELECT COUNT(*) FROM events through the pre-VACUUM handle: {}",
        errmsg(db)
    );
    let got = unsafe { sqlite3_column_int64(c_events, 0) };
    assert_eq!(got, 1_000, "memory: events count through stale handle");
    let rc2 = unsafe { sqlite3_step(c_meta) };
    assert_eq!(
        rc2,
        SQLITE_ROW,
        "memory: re-step meta count: {}",
        errmsg(db)
    );
    assert_eq!(unsafe { sqlite3_column_int64(c_meta, 0) }, 3);

    unsafe { sqlite3_finalize(c_events) };
    unsafe { sqlite3_finalize(c_meta) };
    unsafe { sqlite3_close(db) };
}

/// The WAL arm: its install path always reloaded the catalog, but a
/// LIVE statement (this handle) still resolved its plan against the
/// pre-VACUUM roots — the roots MOVED in the image install, so the
/// same stale-root walk applied. The re-prepare contract covers it.
#[test]
fn issue9_prepared_statements_survive_plain_vacuum_wal() {
    let dir = std::env::temp_dir();
    let path = format!("{}/issue9-wal-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
    let path_c = cstr(&path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK, "open {path}");
    let jm = scalar_text_same_conn(db, "PRAGMA journal_mode");
    if jm != "wal" {
        assert_eq!(
            exec(db, "PRAGMA journal_mode=WAL"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        assert_eq!(
            scalar_text_same_conn(db, "PRAGMA journal_mode"),
            "wal",
            "switch to WAL"
        );
    }

    assert_eq!(
        exec(
            db,
            "CREATE TABLE events(id INTEGER PRIMARY KEY, k INTEGER, note TEXT)"
        ),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "CREATE INDEX ix_events_k ON events(k)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    let mut built = String::from("INSERT INTO events VALUES ");
    for i in 1..=20_000 {
        if i > 1 {
            built.push(',');
        }
        built.push_str(&format!("({},{},'note{}')", i, (i * 7919) % 20_000, i));
    }
    assert_eq!(exec(db, &built), SQLITE_OK, "bulk insert: {}", errmsg(db));

    let c_events = prepare_and_run(db, "SELECT COUNT(*) FROM events");
    let p_lookup = prepare_and_run(db, "SELECT note FROM events WHERE id = 19999");

    assert_eq!(
        exec(db, "DELETE FROM events WHERE id <= 19000"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(exec(db, "VACUUM"), SQLITE_OK, "VACUUM: {}", errmsg(db));

    let rc = unsafe { sqlite3_step(c_events) };
    assert_eq!(
        rc,
        SQLITE_ROW,
        "wal: re-step SELECT COUNT(*) FROM events through the pre-VACUUM handle: {}",
        errmsg(db)
    );
    assert_eq!(
        unsafe { sqlite3_column_int64(c_events, 0) },
        1_000,
        "wal: events count through stale handle"
    );
    // id 19999 is a SURVIVOR (the mass delete took id <= 19000): the
    // stale point-lookup handle must find it through the new roots.
    let rc2 = unsafe { sqlite3_step(p_lookup) };
    assert_eq!(
        rc2,
        SQLITE_ROW,
        "wal: re-step point lookup (id=19999): {}",
        errmsg(db)
    );
    let note = unsafe {
        let p = sqlite3_column_text(p_lookup, 0);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p as *const c_char)
                .to_string_lossy()
                .into_owned()
        }
    };
    assert_eq!(
        note, "note19999",
        "wal: stale point lookup serves the right row"
    );

    // Fresh-connection oracle.
    assert_eq!(
        scalar_i64_fresh(&path, "SELECT COUNT(*) FROM events"),
        1_000
    );
    assert_eq!(scalar_text_fresh(&path, "PRAGMA integrity_check"), "ok");

    unsafe { sqlite3_finalize(c_events) };
    unsafe { sqlite3_finalize(p_lookup) };
    unsafe { sqlite3_close(db) };
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

/// The write side of the class: a stale INSERT handle re-run after the
/// VACUUM would target the OLD roots — a corrupting write into
/// remapped pages. The re-prepare contract must re-target it, and the
/// result must be a clean tree.
#[test]
fn issue9_stale_insert_handle_retargets_after_vacuum() {
    let dir = std::env::temp_dir();
    let path = format!("{}/issue9-ins-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&path);
    repro_round(&path, "delete-journal/insert-retarget", 8_000, 4_000);

    // Second pass on the same file: prepare an INSERT, run it, VACUUM,
    // re-run the SAME handle, then verify integrity + the fresh count.
    let path_c = cstr(&path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(
        exec(db, "PRAGMA journal_mode=DELETE"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    // A PARAMETERIZED stale INSERT handle: bound, run, VACUUM, re-bound
    // and re-run — it must target the NEW roots (the old plan's roots
    // were remapped; a stale write there would corrupt the tree).
    let ins = prepare_only(db, "INSERT INTO t(id, v) VALUES (?, ?)");
    let one = cstr("one");
    unsafe { sqlite3_bind_int64(ins, 1, 1) };
    unsafe { sqlite3_bind_text(ins, 2, one.as_ptr(), 3, -1) };
    let rc = unsafe { sqlite3_step(ins) };
    assert_eq!(rc, SQLITE_DONE, "run insert: {}", errmsg(db));
    unsafe { sqlite3_reset(ins) };
    assert_eq!(exec(db, "VACUUM"), SQLITE_OK, "VACUUM: {}", errmsg(db));
    // Re-bind (bindings survive reset; re-bound explicitly here) and
    // re-run the SAME prepared handle against the vacuumed store.
    let two = cstr("two");
    unsafe { sqlite3_bind_int64(ins, 1, 2) };
    unsafe { sqlite3_bind_text(ins, 2, two.as_ptr(), 3, -1) };
    let rc = unsafe { sqlite3_step(ins) };
    assert_eq!(
        rc,
        SQLITE_DONE,
        "re-run insert through stale handle: {}",
        errmsg(db)
    );
    unsafe { sqlite3_reset(ins) };
    assert_eq!(
        scalar_i64_same_conn(db, "SELECT COUNT(*) FROM t"),
        2,
        "count after stale re-insert"
    );
    assert_eq!(
        scalar_i64_same_conn(db, "SELECT COUNT(*) FROM t WHERE v = 'two'"),
        1,
        "re-inserted row is findable"
    );
    assert_eq!(scalar_text_same_conn(db, "PRAGMA integrity_check"), "ok");
    unsafe { sqlite3_finalize(ins) };
    unsafe { sqlite3_close(db) };
    let _ = std::fs::remove_file(&path);
}

/// The DDL face of the re-prepare contract: SQLite errors a stale
/// statement whose table was dropped ("no such table") at re-step —
/// it must never serve the dropped tree's stale plan.
#[test]
fn issue9_stale_select_errors_after_drop() {
    let path_c = cstr(":memory:");
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(
        exec(db, "CREATE TABLE t(a INTEGER, b TEXT)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, "INSERT INTO t VALUES (1,'x'),(2,'y')"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    let sel = prepare_and_run(db, "SELECT COUNT(*) FROM t");
    assert_eq!(exec(db, "DROP TABLE t"), SQLITE_OK, "{}", errmsg(db));
    let rc = unsafe { sqlite3_step(sel) };
    assert_eq!(rc, 1, "SQLITE_ERROR expected at re-step after DROP");
    let msg = errmsg(db);
    assert!(
        msg.contains("no such table"),
        "expected 'no such table' in: {msg}"
    );
    unsafe { sqlite3_finalize(sel) };
    unsafe { sqlite3_close(db) };
}

/// The backup install's class (the sibling `*db = new` site): live
/// statements on the DESTINATION must survive a backup_step that
/// installs a new image — same identity-preserving adoption.
#[test]
fn issue9_prepared_statements_survive_backup_install() {
    // Source: a small populated DB.
    let dir = std::env::temp_dir();
    let src_path = format!("{}/issue9-bak-src-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&src_path);
    {
        let pc = cstr(&src_path);
        let mut sdb: *mut compat::sqlite3 = ptr::null_mut();
        assert_eq!(unsafe { sqlite3_open(pc.as_ptr(), &mut sdb) }, SQLITE_OK);
        assert_eq!(
            exec(sdb, "CREATE TABLE rows(id INTEGER PRIMARY KEY, v TEXT)"),
            SQLITE_OK
        );
        let mut built = String::from("INSERT INTO rows VALUES ");
        for i in 1..=5_000 {
            if i > 1 {
                built.push(',');
            }
            built.push_str(&format!("({i},'v{i}')"));
        }
        assert_eq!(exec(sdb, &built), SQLITE_OK);
        unsafe { sqlite3_close(sdb) };
    }

    // Destination: a FILE DB with a live COUNT statement, then a full
    // backup from the source.
    let dst_path = format!("{}/issue9-bak-dst-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&dst_path);
    let dc = cstr(&dst_path);
    let mut ddb: *mut compat::sqlite3 = ptr::null_mut();
    assert_eq!(unsafe { sqlite3_open(dc.as_ptr(), &mut ddb) }, SQLITE_OK);
    assert_eq!(
        exec(ddb, "CREATE TABLE old(a INTEGER)"),
        SQLITE_OK,
        "{}",
        errmsg(ddb)
    );
    let stale = prepare_and_run(ddb, "SELECT COUNT(*) FROM old");

    let sc = cstr(&src_path);
    let mut sdb2: *mut compat::sqlite3 = ptr::null_mut();
    assert_eq!(unsafe { sqlite3_open(sc.as_ptr(), &mut sdb2) }, SQLITE_OK);
    let rc = full_backup(ddb, sdb2);
    assert_eq!(rc, SQLITE_OK, "backup: {}", errmsg(ddb));

    // The stale handle's table no longer exists in the installed image:
    // the re-prepare contract must error it ("no such table"), not walk
    // the pre-install plan into the new pages.
    let rc = unsafe { sqlite3_step(stale) };
    assert_eq!(rc, 1, "SQLITE_ERROR expected after backup install");
    let msg = errmsg(ddb);
    assert!(
        msg.contains("no such table"),
        "expected 'no such table' in: {msg}"
    );

    // And a fresh statement reads the installed source content.
    assert_eq!(
        scalar_i64_same_conn(ddb, "SELECT COUNT(*) FROM rows"),
        5_000
    );
    assert_eq!(scalar_text_same_conn(ddb, "PRAGMA integrity_check"), "ok");

    unsafe { sqlite3_finalize(stale) };
    unsafe { sqlite3_close(ddb) };
    unsafe { sqlite3_close(sdb2) };
    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}

/// The ATTACH face of the re-prepare contract: an attach-bearing live
/// statement re-validates its schemas and re-routes per execution — it
/// must never fall into the LOCAL planner after an epoch bump (the
/// local catalog cannot see aux.t), and a LOCAL DDL bump must not
/// disturb it.
#[test]
fn issue9_attach_statement_survives_local_ddl_bump() {
    let dir = std::env::temp_dir();
    let aux_path = format!("{}/issue9-aux-{}.db", dir.display(), std::process::id());
    let _ = std::fs::remove_file(&aux_path);
    // Build the aux database with a table + rows.
    {
        let pc = cstr(&aux_path);
        let mut adb: *mut compat::sqlite3 = ptr::null_mut();
        assert_eq!(unsafe { sqlite3_open(pc.as_ptr(), &mut adb) }, SQLITE_OK);
        assert_eq!(
            exec(adb, "CREATE TABLE aux_t(id INTEGER PRIMARY KEY, v TEXT)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(adb, "INSERT INTO aux_t VALUES (1,'a'),(2,'b'),(3,'c')"),
            SQLITE_OK
        );
        unsafe { sqlite3_close(adb) };
    }

    let path_c = cstr(":memory:");
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe { sqlite3_open(path_c.as_ptr(), &mut db) };
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(
        exec(db, "CREATE TABLE local_t(a INTEGER)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    assert_eq!(
        exec(db, &format!("ATTACH '{}' AS aux", aux_path)),
        SQLITE_OK,
        "attach: {}",
        errmsg(db)
    );

    // Live statements: one routed (aux table), one local.
    let sel_aux = prepare_and_run(db, "SELECT COUNT(*) FROM aux.aux_t");
    let sel_local = prepare_and_run(db, "SELECT COUNT(*) FROM local_t");

    // A LOCAL DDL bumps the schema epoch — both stale handles must
    // re-resolve cleanly on their next step (the attach-bearing one by
    // re-validating schemas + re-routing, NOT by local planning).
    assert_eq!(
        exec(db, "CREATE TABLE local_t2(b INTEGER)"),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );

    let rc = unsafe { sqlite3_step(sel_aux) };
    assert_eq!(
        rc,
        SQLITE_ROW,
        "re-step aux SELECT after local DDL: {}",
        errmsg(db)
    );
    assert_eq!(
        unsafe { sqlite3_column_int64(sel_aux, 0) },
        3,
        "aux count through stale handle"
    );
    let rc = unsafe { sqlite3_step(sel_local) };
    assert_eq!(
        rc,
        SQLITE_ROW,
        "re-step local SELECT after DDL: {}",
        errmsg(db)
    );
    assert_eq!(unsafe { sqlite3_column_int64(sel_local, 0) }, 0);

    unsafe { sqlite3_finalize(sel_aux) };
    unsafe { sqlite3_finalize(sel_local) };
    unsafe { sqlite3_close(db) };
    let _ = std::fs::remove_file(&aux_path);
}

/// sqlite3_backup_init + step(-1) + finish, one shot.
fn full_backup(dst: *mut compat::sqlite3, src: *mut compat::sqlite3) -> c_int {
    extern "C" {
        fn sqlite3_backup_init(
            dst: *mut compat::sqlite3,
            dst_name: *const c_char,
            src: *mut compat::sqlite3,
            src_name: *const c_char,
        ) -> *mut c_void;
        fn sqlite3_backup_step(p: *mut c_void, n_page: c_int) -> c_int;
        fn sqlite3_backup_finish(p: *mut c_void) -> c_int;
    }
    let main_c = cstr("main");
    unsafe {
        let p = sqlite3_backup_init(dst, main_c.as_ptr(), src, main_c.as_ptr());
        if p.is_null() {
            return 1;
        }
        let step_rc = sqlite3_backup_step(p, -1);
        let finish_rc = sqlite3_backup_finish(p);
        if step_rc != SQLITE_OK && step_rc != 101 {
            return step_rc;
        }
        finish_rc
    }
}
