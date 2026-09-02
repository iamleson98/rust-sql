//! Bisect the 270µs first-query cost: different first statements after the
//! same bulk insert.
use rustqlite::{Database, Value};
use std::time::Instant;

fn fresh_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(
            sql,
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn main() {
    // (a) point lookup as first query
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let _ = db
            .query(
                "SELECT name, val, score FROM t WHERE id = ?",
                [Value::Integer(1)],
            )
            .unwrap();
        println!(
            "first=point-lookup   {:>8.1} µs",
            t0.elapsed().as_secs_f64() * 1e6
        );
    }
    // (b) COUNT(*) as first query (aggregate streaming path)
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        println!(
            "first=COUNT(*)       {:>8.1} µs",
            t0.elapsed().as_secs_f64() * 1e6
        );
    }
    // (c) full scan first, then point lookup
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        let t1 = Instant::now();
        let _ = db
            .query(
                "SELECT name, val, score FROM t WHERE id = ?",
                [Value::Integer(1)],
            )
            .unwrap();
        println!(
            "full-scan {:>8.1} µs, then point {:>8.1} µs",
            t0.elapsed().as_secs_f64() * 1e6,
            t1.elapsed().as_secs_f64() * 1e6
        );
    }
    // (d) point lookup on an EMPTY table (parse+plan+fastpath only)
    {
        let mut db = Database::open_in_memory().unwrap();
        db.set_deferred_flush(true);
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'x', 1, 1.0)", [])
            .unwrap();
        let t0 = Instant::now();
        let _ = db
            .query(
                "SELECT name, val, score FROM t WHERE id = ?",
                [Value::Integer(1)],
            )
            .unwrap();
        println!(
            "first=point(1 row)   {:>8.1} µs",
            t0.elapsed().as_secs_f64() * 1e6
        );
    }
}
