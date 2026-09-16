//! pdf-tts mirror-sync regression through the C ABI prepared path.
//!
//! Exact sequence from pdf-tts's `sync_default_version_audio` +
//! `db_engine_contract.rs::sea_orm_composite_pk_upsert_after_migrations`:
//! schema (composite TEXT PK + ALTER ADD COLUMN), seeds, sync(a),
//! correlated-subquery SELECT probe, sync(b) with fresh SQL text.
//!
//! The engine's `Database::execute` path handles every shape here
//! (verified by examples/probe_correlated.rs); this file pins the SAME
//! shapes through prepare/bind/step — the path sqlx/sea-orm drives.

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::ptr;

extern crate sqlite3 as compat;

unsafe extern "C" {
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
    fn sqlite3_step(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_text(
        stmt: *mut compat::sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        len: c_int,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_column_text(stmt: *mut compat::sqlite3_stmt, col: c_int) -> *const c_char;
    fn sqlite3_column_count(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_bind_parameter_count(stmt: *mut compat::sqlite3_stmt) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

struct Db(*mut compat::sqlite3);

impl Db {
    fn open() -> Db {
        let name = CString::new(":memory:").unwrap();
        let mut db: *mut compat::sqlite3 = ptr::null_mut();
        let rc = unsafe { sqlite3_open_v2(name.as_ptr(), &mut db, 0x2 | 0x4, ptr::null()) };
        assert_eq!(rc, SQLITE_OK);
        Db(db)
    }
    fn exec(&self, sql: &str) {
        let csql = CString::new(sql).unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe { sqlite3_prepare_v3(self.0, csql.as_ptr(), -1, 4, &mut stmt, &mut tail) };
        assert_eq!(rc, SQLITE_OK, "prepare failed: {sql} — {}", self.errmsg());
        let rc = unsafe { sqlite3_step(stmt) };
        assert!(
            rc == SQLITE_DONE || rc == SQLITE_ROW,
            "step rc={rc} for {sql}: {}",
            self.errmsg()
        );
        unsafe { sqlite3_finalize(stmt) };
    }
    /// prepare + bind text params (1-based, in order) + step once; returns
    /// first row's first column (NULL column or no row → None).
    fn run_bound(&self, sql: &str, params: &[&str]) -> Option<Option<String>> {
        let csql = CString::new(sql).unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe { sqlite3_prepare_v3(self.0, csql.as_ptr(), -1, 4, &mut stmt, &mut tail) };
        assert_eq!(rc, SQLITE_OK, "prepare failed: {sql} — {}", self.errmsg());
        let npc = unsafe { sqlite3_bind_parameter_count(stmt) };
        eprintln!("  [prepare] params={npc} cols={} sql={sql:?}", unsafe {
            sqlite3_column_count(stmt)
        });
        for (i, v) in params.iter().enumerate() {
            let cv = CString::new(*v).unwrap();
            let rc = unsafe { sqlite3_bind_text(stmt, (i + 1) as c_int, cv.as_ptr(), -1, None) };
            assert_eq!(rc, SQLITE_OK);
        }
        let rc = unsafe { sqlite3_step(stmt) };
        let out = if rc == SQLITE_ROW {
            let p = unsafe { sqlite3_column_text(stmt, 0) };
            if p.is_null() {
                Some(None)
            } else {
                Some(Some(
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned(),
                ))
            }
        } else {
            eprintln!("  [step] rc={rc} ({} params bound)", params.len());
            None
        };
        unsafe { sqlite3_finalize(stmt) };
        out
    }
    fn scalar(&self, sql: &str) -> Option<String> {
        self.run_bound(sql, &[]).flatten()
    }
    fn errmsg(&self) -> String {
        unsafe { CStr::from_ptr(sqlite3_errmsg(self.0)) }
            .to_string_lossy()
            .into()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        unsafe { sqlite3_close(self.0) };
    }
}

#[test]
fn mirror_sync_and_correlated_probe() {
    let db = Db::open();

    // ── pdf-tts-shaped schema ──
    db.exec("CREATE TABLE documents (id TEXT PRIMARY KEY, title TEXT, created_at TEXT)");
    db.exec("CREATE TABLE pages (id TEXT PRIMARY KEY, document_id TEXT, page_number INTEGER)");
    db.exec("CREATE TABLE layout_blocks (id TEXT PRIMARY KEY, page_id TEXT, audio_url TEXT, created_at TEXT)");
    db.exec("CREATE TABLE document_audio_versions (id TEXT PRIMARY KEY, document_id TEXT, name TEXT, is_default INTEGER)");
    db.exec(
        "CREATE TABLE layout_block_audio (block_id TEXT NOT NULL, version_id TEXT NOT NULL, \
         audio_url TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (block_id, version_id))",
    );
    // The migration chain adds merged_into via ALTER — the autoindex
    // re-registration path.
    db.exec("ALTER TABLE layout_block_audio ADD COLUMN merged_into TEXT NULL");
    // The real schema's EXPLICIT single-column index on version_id
    // (idx_layout_block_audio_version_id) — the planner's alternative to
    // the composite PK for the subquery's literal/param side.
    db.exec("CREATE INDEX idx_layout_block_audio_version_id ON layout_block_audio (version_id)");

    // ── seeds (the contract test's exact rows) ──
    db.exec("INSERT INTO layout_blocks VALUES ('d1-b0', 'd1-p1', 'audio/d1/d1-b0.opus', 't')");
    db.exec("INSERT INTO layout_blocks VALUES ('d1-b1', 'd1-p1', NULL, 't')");
    db.exec("INSERT INTO layout_block_audio VALUES ('d1-b0', 'ver-a', 'audio/d1/d1-b0.opus', 't', NULL)");
    db.exec("INSERT INTO layout_block_audio VALUES ('d1-b1', 'ver-b', 'audio/d1/ver-b/d1-b1.opus', 't', NULL)");

    // Sanity: point lookups through the C ABI.
    let v = db.scalar(
        "SELECT audio_url FROM layout_block_audio WHERE block_id='d1-b1' AND version_id='ver-b'",
    );
    assert_eq!(v.as_deref(), Some("audio/d1/ver-b/d1-b1.opus"));

    // ── the mirror-sync UPDATE (correlated scalar subquery in SET) ──
    let sync_sql = "UPDATE \"layout_blocks\" SET \"audio_url\" = \
        (SELECT CASE WHEN \"lba\".\"merged_into\" IS NOT NULL THEN NULL ELSE \"lba\".\"audio_url\" END \
         FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = ?1) \
        WHERE \"page_id\" IN (?2)";

    // sync(a): b0 → its ver-a URL; b1 → NULL (no ver-a row).
    let r = db.run_bound(sync_sql, &["ver-a", "d1-p1"]);
    eprintln!("sync(a) step-row: {r:?}");
    let b0 = db.scalar("SELECT audio_url FROM layout_blocks WHERE id='d1-b0'");
    let b1 = db.scalar("SELECT audio_url FROM layout_blocks WHERE id='d1-b1'");
    eprintln!("after sync(a): b0={b0:?} b1={b1:?}");
    assert_eq!(b0.as_deref(), Some("audio/d1/d1-b0.opus"), "sync(a) b0");
    assert_eq!(b1, None, "sync(a) b1 (no ver-a row)");

    // The standalone correlated-subquery SELECT probe.
    let probe = "SELECT (SELECT CASE WHEN \"lba\".\"merged_into\" IS NOT NULL THEN NULL ELSE \"lba\".\"audio_url\" END \
        FROM \"layout_block_audio\" AS \"lba\" \
        WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = 'ver-b') AS x \
        FROM \"layout_blocks\" WHERE \"id\" = 'd1-b1'";
    let x = db.scalar(probe);
    eprintln!("probe: {x:?}");
    assert_eq!(
        x.as_deref(),
        Some("audio/d1/ver-b/d1-b1.opus"),
        "correlated subquery in SELECT projection must find the row"
    );

    // sync(b) with a fresh SQL text (trailing space defeats any cache).
    let r = db.run_bound(&format!("{sync_sql} "), &["ver-b", "d1-p1"]);
    eprintln!("sync(b) step-row: {r:?}");
    let b1 = db.scalar("SELECT audio_url FROM layout_blocks WHERE id='d1-b1'");
    eprintln!("after sync(b): b1={b1:?}");
    assert_eq!(
        b1.as_deref(),
        Some("audio/d1/ver-b/d1-b1.opus"),
        "sync(b) must mirror b1's URL"
    );
}
