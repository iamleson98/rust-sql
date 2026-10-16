//! Deferred database materialization (SQLite's lazy empty-db header):
//! opening a 0-byte / missing database file writes NOTHING — the
//! synthesized page 1 lives in the page cache, the file stays 0 bytes
//! until the first COMMIT (or a journal-mode switch, which SQLite also
//! makes durable immediately). A stale `-wal` sidecar next to a 0-page
//! main file is deleted on open (SQLite's pagerOpenWalIfPresent).
//! Found by SQLite's own wal4.test (the 557-iteration faultsim loop
//! asserted both behaviors).

use rustqlite::Database;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("deferred-init-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn empty_open_select_close_leaves_zero_byte_file() {
    let dir = tmpdir("zero-byte");
    let path = dir.join("test.db");
    let db = Database::open(&path).unwrap();
    let rows = db.query("SELECT name FROM sqlite_master", []).unwrap();
    assert!(rows.is_empty());
    assert_eq!(
        db.query("SELECT COUNT(*) FROM sqlite_master", []).unwrap()[0][0].as_integer(),
        1 // the count column: 0 rows of content
            - 1
    );
    drop(db);
    let md = std::fs::metadata(&path).unwrap();
    assert_eq!(md.len(), 0, "an unwritten database must stay 0 bytes");
}

#[test]
fn first_commit_materializes_the_header() {
    let dir = tmpdir("first-commit");
    let path = dir.join("test.db");
    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0), 0);
        db.execute("CREATE TABLE t(a)", []).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() >= 4096);
        db.execute("INSERT INTO t VALUES (1)", []).unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer(),
        1
    );
}

#[test]
fn stale_wal_next_to_empty_db_is_deleted_on_open() {
    let dir = tmpdir("stale-wal");
    let path = dir.join("test.db");
    // A leftover sidecar (crash artifact or foreign file) next to a
    // 0-page main database: meaningless (nothing to recover), deleted.
    std::fs::write(dir.join("test.db-wal"), vec![0xABu8; 5000]).unwrap();
    let db = Database::open(&path).unwrap();
    let rows = db.query("SELECT name FROM sqlite_master", []).unwrap();
    assert!(rows.is_empty(), "the stale wal must not be replayed");
    drop(db);
    assert!(
        !dir.join("test.db-wal").exists(),
        "SQLite deletes the wal of a 0-page database on open"
    );
}

#[test]
fn journal_mode_wal_on_empty_db_materializes_and_persists() {
    let dir = tmpdir("wal-empty");
    let path = dir.join("test.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        // SQLite writes page 1 (the header, WAL flag set) synchronously
        // at this pragma: the file is exactly one page.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
        db.execute("CREATE TABLE t(a)", []).unwrap();
        db.execute("INSERT INTO t VALUES (42)", []).unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(),
        "wal"
    );
    assert_eq!(
        db.query("SELECT a FROM t", []).unwrap()[0][0].as_integer(),
        42
    );
}

#[test]
fn rollback_of_first_ddl_leaves_zero_byte_file() {
    // SQLite: a CREATE TABLE rolled back before any commit leaves the
    // 0-byte file untouched (the journal is discarded, page 1 was never
    // written to the MAIN file in rollback-journal mode).
    let dir = tmpdir("rollback-ddl");
    let path = dir.join("test.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", []).unwrap();
        db.execute("CREATE TABLE t(a)", []).unwrap();
        db.execute("ROLLBACK", []).unwrap();
        assert_eq!(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0), 0);
        // The database still works after the abandoned transaction.
        db.execute("BEGIN", []).unwrap();
        db.execute("CREATE TABLE u(b)", []).unwrap();
        db.execute("COMMIT", []).unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM u", []).unwrap()[0][0].as_integer(),
        0
    );
}
