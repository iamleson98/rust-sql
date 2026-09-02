//! Bisect the first-read-after-COMMIT penalty by query type:
//! SELECT 1 (no table) / rowid lookup / indexed lookup / COUNT.
//! If SELECT 1 pays the penalty too, it's API/catalog-level; if only
//! table-touching queries pay, it's btree/page-cache level.
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 * 1e-3
}

fn storm(db: &mut Database, base: i64, n: i64) {
    db.execute("BEGIN", []).unwrap();
    for i in base..base + n {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("user{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    for i in 1..=1000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("user{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }

    // Warm ALL the statements we'll measure.
    let qs = [
        "SELECT 1",
        "SELECT name, val, score FROM t WHERE id = ?",
        "SELECT id, name, score FROM t WHERE val = ?",
        "SELECT COUNT(*) FROM t",
    ];
    let _ = db.query(qs[0], []).unwrap();
    let _ = db.query(qs[1], [Value::Integer(500)]).unwrap();
    let _ = db.query(qs[2], [Value::Integer(500)]).unwrap();
    let _ = db.query(qs[3], []).unwrap();

    for round in 0..3 {
        // small storm
        storm(&mut db, 100_000 + round * 100, 100);
        let a = Instant::now();
        let _ = db.query(qs[0], []).unwrap();
        let d0 = us(a.elapsed());
        let a = Instant::now();
        let _ = db.query(qs[1], [Value::Integer(500)]).unwrap();
        let d1 = us(a.elapsed());
        let a = Instant::now();
        let _ = db.query(qs[2], [Value::Integer(500)]).unwrap();
        let d2 = us(a.elapsed());
        let a = Instant::now();
        let _ = db.query(qs[3], []).unwrap();
        let d3 = us(a.elapsed());
        println!(
            "round {}: SELECT1={:.1}us  rowid={:.1}us  idx={:.1}us  count={:.1}us",
            round, d0, d1, d2, d3
        );
    }

    // Now: does a WRITE (not read) right after commit also pay? e.g. an
    // INSERT outside a txn (autocommit write).
    let a = Instant::now();
    db.execute("INSERT INTO t (name, val, score) VALUES ('x', 1, 1.0)", [])
        .unwrap();
    println!("autocommit INSERT after storm: {:.1} us", us(a.elapsed()));
    let a = Instant::now();
    let _ = db.query(qs[2], [Value::Integer(500)]).unwrap();
    println!("  then idx query:             {:.1} us", us(a.elapsed()));

    // Does an explicit no-op statement pay? BEGIN+ROLLBACK.
    storm(&mut db, 200_000, 50);
    let a = Instant::now();
    db.execute("BEGIN", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    println!("BEGIN+ROLLBACK after storm:   {:.1} us", us(a.elapsed()));
    let a = Instant::now();
    let _ = db.query(qs[2], [Value::Integer(500)]).unwrap();
    println!("  then idx query:             {:.1} us", us(a.elapsed()));

    // Is it the TRANSACTION BEGIN path? Read inside a txn:
    storm(&mut db, 300_000, 50);
    let a = Instant::now();
    db.execute("BEGIN", []).unwrap();
    let d_begin = us(a.elapsed());
    let a = Instant::now();
    let _ = db.query(qs[2], [Value::Integer(500)]).unwrap();
    let d_q = us(a.elapsed());
    db.execute("COMMIT", []).unwrap();
    println!(
        "after storm: BEGIN={:.1}us  first query in txn={:.1}us",
        d_begin, d_q
    );
}
