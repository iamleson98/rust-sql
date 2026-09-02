//! Multiple storms in ONE process: when is the wake paid?
use rustqlite::{Database, Value};
use std::time::Instant;

fn storm_and_query(round: usize) -> (std::time::Duration, std::time::Duration) {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    db.execute("BEGIN", []).unwrap();
    let t0 = Instant::now();
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
    let t_commit = Instant::now();
    db.execute("COMMIT", []).unwrap();
    let t_q = Instant::now();
    let _ = db
        .query(
            "SELECT name, val, score FROM t WHERE id = ?",
            [Value::Integer(1)],
        )
        .unwrap();
    let _ = round;
    (t_commit.duration_since(t0), t_q.duration_since(t_commit))
}

fn main() {
    for r in 0..5 {
        let (d_ins, d_q) = storm_and_query(r);
        println!(
            "round {}: inserts {:>7.1} ms, first-query {:>7.1} µs",
            r,
            d_ins.as_secs_f64() * 1e3,
            d_q.as_secs_f64() * 1e6
        );
    }
}
