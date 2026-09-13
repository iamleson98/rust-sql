//! Regression: DROP TABLE followed by CREATE TABLE of the SAME name
//! must not corrupt scans of the recreated table (freed-page reuse
//! through the implicit-PK-index path). Found by the app-side
//! `seaql_migrations.applied_at` schema repair (v0.5.1 deploy failure).
use rustqlite::{Database, Value};

#[test]
fn drop_then_recreate_same_name_scans_cleanly() {
    let mut db = Database::open_in_memory().unwrap();
    // Shape mirrors seaql_migrations: TEXT PRIMARY KEY (implicit
    // unique index) + a second column.
    db.execute(
        "CREATE TABLE t (version TEXT PRIMARY KEY NOT NULL, applied_at TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t (version, applied_at) VALUES ('m1', '1787000000')",
        [],
    )
    .unwrap();

    // The staged rebuild shape: stage, drop, recreate, copy back, drop stage.
    db.execute(
        "CREATE TABLE t_stage (version TEXT PRIMARY KEY NOT NULL, applied_at INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t_stage SELECT version, CAST(applied_at AS INTEGER) FROM t",
        [],
    )
    .unwrap();
    db.execute("DROP TABLE t", []).unwrap();
    db.execute(
        "CREATE TABLE t (version TEXT PRIMARY KEY NOT NULL, applied_at INTEGER)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t SELECT version, applied_at FROM t_stage", [])
        .unwrap();
    db.execute("DROP TABLE t_stage", []).unwrap();

    // The scan that corrupted: full table read of the recreated table.
    let rows = db.query("SELECT version, applied_at FROM t", []).unwrap();
    assert_eq!(rows.len(), 1, "row survived: {rows:?}");
    assert_eq!(rows[0][0], Value::Text("m1".into()));
    assert_eq!(rows[0][1], Value::Integer(1_787_000_000));

    // Reopen-style stability: new inserts still work.
    db.execute("INSERT INTO t (version, applied_at) VALUES ('m2', 42)", [])
        .unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(2));
}

/// The simpler shape: drop + recreate WITHOUT the staging table.
#[test]
fn drop_then_recreate_direct_scans_cleanly() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (version TEXT PRIMARY KEY NOT NULL, applied_at TEXT)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES ('m1', '123')", [])
        .unwrap();
    db.execute("DROP TABLE t", []).unwrap();
    db.execute(
        "CREATE TABLE t (version TEXT PRIMARY KEY NOT NULL, applied_at INTEGER)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES ('m1', 123)", []).unwrap();
    let rows = db.query("SELECT version, applied_at FROM t", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Integer(123));
}
