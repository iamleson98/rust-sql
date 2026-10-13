//! Two long-lived handles on one WAL file (SQLite's tkt-313723c356.test
//! shape): the log used to be folded only by a SOLE handle, so while two
//! handles stayed open it grew without bound — every open replayed the
//! whole log (727 ms per open/read/update/close cycle vs SQLite's 4 ms
//! after a few hundred cycles). A checkpoint now folds whenever no handle
//! is inside a statement or a read transaction; idle handles revalidate
//! their view at their next statement.

use rustqlite::{Database, Value};

#[test]
fn wal_stays_bounded_with_two_open_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA page_size = 1024", ()).unwrap();
        db.execute("PRAGMA journal_mode = WAL", ()).unwrap();
        db.execute("CREATE TABLE t1(a, b)", ()).unwrap();
        db.execute("CREATE INDEX i1 ON t1(a, b)", ()).unwrap();
        db.execute(
            "INSERT INTO t1 VALUES(randomblob(400), randomblob(400))",
            (),
        )
        .unwrap();
        for _ in 0..8 {
            db.execute(
                "INSERT INTO t1 SELECT randomblob(400), randomblob(400) FROM t1",
                (),
            )
            .unwrap();
        }
    }
    let wal = dir.path().join("t.db-wal");
    let mut max_wal = 0u64;
    // One long-lived handle, the other opening / writing / closing.
    let db = Database::open(&path).unwrap();
    for i in 0..40 {
        assert_eq!(
            db.query("SELECT count(*) FROM t1", ()).unwrap()[0][0],
            Value::Integer(256)
        );
        let len = 399 - (i % 2);
        let mut db2 = Database::open(&path).unwrap();
        db2.execute(&format!("UPDATE t1 SET a = randomblob({len})"), ())
            .unwrap();
        drop(db2);
        // The long-lived handle sees the other one's committed update.
        let r = db
            .query("SELECT min(length(a)), max(length(a)) FROM t1", ())
            .unwrap();
        assert_eq!(r[0], vec![Value::Integer(len), Value::Integer(len)]);
        assert_eq!(
            format!("{:?}", db.query("PRAGMA integrity_check", ()).unwrap()),
            "[[Text(\"ok\")]]"
        );
        max_wal = max_wal.max(std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0));
    }
    // 40 updates x ~1.5 MB of frames each would be ~60 MB unfolded.
    assert!(max_wal < 8 << 20, "WAL grew to {max_wal} bytes");
}

/// SQLite's tkt-313723c356.test through the Tcl harness: a handle that
/// reads only through `query_with_columns` (the column-named surface)
/// skipped the cross-process read scope, so after a second handle's
/// close-time fold rewrote the main file and removed the log, it kept
/// reading pages through its view of the removed log — "index i1 tree
/// is corrupt: failed to fill whole buffer" on a perfectly good file.
#[test]
fn query_with_columns_sees_another_handles_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let saved = dir.path().join("saved");
    std::fs::create_dir(&saved).unwrap();
    {
        let mut db = Database::open(&path).unwrap();
        for s in [
            "PRAGMA page_size = 1024",
            "PRAGMA journal_mode = WAL",
            "CREATE TABLE t1(a, b)",
            "CREATE INDEX i1 ON t1(a, b)",
            "INSERT INTO t1 VALUES(randomblob(400), randomblob(400))",
        ] {
            db.execute(s, ()).unwrap();
        }
        for _ in 0..4 {
            db.execute(
                "INSERT INTO t1 SELECT randomblob(400), randomblob(400) FROM t1",
                (),
            )
            .unwrap();
        }
        // Snapshot while open: the committed rows live in the log.
        for e in std::fs::read_dir(dir.path()).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_file() {
                std::fs::copy(e.path(), saved.join(e.file_name())).unwrap();
            }
        }
    }
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            std::fs::remove_file(e.path()).unwrap();
        }
    }
    for e in std::fs::read_dir(&saved).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), dir.path().join(e.file_name())).unwrap();
    }
    let db = Database::open(&path).unwrap();
    let mut db2 = Database::open(&path).unwrap();
    db.query_with_columns("SELECT * FROM t1", ()).unwrap();
    db2.execute("UPDATE t1 SET a = randomblob(399)", ())
        .unwrap();
    drop(db2);
    let (_, r) = db
        .query_with_columns("SELECT count(*), min(length(a)) FROM t1", ())
        .unwrap();
    assert_eq!(r[0], vec![Value::Integer(16), Value::Integer(399)]);
    let (_, r) = db.query_with_columns("PRAGMA integrity_check", ()).unwrap();
    assert_eq!(format!("{r:?}"), "[[Text(\"ok\")]]");
}
