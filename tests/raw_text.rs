//! TEXT values whose bytes are not valid UTF-8 keep those bytes, as in
//! SQLite (`CAST(x'c3' AS TEXT)`, a bound text of Latin-1 bytes): through
//! a file-backed reopen (the validated decode path), an overflow chain,
//! index lookups, `PRAGMA integrity_check`, and the SQLite-format
//! export / import in both directions (checked with real SQLite).

use rustqlite::types::text::Text;
use rustqlite::{Database, Value};

fn hexes(db: &Database, sql: &str) -> Vec<String> {
    db.query(sql, [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.as_str().to_string(),
            other => panic!("expected hex text, got {other:?}"),
        })
        .collect()
}

fn big_raw() -> Vec<u8> {
    // 9 000 bytes (an overflow chain at any page size) ending in a lone
    // continuation byte and a truncated 3-byte sequence.
    let mut v = b"latin1:".to_vec();
    v.extend(std::iter::repeat(0xE9u8).take(8_990));
    v.extend_from_slice(&[0x80, 0xE2, 0x82]);
    v
}

fn upper_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

#[test]
fn raw_text_survives_reopen_overflow_and_index_lookups() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.db");
    let big = big_raw();
    {
        let mut db = Database::open(&path).unwrap();
        for sql in [
            "CREATE TABLE t(id INTEGER PRIMARY KEY, x TEXT)",
            "CREATE INDEX t_x ON t(x)",
            "INSERT INTO t VALUES (1, CAST(x'c3' AS TEXT))",
            "INSERT INTO t VALUES (2, CAST(x'61ff62' AS TEXT))",
        ] {
            db.execute(sql, []).unwrap();
        }
        // A bound TEXT parameter with invalid bytes, and a big one.
        db.execute(
            "INSERT INTO t VALUES (3, ?)",
            [Value::Text(Text::from_bytes(&[b'L', 0xE9, b'a']))],
        )
        .unwrap();
        db.execute(
            "INSERT INTO t VALUES (4, ?)",
            [Value::Text(Text::from_bytes(&big))],
        )
        .unwrap();
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("PRAGMA integrity_check", []).unwrap(),
        vec![vec![Value::Text("ok".into())]]
    );
    assert_eq!(
        hexes(&db, "SELECT hex(x) FROM t ORDER BY id"),
        vec![
            "C3".to_string(),
            "61FF62".to_string(),
            "4CE961".to_string(),
            upper_hex(&big)
        ]
    );
    // The values themselves carry the bytes (and their typeof is text).
    let rows = db
        .query("SELECT x, typeof(x) FROM t ORDER BY id", [])
        .unwrap();
    match &rows[3][0] {
        Value::Text(t) => {
            assert!(t.is_raw());
            assert_eq!(t.as_bytes(), big.as_slice());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rows[3][1], Value::Text("text".into()));
    // Index lookups by the raw key (and the table scan agrees).
    for (key, id) in [
        (&b"\xc3"[..], 1i64),
        (b"a\xffb", 2),
        (b"L\xe9a", 3),
        (&big[..], 4),
    ] {
        for sql in [
            "SELECT id FROM t WHERE x = ?",
            "SELECT id FROM t NOT INDEXED WHERE x = ?",
        ] {
            let got = db.query(sql, [Value::Text(Text::from_bytes(key))]).unwrap();
            assert_eq!(got, vec![vec![Value::Integer(id)]], "{sql}");
        }
    }
}

#[test]
fn raw_text_round_trips_through_sqlite_files() {
    let dir = tempfile::tempdir().unwrap();
    // SQLite -> engine: a real SQLite file holding invalid UTF-8 TEXT.
    let src = dir.path().join("from_sqlite.db");
    {
        let c = rusqlite::Connection::open(&src).unwrap();
        c.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, x TEXT);
             INSERT INTO t VALUES (1, CAST(x'c3' AS TEXT)), (2, CAST(x'41ff42' AS TEXT)), (3, 'ok');",
        )
        .unwrap();
    }
    let db = Database::open(&src).unwrap();
    assert_eq!(
        hexes(&db, "SELECT hex(x) FROM t ORDER BY id"),
        vec!["C3", "41FF42", "6F6B"]
    );
    // Engine -> SQLite: the export writes the same bytes.
    let out = dir.path().join("exported.db");
    db.export_sqlite_format(&out).unwrap();
    let c = rusqlite::Connection::open(&out).unwrap();
    let got: Vec<String> = c
        .prepare("SELECT hex(x) FROM t ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(got, vec!["C3", "41FF42", "6F6B"]);
    let ok: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");
}
