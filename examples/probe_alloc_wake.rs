//! Isolate the post-storm spike: is it the first ALLOCATION (mimalloc
//! delayed-free processing) or something in the query path?
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

fn main() {
    // (a) plain: point lookup first
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("point-first            {:>8.1} µs", t0.elapsed().as_secs_f64() * 1e6);
    }
    // (b) dummy allocations first (absorb any allocator wake), then point
    {
        let mut db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(64);
        for i in 0..64 {
            sink.push(vec![0u8; (i * 13) % 128 + 8]);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("alloc-warm, then point {:>8.1} µs (allocs: {:.1} µs)",
            t1.elapsed().as_secs_f64() * 1e6, t0.elapsed().as_secs_f64() * 1e6);
    }
    // (c) COMMIT itself timed (does the storm cost land at COMMIT?)
    {
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
        let t0 = Instant::now();
        db.execute("COMMIT", []).unwrap();
        println!("COMMIT alone           {:>8.1} µs", t0.elapsed().as_secs_f64() * 1e6);
    }
}
