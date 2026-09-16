//! Correlated scalar subquery in UPDATE SET through the C ABI prepared
//! path — the sea-orm mirror-sync shape from pdf-tts:
//!
//! ```sql
//! UPDATE "layout_blocks" SET "audio_url" =
//!   (SELECT CASE WHEN "lba"."merged_into" IS NOT NULL THEN NULL
//!    ELSE "lba"."audio_url" END
//!    FROM "layout_block_audio" AS "lba"
//!    WHERE "lba"."block_id" = "layout_blocks"."id"
//!      AND "lba"."version_id" = ?1)
//! WHERE "page_id" IN (?2)
//! ```
//!
//! with ALL parameters bound through sqlite3_bind_*. Pins the engine's
//! alias-qualified index-lookup output columns (tests/correlated_index_leak.rs
//! pins the engine side; this file pins the same contract through the C ABI
//! the sqlx/sea-orm stack drives).

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
    fn sqlite3_changes(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_column_text(stmt: *mut compat::sqlite3_stmt, col: c_int) -> *const c_char;
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
        assert_eq!(rc, SQLITE_OK, "prepare failed: {sql}");
        let rc = unsafe { sqlite3_step(stmt) };
        assert!(rc == SQLITE_DONE || rc == SQLITE_ROW, "step rc={rc}: {sql}");
        unsafe { sqlite3_finalize(stmt) };
    }
    /// prepare + bind (1-based) + step once; first column as text.
    fn run_bound(&self, sql: &str, params: &[&str]) -> Option<Option<String>> {
        let csql = CString::new(sql).unwrap();
        let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let rc = unsafe { sqlite3_prepare_v3(self.0, csql.as_ptr(), -1, 4, &mut stmt, &mut tail) };
        assert_eq!(rc, SQLITE_OK, "prepare failed: {sql}");
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
            None
        };
        unsafe { sqlite3_finalize(stmt) };
        out
    }
    fn scalar(&self, sql: &str) -> Option<String> {
        self.run_bound(sql, &[]).flatten()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        unsafe { sqlite3_close(self.0) };
    }
}

#[test]
fn update_correlated_subquery_bound_params() {
    let db = Db::open();

    // pdf-tts-shaped schema: composite TEXT PK + ALTER ADD COLUMN + an
    // explicit single-column index (the planner's IndexLookup trigger).
    db.exec("CREATE TABLE layout_blocks (id TEXT PRIMARY KEY, page_id TEXT, audio_url TEXT, created_at TEXT)");
    db.exec(
        "CREATE TABLE layout_block_audio (block_id TEXT NOT NULL, version_id TEXT NOT NULL, \
         audio_url TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY (block_id, version_id))",
    );
    db.exec("ALTER TABLE layout_block_audio ADD COLUMN merged_into TEXT NULL");
    db.exec("CREATE INDEX idx_layout_block_audio_version_id ON layout_block_audio (version_id)");

    db.exec("INSERT INTO layout_blocks VALUES ('b1','p1',NULL,'t')");
    db.exec("INSERT INTO layout_blocks VALUES ('b2','p1','stale','t')");
    db.exec("INSERT INTO layout_block_audio VALUES ('b1','vb','url-b','t',NULL)");
    db.exec("INSERT INTO layout_block_audio VALUES ('b2','vb','url-b2','t',NULL)");

    let sync_sql = "UPDATE \"layout_blocks\" SET \"audio_url\" = \
        (SELECT CASE WHEN \"lba\".\"merged_into\" IS NOT NULL THEN NULL ELSE \"lba\".\"audio_url\" END \
         FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = ?1) \
        WHERE \"page_id\" IN (?2)";
    let r = db.run_bound(sync_sql, &["vb", "p1"]);
    eprintln!("sync step: {r:?} changes={}", {
        // SAFETY: db handle is alive.
        unsafe { sqlite3_changes(db.0) }
    });

    // The correlated subquery must read lba.audio_url for EACH matching
    // outer row — NOT leak the outer layout_blocks.audio_url (NULL /
    // 'stale'), which was the regression.
    let b1 = db.scalar("SELECT audio_url FROM layout_blocks WHERE id='b1'");
    let b2 = db.scalar("SELECT audio_url FROM layout_blocks WHERE id='b2'");
    assert_eq!(b1.as_deref(), Some("url-b"), "b1 must mirror its lba row");
    assert_eq!(b2.as_deref(), Some("url-b2"), "b2 must mirror its lba row");

    // And the correlated SELECT probe (subquery in a SELECT projection).
    let x = db.scalar(
        "SELECT (SELECT CASE WHEN \"lba\".\"merged_into\" IS NOT NULL THEN NULL ELSE \"lba\".\"audio_url\" END \
         FROM \"layout_block_audio\" AS \"lba\" \
         WHERE \"lba\".\"block_id\" = \"layout_blocks\".\"id\" AND \"lba\".\"version_id\" = 'vb') \
         FROM \"layout_blocks\" WHERE \"id\" = 'b2'",
    );
    assert_eq!(x.as_deref(), Some("url-b2"), "correlated SELECT probe");
}
