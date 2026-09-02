//! How many allocations does it take to fully drain the post-storm wake?
use rustqlite::{Database, Value};
use std::time::Instant;

fn fresh_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(sql, [
            Value::Text(format!("name{}", i).into()),
            Value::Integer(i * 2),
            Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn drain(n: usize) -> (std::time::Duration, std::time::Duration) {
    let mut db = fresh_db();
    let t0 = Instant::now();
    let mut sink: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        sink.push(vec![0u8; (i % 16) * 8 + 8]);
    }
    drop(sink);
    let t1 = Instant::now();
    let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
    (t0.elapsed(), t1.elapsed())
}

fn main() {
    // warm the process code paths first with a throwaway db+query
    {
        let mut db = fresh_db();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
    }
    for n in [0usize, 64, 512, 4096, 32768] {
        let (d_tap, d_q) = drain(n);
        println!("drain {:>6} allocs: tap {:>8.1} µs, query {:>7.1} µs",
            n, d_tap.as_secs_f64() * 1e6, d_q.as_secs_f64() * 1e6);
    }
}
