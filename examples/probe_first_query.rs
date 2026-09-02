//! Decompose the first-query cost after a bulk insert (bench shape).
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
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

    let qsql = "SELECT name, val, score FROM t WHERE id = ?";
    // first query — cold everything
    let t0 = Instant::now();
    let _ = db.query(qsql, [Value::Integer(1)]).unwrap();
    println!(
        "first query:   {:>8.1} µs",
        t0.elapsed().as_secs_f64() * 1e6
    );
    // second query — cache populate
    let t1 = Instant::now();
    let _ = db.query(qsql, [Value::Integer(2)]).unwrap();
    println!(
        "second query:  {:>8.1} µs",
        t1.elapsed().as_secs_f64() * 1e6
    );
    // third query — warm
    let t2 = Instant::now();
    let _ = db.query(qsql, [Value::Integer(3)]).unwrap();
    println!(
        "third query:   {:>8.1} µs",
        t2.elapsed().as_secs_f64() * 1e6
    );
    // 1000 warm ops
    let t3 = Instant::now();
    for i in 1..=1000i64 {
        let target = (i % 1000) + 1;
        let _ = db.query(qsql, [Value::Integer(target)]).unwrap();
    }
    println!(
        "1000 warm ops: {:>8.1} µs ({:.1} ns/op)",
        t3.elapsed().as_secs_f64() * 1e6,
        t3.elapsed().as_secs_f64() * 1e9 / 1000.0
    );
}
