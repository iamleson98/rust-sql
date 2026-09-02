//! Does the small-class tap work when it runs as the VERY FIRST query
//! in the process (cold code), like the in-query settle would?
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
    // tap-then-query FIRST in the process (cold everything)
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(64);
        for i in 0..64u32 {
            sink.push(vec![0u8; ((i * 13) % 128 + 8) as usize]);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("COLD tap-then-query: tap {:>6.1} µs, query {:>7.1} µs",
            t0.elapsed().as_secs_f64() * 1e6, t1.elapsed().as_secs_f64() * 1e6);
    }
    // now warm — repeat
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(64);
        for i in 0..64u32 {
            sink.push(vec![0u8; ((i * 13) % 128 + 8) as usize]);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db.query("SELECT name, val, score FROM t WHERE id = ?", [Value::Integer(1)]).unwrap();
        println!("WARM tap-then-query: tap {:>6.1} µs, query {:>7.1} µs",
            t0.elapsed().as_secs_f64() * 1e6, t1.elapsed().as_secs_f64() * 1e6);
    }
}
