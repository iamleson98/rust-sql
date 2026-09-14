//! Probe: native→SQLite `VACUUM INTO` export.
//!
//! Creates a native-format (RSQLDB04) database exercising the full schema
//! surface, runs `VACUUM INTO`, and verifies with REAL SQLite that the
//! output is a genuine SQLite database: magic header, integrity_check,
//! data equality, index/trigger/view round-trip, AUTOINCREMENT
//! high-water preservation.
use rustqlite::{Database, Value};

fn main() {
    let src = std::env::temp_dir().join("rsql_probe_export_src.db");
    let dst = std::env::temp_dir().join("rsql_probe_export_out.db");
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);

    let mut db = Database::open(&src).unwrap();
    let head = std::fs::read(&src).unwrap();
    assert_eq!(&head[0..8], b"RSQLDB04", "source must be native format");

    db.execute(
        "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT, b TEXT, c REAL);
         CREATE UNIQUE INDEX idx_b ON t(b);
         CREATE INDEX idx_desc ON t(c DESC);
         CREATE VIEW v AS SELECT b FROM t WHERE a > 0;
         CREATE TRIGGER trg AFTER INSERT ON t BEGIN UPDATE t SET b = 'trg' || NEW.a WHERE a = NEW.a; END;
         CREATE TABLE wr(k TEXT COLLATE NOCASE, v INT, PRIMARY KEY(k, v)) WITHOUT ROWID;",
        [],
    )
    .unwrap();
    for i in 0..50i64 {
        db.execute(
            "INSERT INTO t(b, c) VALUES (?, ?)",
            [
                Value::Text(format!("row{}", i).into()),
                Value::Real(i as f64 / 7.0),
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO wr(k, v) VALUES ('B', 2), ('a', 1), ('b', 3)",
        [],
    )
    .unwrap();
    // AUTOINCREMENT high-water: delete the top row; the seq must survive.
    db.execute("DELETE FROM t WHERE a = 50", []).unwrap();

    db.execute(format!("VACUUM INTO '{}'", dst.display()).as_str(), [])
        .unwrap();

    // 1. Output magic is real SQLite.
    let bytes = std::fs::read(&dst).unwrap();
    assert_eq!(
        &bytes[0..16],
        b"SQLite format 3\0",
        "output must be SQLite magic"
    );
    let write_v = u16::from_be_bytes([bytes[18], bytes[19]]);
    println!("output magic       = SQLite format 3");
    println!("output header 18/19= {write_v} (1 = rollback mode, like real SQLite VACUUM INTO)");

    // 2. Source untouched: still native, still queryable.
    let still = std::fs::read(&src).unwrap();
    assert_eq!(&still[0..8], b"RSQLDB04");
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(49));

    // 3. Real SQLite opens it, passes integrity_check, agrees on data.
    let conn = rusqlite::Connection::open(&dst).unwrap();
    let ok: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 49);
    let b: String = conn
        .query_row("SELECT b FROM t WHERE a = 49", [], |r| r.get(0))
        .unwrap();
    assert_eq!(b, "trg49", "trigger-rewritten value round-trips");
    let v: i64 = conn
        .query_row("SELECT COUNT(*) FROM v", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 49, "view reads in SQLite");
    // WITHOUT ROWID rows + NOCASE key order.
    let ks: Vec<String> = {
        let mut stmt = conn.prepare("SELECT k FROM wr").unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    };
    assert_eq!(
        ks,
        vec!["a".to_string(), "B".to_string(), "b".to_string()],
        "NOCASE PK order"
    );
    // Unique index enforced by SQLite itself on the written file. The
    // AFTER-INSERT trigger rewrites b to 'trg{a}', so the EXISTING key
    // is 'trg49'; SQLite's unique check runs before AFTER triggers
    // fire, so this must abort with a constraint error.
    let dup = conn.execute("INSERT INTO t(a, b, c) VALUES (900, 'trg49', 0.0)", []);
    assert!(
        dup.is_err(),
        "UNIQUE idx_b must be enforced by SQLite on the export"
    );
    assert!(
        matches!(dup, Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation),
        "expected a constraint violation, got {dup:?}"
    );
    // AUTOINCREMENT high-water preserved (deleted top rowid 50).
    let seq: i64 = conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 't'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seq, 50, "sqlite_sequence high-water survives export");
    println!("integrity_check    = ok");
    println!("rows/view/trigger/WR-PK/autoindex/AUTOINCREMENT all verified by real SQLite");

    // 4. Engine reopens its own export (magic sniff).
    let rt = Database::open(&dst).unwrap();
    let n2 = rt.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n2[0][0], Value::Integer(49));
    println!("engine re-open     = ok (49 rows)");

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    println!("ALL EXPORT CHECKS PASSED");
}
