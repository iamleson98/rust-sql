//! Isolate the point-lookup-by-rowid bench shape: fresh DB → 10k-row
//! in-txn insert → time 1000 point lookups (exactly like bench_compare),
//! then variants (warmup vs no warmup, flush state).
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    for variant in 0..3 {
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

        let qsql = "SELECT name, val, score FROM t WHERE id = ?";
        match variant {
            0 => {
                // exactly the bench: no warmup
                let start = Instant::now();
                for i in 1..=1000i64 {
                    let target = (i % 1000) + 1;
                    let _ = db.query(qsql, [Value::Integer(target)]).unwrap();
                }
                println!("no-warmup           {:>8.1} µs total", start.elapsed().as_secs_f64() * 1e6);
            }
            1 => {
                // warm both the statement cache and the flush state
                let _ = db.query(qsql, [Value::Integer(1)]).unwrap();
                let _ = db.query(qsql, [Value::Integer(2)]).unwrap();
                let start = Instant::now();
                for i in 1..=1000i64 {
                    let target = (i % 1000) + 1;
                    let _ = db.query(qsql, [Value::Integer(target)]).unwrap();
                }
                println!("warm-2-queries       {:>8.1} µs total", start.elapsed().as_secs_f64() * 1e6);
            }
            _ => {
                // longer run to see steady-state per-op
                let _ = db.query(qsql, [Value::Integer(1)]).unwrap();
                let _ = db.query(qsql, [Value::Integer(2)]).unwrap();
                let start = Instant::now();
                for i in 1..=100000i64 {
                    let target = (i % 1000) + 1;
                    let _ = db.query(qsql, [Value::Integer(target)]).unwrap();
                }
                let d = start.elapsed();
                println!("steady-state 100k    {:>8.1} ns/op (total {:?})", d.as_secs_f64() * 1e9 / 100000.0, d);
            }
        }
    }
}
