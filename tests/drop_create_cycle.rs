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

#[test]
fn drop_reclaims_the_whole_subtree_from_the_live_root() {
    // Two real bugs pinned by sqlite_dbdata's forensic view:
    // 1. DROP freed only the ROOT page — a multi-page table's entire
    //    subtree leaked (orphaned contentful pages, freelist stuck).
    // 2. The freed "root" was the Table struct's CREATE-time root_page —
    //    STALE after a re-rooting insert — so DROP actually freed a
    //    live MID-TREE page (the freelist then handed it out while the
    //    tree still referenced it).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reclaim.db");
    let mut db = rustqlite::Database::open(&path).unwrap();
    db.execute("CREATE TABLE dropme (v TEXT)", []).unwrap();
    {
        let mut sql = String::from("INSERT INTO dropme VALUES ");
        for i in 0..400i64 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("('row number {} padding padding padding')", i));
        }
        db.execute(&sql, []).unwrap(); // re-roots dropme (leaf -> interior)
    }
    db.execute("CREATE TABLE keep (v)", []).unwrap();
    db.execute("INSERT INTO keep VALUES ('k')", []).unwrap();
    let before: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    db.execute("DROP TABLE dropme", []).unwrap();
    let after: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    let free: i64 = db.query("PRAGMA freelist_count", []).unwrap()[0][0].as_integer();
    // dropme spans >= 5 pages (root interior + 4+ leaves) + its schema
    // row; keep and the catalog survive.
    assert!(
        free >= 5,
        "the whole subtree must reach the freelist, got {free} free pages ({before} -> {after})"
    );
    assert_eq!(after, before, "no truncation expected: keep holds the tail");
    let ok = db.query("PRAGMA integrity_check", []).unwrap()[0][0].as_text() == "ok";
    assert!(ok, "integrity after the drop");
    // And the freed pages are reusable: a fresh table takes them without
    // growing the file.
    let grow_before: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    db.execute("CREATE TABLE reuse (a, b, c)", []).unwrap();
    {
        let mut sql = String::from("INSERT INTO reuse VALUES ");
        for i in 0..50i64 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({i}, {i}, {i})"));
        }
        db.execute(&sql, []).unwrap();
    }
    let grow_after: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    assert!(
        grow_after <= grow_before + 2,
        "freelist pages must be reused before the file grows: {grow_before} -> {grow_after}"
    );
    let ok = db.query("PRAGMA integrity_check", []).unwrap()[0][0].as_text() == "ok";
    assert!(ok, "integrity after reuse");
}
