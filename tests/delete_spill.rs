//! DELETE-mode mid-transaction page spill (the `-spill` sidecar): bounded
//! RSS for big write transactions. Dirty pages evicted under cache
//! pressure mid-txn go to the sidecar (the main file keeps pre-BEGIN
//! bytes); COMMIT drains them into the main file; ROLLBACK discards
//! them; get_page misses re-read the newest uncommitted version.
//!
//! The tests pin `PRAGMA cache_size` to a handful of pages so even small
//! transactions force spills.

use rustqlite::{Database, Value};
use std::path::PathBuf;

fn tmp_db(name: &str) -> (Database, PathBuf) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("rustqlite-spill-{name}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-spill", path.display()));
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let mut db = Database::open(&path).expect("open");
    db.execute("PRAGMA cache_size = 4", []).expect("cache_size");
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)",
        [],
    )
    .expect("create");
    (db, path)
}

fn insert_rows(db: &mut Database, lo: i64, hi: i64) {
    for i in lo..=hi {
        db.execute(
            "INSERT INTO t (a, b, c) VALUES (?, ?, ?)",
            [
                Value::Integer(i * 3),
                Value::Real(i as f64 / 7.0),
                Value::Text(format!("payload-{i:06}").into()),
            ],
        )
        .expect("insert");
    }
}

fn count(db: &Database) -> i64 {
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    match rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("bad count: {other:?}"),
    }
}

fn spill_exists(path: &std::path::Path) -> bool {
    std::path::Path::new(&format!("{}-spill", path.display())).exists()
}

/// COMMIT drains the spill into the main file; the reopened database
/// sees every row; the sidecar is gone.
#[test]
fn spill_commit_roundtrip() {
    let (mut db, path) = tmp_db("commit");
    db.execute("BEGIN", []).unwrap();
    insert_rows(&mut db, 1, 3000);
    db.execute("COMMIT", []).unwrap();
    assert_eq!(count(&db), 3000);
    drop(db);
    assert!(!spill_exists(&path), "sidecar must be dropped at close");
    let db = Database::open(&path).expect("reopen");
    assert_eq!(count(&db), 3000);
    // Spot-check rows that certainly lived on spilled pages (early and
    // late in the key range).
    for id in [1i64, 750, 1500, 3000] {
        let rows = db
            .query("SELECT c FROM t WHERE id = ?", [Value::Integer(id)])
            .unwrap();
        assert_eq!(
            rows.first().and_then(|r| r.first()).cloned(),
            Some(Value::Text(format!("payload-{id:06}").into())),
            "row {id} lost after spill drain"
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// ROLLBACK discards the spill: the main file was never touched
/// mid-txn, so the pre-BEGIN state is intact with zero rows.
#[test]
fn spill_rollback_discards() {
    let (mut db, path) = tmp_db("rollback");
    insert_rows(&mut db, 1, 50); // pre-BEGIN state (committed)
    db.execute("BEGIN", []).unwrap();
    insert_rows(&mut db, 51, 3050); // spills under cache_size=4
    assert_eq!(count(&db), 3050, "own-writes visible inside the txn");
    db.execute("ROLLBACK", []).unwrap();
    assert_eq!(count(&db), 50, "rollback must discard spilled work");
    drop(db);
    let db = Database::open(&path).expect("reopen");
    assert_eq!(count(&db), 50, "file still holds the pre-BEGIN state");
    let _ = std::fs::remove_file(&path);
}

/// Reading spilled pages back INSIDE the transaction (the get_page
/// miss → spill-hit → re-cache-dirty path) must serve the uncommitted
/// newest version, not the (stale) main-file bytes.
#[test]
fn spill_unspill_readback_mid_txn() {
    let (mut db, path) = tmp_db("readback");
    db.execute("BEGIN", []).unwrap();
    insert_rows(&mut db, 1, 2000); // spills
                                   // Re-read the whole range: every row must come back with its
                                   // uncommitted value (the main file still has no pages for most of
                                   // these ids — a spill miss would read zeros or error).
    let rows = db
        .query(
            "SELECT id, c FROM t WHERE id BETWEEN 1 AND 2000 ORDER BY id",
            [],
        )
        .expect("mid-txn scan");
    assert_eq!(rows.len(), 2000);
    for (i, row) in rows.iter().enumerate() {
        let id = i as i64 + 1;
        assert_eq!(row[0], Value::Integer(id));
        assert_eq!(
            row[1],
            Value::Text(format!("payload-{id:06}").into()),
            "row {id} served stale/zero bytes from the spill-miss path"
        );
    }
    // UPDATE a definitely-spilled page, spill again (cache pressure),
    // re-read: newest version wins.
    db.execute("UPDATE t SET c = 'updated' WHERE id < 500", [])
        .expect("update spilled pages");
    let rows = db
        .query("SELECT c FROM t WHERE id < 500", [])
        .expect("re-read");
    assert_eq!(rows.len(), 499);
    assert!(
        rows.iter().all(|r| r[0] == Value::Text("updated".into())),
        "post-update spill re-read must serve the newest version"
    );
    db.execute("COMMIT", []).unwrap();
    drop(db);
    let db = Database::open(&path).expect("reopen");
    let rows = db.query("SELECT c FROM t WHERE id < 500", []).unwrap();
    assert!(rows.iter().all(|r| r[0] == Value::Text("updated".into())));
    let _ = std::fs::remove_file(&path);
}

/// A leftover `-spill` sidecar (crash mid-txn) is uncommitted garbage:
/// open deletes it and serves the committed main file.
#[test]
fn spill_stale_sidecar_removed_at_open() {
    let (mut db, path) = tmp_db("stale");
    insert_rows(&mut db, 1, 100);
    drop(db);
    // Forge a stale sidecar as a crashed transaction would leave it.
    let spill = format!("{}-spill", path.display());
    std::fs::write(&spill, vec![0u8; 8192]).expect("forge sidecar");
    let db = Database::open(&path).expect("open with stale sidecar");
    assert_eq!(count(&db), 100, "committed state untouched");
    assert!(!spill_exists(&path), "stale sidecar deleted at open");
    let _ = std::fs::remove_file(&path);
}

/// Two sequential spill transactions: the sidecar is reset (not
/// appended forever) and the second txn's drain only writes its own
/// pages.
#[test]
fn spill_two_txns_second_drains_cleanly() {
    let (mut db, path) = tmp_db("two-txns");
    db.execute("BEGIN", []).unwrap();
    insert_rows(&mut db, 1, 1500);
    db.execute("COMMIT", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    insert_rows(&mut db, 1501, 3000);
    db.execute("COMMIT", []).unwrap();
    assert_eq!(count(&db), 3000);
    drop(db);
    let db = Database::open(&path).expect("reopen");
    assert_eq!(count(&db), 3000);
    for id in [1i64, 1500, 1501, 3000] {
        let rows = db
            .query("SELECT a FROM t WHERE id = ?", [Value::Integer(id)])
            .unwrap();
        assert_eq!(rows.len(), 1, "row {id}");
    }
    let _ = std::fs::remove_file(&path);
}

/// SAVEPOINT rollback inside a spilling transaction: the savepoint's
/// pre-images restore in-cache and the spilled records of rolled-back
/// work die with the savepoint.
#[test]
fn spill_savepoint_rollback() {
    let (mut db, path) = tmp_db("savepoint");
    db.execute("BEGIN", []).unwrap();
    insert_rows(&mut db, 1, 800);
    db.execute("SAVEPOINT half", []).unwrap();
    insert_rows(&mut db, 801, 2200); // spills under pressure
    assert_eq!(count(&db), 2200);
    db.execute("ROLLBACK TO half", []).unwrap();
    db.execute("RELEASE half", []).unwrap();
    let probe = db
        .query("SELECT id FROM t ORDER BY id LIMIT 1", [])
        .unwrap();
    eprintln!("probe row: {:?}", probe.first().map(|r| r.first().cloned()));
    assert_eq!(count(&db), 800, "savepoint rollback discards spilled rows");
    db.execute("COMMIT", []).unwrap();
    drop(db);
    let db = Database::open(&path).expect("reopen");
    assert_eq!(count(&db), 800);
    let _ = std::fs::remove_file(&path);
}
