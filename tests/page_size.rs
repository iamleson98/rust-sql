//! PRAGMA page_size tests: setting on a fresh database, persistence
//! across reopen, and the too-late no-op semantics (SQLite ignores the
//! pragma once the database has content without a VACUUM).
use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("{}.db", name));
    let _ = std::fs::remove_file(&path);
    let db = Database::open(&path).unwrap();
    (db, path)
}

#[test]
fn page_size_set_on_fresh_db_and_persist() {
    let (mut db, path) = tmpdb("pagesize_fresh");
    db.execute("PRAGMA page_size = 4096", []).unwrap();
    assert_eq!(db.query("PRAGMA page_size", []).unwrap()[0][0].as_integer(), 4096);

    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("value-{}", i).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.flush().unwrap();

    // File size must be a multiple of the configured page size.
    let sz = std::fs::metadata(&path).unwrap().len();
    assert_eq!(sz % 4096, 0, "file size must align to 4 KiB pages");

    // Round-trip: the header stores the page size.
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.query("PRAGMA page_size", []).unwrap()[0][0].as_integer(), 4096);
    let n = db2
        .query("SELECT COUNT(*) FROM t WHERE id BETWEEN 100 AND 199", [])
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(n, 100);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn page_size_ignored_after_content() {
    let (mut db, _path) = tmpdb("pagesize_late");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('a')", []).unwrap();
    // Content exists — the pragma must be a silent no-op (SQLite behavior).
    db.execute("PRAGMA page_size = 65536", []).unwrap();
    let cur = db.query("PRAGMA page_size", []).unwrap()[0][0].as_integer();
    assert_eq!(cur, 8192, "page size must not change after content exists");
    // Data survives.
    assert_eq!(db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer(), 1);
    let _ = std::fs::remove_file(&_path);
}

#[test]
fn page_size_invalid_values_rejected() {
    let (mut db, _path) = tmpdb("pagesize_invalid");
    // Not a power of two in the supported set — silently ignored.
    db.execute("PRAGMA page_size = 5000", []).unwrap();
    assert_eq!(db.query("PRAGMA page_size", []).unwrap()[0][0].as_integer(), 8192);
    let _ = std::fs::remove_file(&_path);
}
