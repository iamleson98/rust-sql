//! Proper cold-process drain test: one variant per process run.
//! Usage: probe_drain_cold [drain_count] [min_size] [max_span]
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(0);
    let span: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(128);

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
    // drain: N allocations cycling 8..(8+span)
    let t0 = Instant::now();
    let mut sink: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        sink.push(vec![0u8; (i % (span / 8)) * 8 + 8]);
    }
    drop(sink);
    let t1 = Instant::now();
    let _ = db.query(qsql, [Value::Integer(1)]).unwrap();
    let t2 = Instant::now();
    println!(
        "n={} span={}: drain {:>7.1} µs, query {:>7.1} µs",
        n,
        span,
        t1.duration_since(t0).as_secs_f64() * 1e6,
        t2.duration_since(t1).as_secs_f64() * 1e6
    );
}
