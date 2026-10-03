//! Adopt-on-write: the SQLite-format container drop's replacement
//! semantics. Opening a REAL SQLite `.db` loads it for interop
//! (read-only with respect to the file's bytes); the FIRST write
//! commit ADOPTS the path into the native container — one atomic
//! whole-image publish (temp + fsync + rename), then full native
//! machinery (page-granular WAL, MRMW multi-session).
//!
//! This suite pins every observable property of that handoff.

use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "adopt-{}-{}-{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// The canonical fixture: a REAL SQLite-format file written by the
/// engine's interchange writer.
fn sqlite_fixture(path: &std::path::Path, ddl: &[&str], inserts: &[&str]) {
    let mut db = Database::open_in_memory().unwrap();
    for s in ddl {
        db.execute(s, ()).unwrap();
    }
    for s in inserts {
        db.execute(s, ()).unwrap();
    }
    db.export_sqlite_format(path).unwrap();
}

fn is_sqlite_file(path: &std::path::Path) -> bool {
    let mut magic = [0u8; 16];
    match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            f.read_exact(&mut magic).is_ok() && &magic == b"SQLite format 3\0"
        }
        Err(_) => false,
    }
}

fn is_native_file(path: &std::path::Path) -> bool {
    let mut magic = [0u8; 6];
    match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            f.read_exact(&mut magic).is_ok() && &magic == b"RSQLDB"
        }
        Err(_) => false,
    }
}

/// A read-only open NEVER touches the file: same bytes, still SQLite
/// magic, `disk_format() == "sqlite"` for the session's lifetime.
#[test]
fn read_only_open_never_adopts() {
    let path = tmpdb("readonly");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT, b TEXT)"],
        &["INSERT INTO t VALUES (1, 'one'), (2, 'two')"],
    );
    let before = std::fs::read(&path).unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.disk_format(), "sqlite");
    assert_eq!(
        db.query("SELECT b FROM t ORDER BY a", []).unwrap(),
        vec![
            vec![Value::Text("one".into())],
            vec![Value::Text("two".into())],
        ]
    );
    drop(db);

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a read-only session must leave the SQLite bytes untouched"
    );
    assert!(is_sqlite_file(&path));
}

/// The FIRST write commit adopts the path into the native container:
/// native magic, every row (including the write) survives, reopen is a
/// native session, and further writes are plain native commits.
#[test]
fn first_write_adopts_the_path() {
    let path = tmpdb("adopt");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)"],
        &["INSERT INTO t (b) VALUES ('base')"],
    );

    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        db.execute("INSERT INTO t (b) VALUES ('first-write')", ())
            .unwrap();
        // The adopting commit rewrites the path; the session IS native now.
        assert_eq!(db.disk_format(), "native");
        // Read-your-write through the SAME session handle (the pager
        // rebind is invisible: same Arc identity, same page ids).
        assert_eq!(
            db.query("SELECT count(*) FROM t", []).unwrap(),
            vec![vec![Value::Integer(2)]]
        );
        // A second write is a plain native commit.
        db.execute("INSERT INTO t (b) VALUES ('second')", ())
            .unwrap();
    }

    assert!(
        is_native_file(&path),
        "the path is the native container now"
    );
    let db = Database::open(&path).unwrap();
    assert_eq!(db.disk_format(), "native");
    assert_eq!(
        db.query("SELECT b FROM t ORDER BY a", []).unwrap(),
        vec![
            vec![Value::Text("base".into())],
            vec![Value::Text("first-write".into())],
            vec![Value::Text("second".into())],
        ]
    );
}

/// DDL adopts too (the schema lives in the same commit boundary).
#[test]
fn ddl_adopts_the_path() {
    let path = tmpdb("adopt-ddl");
    sqlite_fixture(
        &path,
        &["CREATE TABLE base (a INT)"],
        &["INSERT INTO base VALUES (7)"],
    );
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE extra (b TEXT)", ()).unwrap();
        db.execute("INSERT INTO extra VALUES ('x')", ()).unwrap();
    }
    assert!(is_native_file(&path));
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query(
            "SELECT (SELECT count(*) FROM base) + (SELECT count(*) FROM extra)",
            []
        )
        .unwrap(),
        vec![vec![Value::Integer(2)]]
    );
}

/// An explicit transaction adopts at its COMMIT, not mid-flight; a
/// ROLLBACK leaves the SQLite bytes untouched.
#[test]
fn txn_adopts_at_commit_not_before() {
    let path = tmpdb("adopt-txn");
    sqlite_fixture(&path, &["CREATE TABLE t (a INTEGER PRIMARY KEY)"], &[]);

    // ROLLBACK: no adoption (nothing committed).
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", []).unwrap();
        db.execute("INSERT INTO t (a) VALUES (1)", []).unwrap();
        assert_eq!(db.disk_format(), "sqlite", "mid-txn: file untouched");
        db.execute("ROLLBACK", []).unwrap();
        assert_eq!(db.disk_format(), "sqlite", "rolled back: no adoption");
    }
    assert!(is_sqlite_file(&path), "rollback must not adopt");

    // COMMIT: adoption at the boundary.
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", []).unwrap();
        db.execute("INSERT INTO t (a) VALUES (1)", []).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
        db.execute("COMMIT", []).unwrap();
        assert_eq!(db.disk_format(), "native", "commit adopts");
    }
    assert!(is_native_file(&path));
}

/// A bare COMMIT of a read-only transaction must NOT adopt (the
/// file's bytes were never a user's write target).
#[test]
fn readonly_commit_does_not_adopt() {
    let path = tmpdb("adopt-bare-commit");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT)"],
        &["INSERT INTO t VALUES (1)"],
    );
    let before = std::fs::read(&path).unwrap();
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", []).unwrap();
        let _ = db.query("SELECT count(*) FROM t", []).unwrap();
        db.execute("COMMIT", []).unwrap();
        assert_eq!(db.disk_format(), "sqlite");
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// Stale SQLite sidecars (`-wal`, `-shm`, `-journal`) die at adoption;
/// the native WAL sidecar later uses the same `-wal` name without
/// confusion (a foreign-magic leftover is reset by `Wal::recover`).
#[test]
fn adoption_removes_stale_sidecars() {
    let path = tmpdb("adopt-sidecar");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT)"],
        &["INSERT INTO t VALUES (1)"],
    );
    let side = |sfx: &str| {
        path.with_file_name(format!(
            "{}{}",
            path.file_name().unwrap().to_string_lossy(),
            sfx
        ))
    };
    let wal = side("-wal");
    let shm = side("-shm");
    let rj = side("-journal");
    std::fs::write(&wal, b"stale sqlite wal bytes").unwrap();
    std::fs::write(&shm, b"shm").unwrap();
    std::fs::write(&rj, b"journal").unwrap();

    {
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            db.query("SELECT count(*) FROM t", []).unwrap(),
            vec![vec![Value::Integer(1)]]
        );
        db.execute("INSERT INTO t VALUES (2)", []).unwrap(); // adopts
    }
    assert!(is_native_file(&path));
    assert!(!wal.exists(), "the stale SQLite -wal must be removed");
    assert!(!shm.exists(), "the stale -shm must be removed");
    assert!(!rj.exists(), "the stale -journal must be removed");
}

/// Crash-window hygiene: an orphaned adoption staging file is swept at
/// the next SQLite-format open, and an adoption that never reached the
/// rename leaves the original bytes intact.
#[test]
fn orphaned_adoption_temp_is_swept() {
    let path = tmpdb("adopt-sweep");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT)"],
        &["INSERT INTO t VALUES (1)"],
    );
    let stem = path.file_name().unwrap().to_string_lossy().into_owned();
    let orphan = path.with_file_name(format!(".{}.rsqladopt999999", stem));
    std::fs::write(&orphan, b"torn staging bytes").unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT a FROM t", []).unwrap(),
        vec![vec![Value::Integer(1)]],
        "the original bytes survived the torn adoption"
    );
    drop(db);
    assert!(!orphan.exists(), "the sweeper removed the orphan temp");
    assert!(is_sqlite_file(&path));
}

/// Prepared-statement DML (the C ABI / sqlx surface) adopts exactly
/// like the execute path.
#[test]
fn prepared_stmt_dml_adopts() {
    let path = tmpdb("adopt-stmt");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INTEGER PRIMARY KEY, v TEXT)"],
        &[],
    );
    {
        let db = Database::open(&path).unwrap();
        let mut stmt = db.prepare("INSERT INTO t (v) VALUES (?)").unwrap();
        for v in ["a", "b", "c"] {
            stmt.bind(1, Value::Text(v.into())).unwrap();
            while let rustqlite::StepResult::Row = stmt.step().unwrap() {}
            stmt.reset();
        }
        drop(stmt);
        assert_eq!(
            db.disk_format(),
            "native",
            "stepped DML adopts at its commit"
        );
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT v FROM t ORDER BY a", []).unwrap(),
        vec![
            vec![Value::Text("a".into())],
            vec![Value::Text("b".into())],
            vec![Value::Text("c".into())],
        ]
    );
}

/// The pending session reports `journal_mode = memory` (its data lives
/// in memory until adoption), like every other memory-backed session.
#[test]
fn pending_reports_memory_journal_mode() {
    let path = tmpdb("adopt-mode");
    sqlite_fixture(&path, &["CREATE TABLE t (a INT)"], &[]);
    let mut db = Database::open(&path).unwrap();
    let mode = db.query("PRAGMA journal_mode", []).unwrap();
    assert_eq!(mode, vec![vec![Value::Text("memory".into())]]);
    db.execute("INSERT INTO t VALUES (1)", []).unwrap(); // adopts
    let mode = db.query("PRAGMA journal_mode", []).unwrap();
    assert_eq!(mode, vec![vec![Value::Text("delete".into())]]);
}

/// A UTF-16-encoded SQLite file loads (ORDER BY follows the file's
/// text encoding) and adopts like any other.
#[test]
fn utf16_file_loads_and_adopts() {
    let path = tmpdb("adopt-utf16");
    {
        let rc = rusqlite::Connection::open(&path).unwrap();
        rc.execute_batch(
            "PRAGMA encoding='UTF-16'; CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('hello');",
        )
        .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT a FROM t", []).unwrap(),
        vec![vec![Value::Text("hello".into())]]
    );
    let enc = db.query("PRAGMA encoding", []).unwrap();
    assert_eq!(enc, vec![vec![Value::Text("UTF-16le".into())]]);
    db.execute("INSERT INTO t VALUES ('world')", ()).unwrap();
    assert_eq!(db.disk_format(), "native");
    assert_eq!(
        db.query("SELECT count(*) FROM t", []).unwrap(),
        vec![vec![Value::Integer(2)]]
    );
}

/// `image()` (the serialize contract) of a pending session returns the
/// SOURCE's SQLite bytes — byte-stable on an untouched open.
#[test]
fn image_of_pending_session_is_the_source_bytes() {
    let path = tmpdb("adopt-image");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT)"],
        &["INSERT INTO t VALUES (5)"],
    );
    let db = Database::open(&path).unwrap();
    let img = db.image().unwrap();
    assert_eq!(&img[..16], b"SQLite format 3\0");
    assert_eq!(img, std::fs::read(&path).unwrap());
}

/// VACUUM INTO on a pending session writes a REAL SQLite-format file
/// of the committed state (the interchange writer).
#[test]
fn vacuum_into_pending_writes_real_sqlite() {
    let path = tmpdb("adopt-vinto");
    let out = tmpdb("adopt-vinto-out");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT)"],
        &["INSERT INTO t VALUES (9)"],
    );
    let mut db = Database::open(&path).unwrap();
    db.execute(format!("VACUUM INTO '{}'", out.display()).as_str(), [])
        .unwrap();
    assert!(is_sqlite_file(&out));
    let rc = rusqlite::Connection::open(&out).unwrap();
    let n: i64 = rc.query_row("SELECT a FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 9);
    // The SOURCE file is untouched (export-only).
    assert!(is_sqlite_file(&path));
}

/// After adoption, a SECOND session can open the same file — the
/// native container's multi-session MRMW (impossible for the old
/// single-connection SQLite-format container).
#[test]
fn post_adoption_multi_session() {
    let path = tmpdb("adopt-multi");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INTEGER PRIMARY KEY, v TEXT)"],
        &[],
    );
    {
        let mut w = Database::open(&path).unwrap();
        w.execute("INSERT INTO t (v) VALUES ('w1')", []).unwrap(); // adopts
        let r = Database::open(&path).unwrap();
        // The second session reads the adopted native file — shared,
        // not single-connection.
        assert_eq!(
            r.query("SELECT count(*) FROM t", []).unwrap(),
            vec![vec![Value::Integer(1)]]
        );
        w.execute("INSERT INTO t (v) VALUES ('w2')", []).unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT count(*) FROM t", []).unwrap(),
        vec![vec![Value::Integer(2)]]
    );
}

/// A stale rollback-journal sidecar (an invalidated / zeroed header —
/// SQLite's own commit marker form) is removed at open, and the clean
/// image loads pending.
#[test]
fn stale_journal_sidecar_swept_at_open() {
    let path = tmpdb("adopt-rjsweep");
    sqlite_fixture(
        &path,
        &["CREATE TABLE t (a INT)"],
        &["INSERT INTO t VALUES (1)"],
    );
    let journal = path.with_file_name(format!(
        "{}-journal",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&journal, vec![0u8; 512]).unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT count(*) FROM t", []).unwrap(),
        vec![vec![Value::Integer(1)]]
    );
    drop(db);
    assert!(!journal.exists(), "the invalidated journal is consumed");
}
