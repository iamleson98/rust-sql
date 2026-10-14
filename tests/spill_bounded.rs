//! A long write transaction that keeps evicting and re-reading the same
//! pages (SQLite's savepoint6.test: one savepoint-held transaction over
//! thousands of random writes under a small page cache) must keep its
//! spill bounded by the pages it holds — each re-spill of a page used to
//! append a fresh record, and the DELETE-mode `-spill` sidecar grew past
//! 7 GB for a 1 MB database.

use rustqlite::Database;

fn churn(db: &mut Database) {
    db.execute("PRAGMA cache_size = 10", ()).unwrap();
    db.execute("CREATE TABLE t1(x UNIQUE, y)", ()).unwrap();
    db.execute("CREATE INDEX i2 ON t1(y)", ()).unwrap();
    db.execute("BEGIN", ()).unwrap();
    let mut seed: u64 = 7;
    for _ in 0..20_000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let x = (seed >> 33) % 1000;
        if seed & 1 == 0 {
            db.execute(
                &format!(
                    "INSERT OR REPLACE INTO t1 VALUES({x}, printf('%.*c', 300 + {x} % 200, 'y'))"
                ),
                (),
            )
            .unwrap();
        } else {
            db.execute(&format!("DELETE FROM t1 WHERE x = {x}"), ())
                .unwrap();
        }
    }
}

fn file_len(p: &std::path::Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

#[test]
fn delete_mode_spill_stays_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    churn(&mut db);
    let spill = file_len(&dir.path().join("t.db-spill"));
    assert!(spill < 16 << 20, "spill grew to {spill} bytes");
    db.execute("COMMIT", ()).unwrap();
    let r = db.query("PRAGMA integrity_check", ()).unwrap();
    assert_eq!(format!("{r:?}"), "[[Text(\"ok\")]]");
}

#[test]
fn wal_mode_spill_stays_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", ()).unwrap();
    churn(&mut db);
    let wal = file_len(&dir.path().join("t.db-wal"));
    eprintln!("wal mid-transaction: {wal} bytes");
    assert!(wal < 64 << 20, "WAL grew to {wal} bytes mid-transaction");
    db.execute("COMMIT", ()).unwrap();
    let r = db.query("PRAGMA integrity_check", ()).unwrap();
    assert_eq!(format!("{r:?}"), "[[Text(\"ok\")]]");
    let live = db
        .query("SELECT count(*), sum(x), sum(length(y)) FROM t1", ())
        .unwrap();
    // Crash image: the frames rewritten in place were re-chained before
    // the commit frame, so recovery replays the whole transaction.
    let crash = dir.path().join("crash");
    std::fs::create_dir(&crash).unwrap();
    for f in ["t.db", "t.db-wal"] {
        std::fs::copy(dir.path().join(f), crash.join(f)).unwrap();
    }
    let rec = Database::open(crash.join("t.db")).unwrap();
    assert_eq!(
        rec.query("SELECT count(*), sum(x), sum(length(y)) FROM t1", ())
            .unwrap(),
        live
    );
    let r = rec.query("PRAGMA integrity_check", ()).unwrap();
    assert_eq!(format!("{r:?}"), "[[Text(\"ok\")]]");
    std::mem::forget(db);
}
