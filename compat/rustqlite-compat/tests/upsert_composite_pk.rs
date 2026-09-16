//! `ALTER TABLE ADD COLUMN` must not orphan the table's indexes — the
//! implicit PK/UNIQUE autoindexes and explicit CREATE INDEX entries
//! alike — from the catalog.
//!
//! Regression (found via pdf-tts's sea-orm 1.1 migration chain):
//! `layout_block_audio` has `PRIMARY KEY (block_id, version_id)`; a later
//! migration adds `merged_into` via ALTER TABLE ADD COLUMN; after that,
//! every `INSERT … ON CONFLICT ("block_id", "version_id") DO UPDATE …`
//! failed with "ON CONFLICT clause does not match any PRIMARY KEY or
//! UNIQUE constraint" because the catalog had forgotten the implicit
//! autoindex until a reopen. Index maintenance on subsequent writes
//! silently stopped for the same reason.
//!
//! These drive the exact `sqlite3_*` call sequence sea-orm generates.

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
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;

struct Db(*mut compat::sqlite3);

impl Db {
    fn open() -> Db {
        let name = CString::new(":memory:").unwrap();
        let mut db: *mut compat::sqlite3 = ptr::null_mut();
        let rc = unsafe {
            sqlite3_open_v2(
                name.as_ptr(),
                &mut db,
                SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
                ptr::null(),
            )
        };
        assert_eq!(rc, SQLITE_OK);
        Db(db)
    }

    /// Prepare, step once, finalize. Returns (step rc, errmsg).
    fn exec(&self, sql: &str) -> (c_int, String) {
        let csql = CString::new(sql).unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe { sqlite3_prepare_v3(self.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
        if rc != SQLITE_OK {
            return (rc, self.errmsg());
        }
        let rc = unsafe { sqlite3_step(stmt) };
        unsafe { sqlite3_finalize(stmt) };
        (rc, self.errmsg())
    }

    fn errmsg(&self) -> String {
        unsafe {
            CStr::from_ptr(sqlite3_errmsg(self.0))
                .to_string_lossy()
                .into()
        }
    }

    /// First row's first column as text.
    fn query_one(&self, sql: &str) -> Option<String> {
        let csql = CString::new(sql).unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe { sqlite3_prepare_v3(self.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
        if rc != SQLITE_OK {
            return None;
        }
        let rc = unsafe { sqlite3_step(stmt) };
        let out = if rc == SQLITE_ROW {
            Some(
                unsafe { CStr::from_ptr(sqlite3_column_text(stmt, 0)) }
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            None
        };
        unsafe { sqlite3_finalize(stmt) };
        out
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        unsafe { sqlite3_close(self.0) };
    }
}

const CREATE: &str = "CREATE TABLE layout_block_audio (\
    block_id TEXT NOT NULL, \
    version_id TEXT NOT NULL, \
    audio_url TEXT NOT NULL, \
    created_at TEXT NOT NULL, \
    PRIMARY KEY (block_id, version_id))";

/// The exact SQL shape sea-orm 1.1 generates for
/// `OnConflict::columns([BlockId, VersionId]).update_columns(...)`.
const UPSERT_SEA_ORM: &str = "INSERT INTO \"layout_block_audio\" \
    (\"block_id\", \"version_id\", \"audio_url\", \"created_at\") \
    VALUES (?1, ?2, ?3, ?4) \
    ON CONFLICT (\"block_id\", \"version_id\") DO UPDATE SET \
    \"audio_url\" = \"excluded\".\"audio_url\"";

#[test]
fn add_column_keeps_composite_pk_upsert_working() {
    let db = Db::open();
    let (rc, msg) = db.exec(CREATE);
    assert_eq!(rc, SQLITE_DONE, "create: {msg}");

    // The migration-chain shape: a LATER migration adds a column.
    let (rc, msg) = db.exec("ALTER TABLE layout_block_audio ADD COLUMN merged_into TEXT NULL");
    assert_eq!(rc, SQLITE_DONE, "alter: {msg}");

    // Upsert with BOUND parameters through the prepared path — the
    // unconflicted insert first (target resolution happens on EVERY
    // insert, not just conflicts).
    let csql = CString::new(UPSERT_SEA_ORM).unwrap();
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe { sqlite3_prepare_v3(db.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
    assert_eq!(rc, SQLITE_OK, "prepare: {}", db.errmsg());
    for (i, v) in ["b1", "v1", "u1", "t"].iter().enumerate() {
        let cv = CString::new(*v).unwrap();
        let brc = unsafe { sqlite3_bind_text(stmt, (i + 1) as c_int, cv.as_ptr(), -1, None) };
        assert_eq!(brc, SQLITE_OK);
    }
    let rc = unsafe { sqlite3_step(stmt) };
    assert_eq!(rc, SQLITE_DONE, "unconflicted upsert: {}", db.errmsg());
    unsafe { sqlite3_finalize(stmt) };

    // The conflicting insert: DO UPDATE must fire and rewrite audio_url.
    let csql = CString::new(UPSERT_SEA_ORM).unwrap();
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe { sqlite3_prepare_v3(db.0, csql.as_ptr(), -1, 0, &mut stmt, &mut tail) };
    assert_eq!(rc, SQLITE_OK);
    for (i, v) in ["b1", "v1", "u2", "t"].iter().enumerate() {
        let cv = CString::new(*v).unwrap();
        unsafe { sqlite3_bind_text(stmt, (i + 1) as c_int, cv.as_ptr(), -1, None) };
    }
    let rc = unsafe { sqlite3_step(stmt) };
    assert_eq!(rc, SQLITE_DONE, "conflicted upsert: {}", db.errmsg());
    unsafe { sqlite3_finalize(stmt) };

    assert_eq!(
        db.query_one("SELECT audio_url FROM layout_block_audio"),
        Some("u2".to_string()),
        "DO UPDATE must rewrite the conflicting row"
    );

    // Duplicate PK enforcement must survive the ALTER too.
    let (rc, msg) = db.exec("INSERT INTO layout_block_audio VALUES ('b1','v1','dup',NULL,'t')");
    assert_ne!(rc, SQLITE_DONE, "duplicate PK must be rejected ({msg})");
}

#[test]
fn add_column_keeps_explicit_indexes_maintained() {
    let db = Db::open();
    let (rc, msg) = db.exec(CREATE);
    assert_eq!(rc, SQLITE_DONE, "create: {msg}");
    let (rc, msg) = db.exec("CREATE INDEX idx_lba_created ON layout_block_audio (created_at)");
    assert_eq!(rc, SQLITE_DONE, "index: {msg}");
    let (rc, msg) = db.exec("ALTER TABLE layout_block_audio ADD COLUMN merged_into TEXT NULL");
    assert_eq!(rc, SQLITE_DONE, "alter: {msg}");

    // Writes after the ALTER must keep the explicit index maintained:
    // an index-driven lookup must find the row.
    let (rc, msg) = db.exec("INSERT INTO layout_block_audio VALUES ('b1','v1','u1','t1',NULL)");
    assert_eq!(rc, SQLITE_DONE, "insert: {msg}");
    assert_eq!(
        db.query_one("SELECT block_id FROM layout_block_audio WHERE created_at = 't1'"),
        Some("b1".to_string()),
        "explicit index lookup must see post-ALTER writes"
    );
}
