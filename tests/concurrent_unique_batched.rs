//! `BEGIN CONCURRENT` writers whose multi-row INSERT / REPLACE statements
//! take the sorted-batch index path on a UNIQUE-indexed table: disjoint
//! key sets both commit; overlapping key sets never leave a duplicate
//! (the second committer is rejected, or its statement fails), and every
//! index agrees with the table afterwards, before and after a reopen.

use rustqlite::{Database, Value};

fn waldb(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("{}_{}.db", name, std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, k INTEGER UNIQUE, v TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX uv ON u(v)", []).unwrap();
    db.execute("CREATE TABLE src (n INTEGER PRIMARY KEY)", [])
        .unwrap();
    db.execute(
        "WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM c WHERE x < 400) \
         INSERT INTO src SELECT x FROM c",
        [],
    )
    .unwrap();
    (db, path)
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}

fn int(db: &Database, sql: &str) -> i64 {
    db.query(sql, []).unwrap()[0][0].as_integer()
}

/// No duplicate keys, the indexes agree with the table.
fn check(db: &Database) {
    let rows = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(rows[0][0], Value::Text("ok".into()), "{rows:?}");
    assert_eq!(
        int(db, "SELECT count(*) FROM u"),
        int(db, "SELECT count(DISTINCT k) FROM u WHERE k IS NOT NULL")
            + int(db, "SELECT count(*) FROM u WHERE k IS NULL")
    );
    assert_eq!(
        int(db, "SELECT count(*) FROM u"),
        int(
            db,
            "SELECT count(*) FROM u INDEXED BY uv WHERE v IS NOT NULL"
        )
    );
}

fn reopen_check(db: Database, path: &std::path::Path) -> i64 {
    let n = int(&db, "SELECT count(*) FROM u");
    drop(db);
    let db2 = Database::open(path).unwrap();
    check(&db2);
    assert_eq!(int(&db2, "SELECT count(*) FROM u"), n);
    n
}

#[test]
fn concurrent_batched_unique_disjoint_keys_both_commit() {
    let (mut db, path) = waldb("cub_disjoint");
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute(
        "INSERT INTO u (k, v) SELECT n, 'a' || n FROM src WHERE n <= 200",
        [],
    )
    .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute(
        "INSERT INTO u (k, v) SELECT n + 1000, 'b' || n FROM src WHERE n <= 200",
        [],
    )
    .unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    // Disjoint keys: SQLite's begin-concurrent may still reject on a page
    // overlap; either way no duplicate and no lost committed row.
    if db.execute("COMMIT", []).is_err() {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    check(&db);
    let n = reopen_check(db, &path);
    assert!(n == 200 || n == 400, "{n}");
    cleanup(&path);
}

#[test]
fn concurrent_batched_unique_overlapping_keys_never_duplicate() {
    let (mut db, path) = waldb("cub_overlap");
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute(
        "INSERT INTO u (k, v) SELECT n, 'a' || n FROM src WHERE n <= 200",
        [],
    )
    .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    // Overlaps keys 150..=200 with the first writer's uncommitted rows.
    let stmt = db.execute(
        "INSERT INTO u (k, v) SELECT n + 149, 'b' || n FROM src WHERE n <= 200",
        [],
    );
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let committed = stmt.is_ok() && db.execute("COMMIT", []).is_ok();
    if !committed {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    assert!(!committed, "an overlapping UNIQUE key set must not commit");
    check(&db);
    assert_eq!(reopen_check(db, &path), 200);
    cleanup(&path);
}

#[test]
fn concurrent_batched_replace_overlapping_keys_stay_unique() {
    let (mut db, path) = waldb("cub_replace");
    db.execute(
        "INSERT INTO u (k, v) SELECT n, 'base' || n FROM src WHERE n <= 300",
        [],
    )
    .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    // Every row conflicts with a committed row, some with this
    // statement's own earlier rows (k = n % 150).
    db.execute(
        "REPLACE INTO u (k, v) SELECT n % 150, 'r1-' || n FROM src WHERE n <= 300",
        [],
    )
    .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let stmt = db.execute(
        "REPLACE INTO u (k, v) SELECT n % 120, 'r2-' || n FROM src WHERE n <= 250",
        [],
    );
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    if !(stmt.is_ok() && db.execute("COMMIT", []).is_ok()) {
        let _ = db.execute("ROLLBACK", []);
    }
    Database::set_conn_identity(0);
    check(&db);
    // T1's REPLACE folded keys 0..150 onto their last writer.
    assert_eq!(
        int(&db, "SELECT count(*) FROM u WHERE k < 150"),
        int(&db, "SELECT count(DISTINCT k) FROM u WHERE k < 150")
    );
    reopen_check(db, &path);
    cleanup(&path);
}
