//! WAL mode tests: commits, WAL-served reads, crash recovery, checkpoint,
//! rollback, and pragma surface.
use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("{}.db", name));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    let db = Database::open(&path).unwrap();
    (db, path)
}

fn cleanup(path: &std::path::PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}

#[test]
fn wal_basic_commit_and_read() {
    let (mut db, path) = tmpdb("wal_basic");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    assert_eq!(
        db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(),
        "wal"
    );
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('a'), ('b'), ('c')", []).unwrap();

    // Reads see WAL-committed data through the page map.
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 3);
    let rows = db.query("SELECT v FROM t WHERE id = 2", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "b");
    drop(db);

    // Clean close checkpointed the WAL; reopen sees everything, no -wal.
    let db = Database::open(&path).unwrap();
    assert_eq!(db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(), "delete");
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 3);
    cleanup(&path);
}

#[test]
fn wal_crash_recovery_unclean_shutdown() {
    let (mut db, path) = tmpdb("wal_crash");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=500i64 {
        db.execute("INSERT INTO t (v) VALUES (?)", [Value::Integer(i)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Simulate a crash: forget the Database — its Drop (checkpoint + WAL
    // removal) never runs. The -wal file with committed frames remains.
    let rows_before = db.query("SELECT COUNT(*), SUM(v) FROM t", []).unwrap();
    std::mem::forget(db);

    assert!(std::path::PathBuf::from(format!("{}-wal", path.to_str().unwrap())).exists(),
        "WAL file must survive the simulated crash");

    // Recovery: committed frames are served from the WAL on reopen.
    let mut db2 = Database::open(&path).unwrap();
    let rows_after = db2.query("SELECT COUNT(*), SUM(v) FROM t", []).unwrap();
    assert_eq!(rows_before[0][0].as_integer(), rows_after[0][0].as_integer());
    assert_eq!(rows_before[0][1].as_integer(), rows_after[0][1].as_integer());
    assert_eq!(rows_after[0][0].as_integer(), 500);

    // The recovered pager is in WAL mode (journal state persisted).
    assert_eq!(db2.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(), "wal");

    // And it accepts new writes on top of the recovered state.
    db2.execute("INSERT INTO t (v) VALUES (999)", []).unwrap();
    let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 501);
    drop(db2);
    cleanup(&path);
}

#[test]
fn wal_served_reads_after_reopen_without_checkpoint() {
    // Even without auto-checkpoint, reads resolve through the WAL map.
    let (mut db, path) = tmpdb("wal_served");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('x')", []).unwrap();
    let sum_before = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    std::mem::forget(db);
    assert_eq!(sum_before[0][0].as_integer(), 1);

    let mut db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "x");

    // Explicit checkpoint: pages copied back, WAL reset; data intact.
    db2.execute("PRAGMA wal_checkpoint", []).unwrap();
    let rows = db2.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "x");
    drop(db2);
    cleanup(&path);
}

#[test]
fn wal_checkpoint_persists_to_main_file() {
    let (mut db, path) = tmpdb("wal_ckpt");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=200i64 {
        db.execute("INSERT INTO t (v) VALUES (?)", [Value::Integer(i * 3)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("PRAGMA wal_checkpoint", []).unwrap();

    // After the checkpoint, a reopen (WAL deleted on clean drop) reads the
    // main file alone.
    drop(db);
    let db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT COUNT(*), SUM(v) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 200);
    let expected: i64 = (1..=200).map(|i| i * 3).sum();
    assert_eq!(rows[0][1].as_integer(), expected);
    cleanup(&path);
}

#[test]
fn wal_rollback_discards_uncommitted() {
    let (mut db, path) = tmpdb("wal_rollback");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES (1)", []).unwrap();

    db.execute("BEGIN", []).unwrap();
    for i in 2..=100i64 {
        db.execute("INSERT INTO t (v) VALUES (?)", [Value::Integer(i)]).unwrap();
    }
    db.execute("UPDATE t SET v = v * 100 WHERE id = 1", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();

    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    let rows = db.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);

    // WAL-mode commits still work after a rollback.
    db.execute("INSERT INTO t (v) VALUES (42)", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    cleanup(&path);
}

#[test]
fn wal_auto_checkpoint_under_churn() {
    // Enough commits to cross the 1000-frame auto-checkpoint threshold:
    // the WAL resets and everything stays readable.
    let (mut db, path) = tmpdb("wal_autockpt");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    for batch in 0..80 {
        db.execute("BEGIN", []).unwrap();
        for i in 1..=40i64 {
            db.execute(
                "INSERT INTO t (v) VALUES (?)",
                [Value::Text(format!("b{batch}r{i}").into())],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 80 * 40);
    let rows = db.query("SELECT v FROM t WHERE id = 3200", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "b79r40");
    drop(db);

    let db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 80 * 40);
    cleanup(&path);
}

#[test]
fn wal_synchronous_normal_and_level_reads() {
    let (mut db, path) = tmpdb("wal_sync");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    assert_eq!(db.query("PRAGMA synchronous", []).unwrap()[0][0].as_integer(), 2);
    db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    assert_eq!(db.query("PRAGMA synchronous", []).unwrap()[0][0].as_integer(), 1);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=50i64 {
        db.execute("INSERT INTO t (v) VALUES (?)", [Value::Integer(i)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 50);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 50);
    cleanup(&path);
}

#[test]
fn wal_switch_back_to_delete() {
    let (mut db, path) = tmpdb("wal_switch");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES (7)", []).unwrap();

    // Switching back checkpoints + removes the WAL.
    db.execute("PRAGMA journal_mode = DELETE", []).unwrap();
    assert_eq!(db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(), "delete");
    let rows = db.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 7);
    drop(db);

    let mut db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 7);
    // And new writes in delete mode still work.
    db2.execute("INSERT INTO t (v) VALUES (8)", []).unwrap();
    let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    cleanup(&path);
}

#[test]
fn wal_indexes_and_dml_roundtrip() {
    // Secondary indexes, UPDATE and DELETE all maintain the WAL view.
    let (mut db, path) = tmpdb("wal_dml");
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v TEXT)", []).unwrap();
    db.execute("CREATE INDEX idx_k ON t(k)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=300i64 {
        db.execute("INSERT INTO t (k, v) VALUES (?, ?)",
            [Value::Integer(i % 10), Value::Text(format!("v{i}").into())]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Index-served point lookups see WAL pages.
    let rows = db.query("SELECT COUNT(*) FROM t WHERE k = 3", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 30);

    db.execute("UPDATE t SET v = 'upd' WHERE k = 3", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t WHERE v = 'upd'", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 30);

    db.execute("DELETE FROM t WHERE k = 3", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 270);

    std::mem::forget(db);
    let db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 270);
    let rows = db2.query("SELECT COUNT(*) FROM t WHERE k = 3", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
    drop(db2);
    cleanup(&path);
}
