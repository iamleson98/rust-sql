//! WAL commit frame accounting: a commit logs the pages it changed — the
//! header page (page 0) only when its header bytes changed. An unchanged
//! header frame on every single-row autocommit doubled the WAL traffic,
//! so the 1000-frame auto-checkpoint (and its syncs) fired twice as often.
//! Crash recovery of header-free commits is covered by
//! `cross_process_locking::killed_writer_recovers_header_free_commits`.

use rustqlite::{Database, Value};

/// The WAL's frame count, read as `PRAGMA wal_checkpoint`'s `log` column
/// (the checkpoint then resets the log to 0).
fn log_frames_then_checkpoint(db: &mut Database) -> i64 {
    db.query("PRAGMA wal_checkpoint", []).unwrap()[0][1].as_integer()
}

#[test]
fn header_page_is_logged_only_when_it_changes() {
    let dir = std::env::temp_dir().join(format!(
        "rsql-walframes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode=WAL", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (v) VALUES (1), (2), (3)", [])
            .unwrap();
        log_frames_then_checkpoint(&mut db);
        // In-place single-row UPDATEs: one leaf frame per commit.
        for id in 1..=3 {
            db.execute("UPDATE t SET v = v + 10 WHERE id = ?", [Value::Integer(id)])
                .unwrap();
        }
        assert_eq!(log_frames_then_checkpoint(&mut db), 3);
        // A schema change moves the header (schema cookie, page count):
        // its commit logs page 0 as well.
        db.execute("CREATE TABLE u (a)", []).unwrap();
        assert!(log_frames_then_checkpoint(&mut db) >= 2);
        let sum = db.query("SELECT SUM(v) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(sum, 36);
    }
    // A clean reopen reads the same state.
    let db = Database::open(&path).unwrap();
    let sum = db.query("SELECT SUM(v) FROM t", []).unwrap()[0][0].as_integer();
    assert_eq!(sum, 36);
    let n = db
        .query("SELECT COUNT(*) FROM sqlite_master WHERE name = 'u'", [])
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(n, 1);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
