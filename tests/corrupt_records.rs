//! An undecodable record is corruption, and every path reports it.
//!
//! Scan, filter, LIMIT, aggregate, join, top-N, the parallel workers, the
//! prepared-statement streaming drivers, UPDATE and DELETE used to SKIP a
//! row whose stored record failed to decode — a damaged page then showed
//! up as missing rows (or as a write that silently left the row alone)
//! instead of an error. SQLite reports SQLITE_CORRUPT; so does this engine
//! now. The same streaming drivers also swallowed a RAISING predicate (the
//! prepared-statement path returned the other rows where `query` and
//! SQLite raise); that is pinned here too, against SQLite step by step.

use rustqlite::{Database, StepResult, Value};

const MARKER: &str = "CORRUPT_ME_0123456789";

/// A file database whose row 117 has an invalid value tag: the TEXT
/// value's type byte (0x07, two bytes before the marker — tag + 1-byte
/// length) is overwritten with an unknown tag.
fn corrupt_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("c.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX t_b ON t(b)", []).unwrap();
        db.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, w INTEGER)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=300i64 {
            let a = if i == 117 {
                MARKER.to_string()
            } else {
                format!("row{i}")
            };
            db.execute(
                "INSERT INTO t VALUES (?1, ?2, ?3)",
                vec![
                    Value::Integer(i),
                    Value::Text(a.into()),
                    Value::Integer(i * 2),
                ],
            )
            .unwrap();
            db.execute(
                "INSERT INTO u VALUES (?1, ?2)",
                vec![Value::Integer(i), Value::Integer(i)],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    let hits: Vec<usize> = bytes
        .windows(MARKER.len())
        .enumerate()
        .filter(|(_, w)| *w == MARKER.as_bytes())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hits.len(), 1, "marker must be stored exactly once");
    let pos = hits[0];
    assert_eq!(bytes[pos - 1] as usize, MARKER.len(), "1-byte length");
    assert_eq!(bytes[pos - 2], 0x07, "TEXT tag");
    bytes[pos - 2] = 0xEE;
    std::fs::write(&path, &bytes).unwrap();
    path
}

fn is_corrupt(e: &rustqlite::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("corrupt") || s.contains("malformed") || s.contains("decode")
}

fn step_all(db: &Database, sql: &str) -> (usize, Result<(), String>) {
    let mut st = match db.prepare(sql) {
        Ok(s) => s,
        Err(e) => return (0, Err(e.to_string())),
    };
    let mut n = 0;
    loop {
        match st.step() {
            Ok(StepResult::Row) => n += 1,
            Ok(StepResult::Done) => return (n, Ok(())),
            Err(e) => return (n, Err(e.to_string())),
        }
    }
}

#[test]
fn corrupt_record_fails_every_read_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = corrupt_db(&dir);
    let db = Database::open(&path).unwrap();
    for sql in [
        "SELECT * FROM t",
        "SELECT a FROM t",
        "SELECT * FROM t WHERE b > 0",
        "SELECT * FROM t WHERE a LIKE '%9%'",
        "SELECT * FROM t LIMIT 1000",
        "SELECT * FROM t WHERE upper(a) <> '' LIMIT 1000",
        "SELECT * FROM t ORDER BY a LIMIT 3",
        "SELECT a, count(*) FROM t GROUP BY a",
        "SELECT max(a) FROM t",
        "SELECT * FROM t WHERE id = 117",
        "SELECT * FROM t WHERE id BETWEEN 100 AND 130",
        "SELECT t.a FROM t JOIN u ON u.id = t.id",
        "SELECT DISTINCT a FROM t",
    ] {
        match db.query(sql, []) {
            Ok(rows) => panic!("{sql}: returned {} rows from a corrupt table", rows.len()),
            Err(e) => assert!(is_corrupt(&e), "{sql}: {e}"),
        }
        let (_, r) = step_all(&db, sql);
        assert!(r.is_err(), "{sql}: the step path must fail too");
    }
    // Rows that do not touch the damaged record still read.
    assert_eq!(
        db.query("SELECT count(*) FROM u", []).unwrap()[0][0].as_integer(),
        300
    );
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_ne!(ic[0][0].as_text(), "ok", "integrity_check must see it");
}

#[test]
fn corrupt_record_fails_writes() {
    for sql in [
        "UPDATE t SET b = b + 1",
        "UPDATE t SET b = b + 1 WHERE a <> ''",
        "DELETE FROM t WHERE a <> ''",
        "DELETE FROM t WHERE id BETWEEN 110 AND 120",
        "UPDATE t SET a = 'x' WHERE id = 117",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = corrupt_db(&dir);
        let mut db = Database::open(&path).unwrap();
        let before = db.query("SELECT count(*) FROM u", []).unwrap()[0][0].as_integer();
        let r = db.execute(sql, []);
        assert!(r.is_err(), "{sql}: a write over a corrupt record must fail");
        assert_eq!(
            db.query("SELECT count(*) FROM u", []).unwrap()[0][0].as_integer(),
            before
        );
    }
}

/// A raising WHERE on the prepared-statement path: the rows BEFORE the
/// raising row come back, then the step that reaches it fails — exactly
/// SQLite's sequence (it used to drop the row and finish successfully).
#[test]
fn raising_predicate_step_matches_sqlite() {
    let mut db = Database::open_in_memory().unwrap();
    let lite = rusqlite::Connection::open_in_memory().unwrap();
    let ddl = "CREATE TABLE t(id INTEGER PRIMARY KEY, x INTEGER, y TEXT)";
    db.execute(ddl, []).unwrap();
    lite.execute_batch(ddl).unwrap();
    for i in 1..=50i64 {
        let x = if i == 30 { i64::MIN } else { i };
        db.execute(
            "INSERT INTO t(x, y) VALUES (?1, ?2)",
            vec![Value::Integer(x), Value::Text(format!("r{i}").into())],
        )
        .unwrap();
        lite.execute(
            "INSERT INTO t(x, y) VALUES (?1, ?2)",
            rusqlite::params![x, format!("r{i}")],
        )
        .unwrap();
    }
    for sql in [
        "SELECT * FROM t WHERE abs(x) > 0",
        "SELECT y FROM t WHERE abs(x) > 0",
        "SELECT id FROM t WHERE id > 5 AND abs(x) > 0",
        "SELECT * FROM t WHERE id >= 10 AND abs(x) > 0",
    ] {
        let mut st = lite.prepare(sql).unwrap();
        let mut rows = st.raw_query();
        let mut lite_n = 0;
        let lite_err = loop {
            match rows.next() {
                Ok(Some(_)) => lite_n += 1,
                Ok(None) => break false,
                Err(_) => break true,
            }
        };
        let (n, r) = step_all(&db, sql);
        assert!(lite_err, "oracle raises for {sql}");
        assert!(r.is_err(), "{sql}: step must raise");
        assert_eq!(n, lite_n, "{sql}: rows delivered before the error");
    }
}
