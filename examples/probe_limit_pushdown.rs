//! Probe: does the LIMIT pushdown fire on the direct query path vs the
//! streaming statement (step) path?

use std::time::Instant;
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..5000i64 {
        db.execute(
            "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Real(i as f64 * 0.5), Value::Text(format!("name-{i}").into())],
        ).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let n = 2000;

    // 1. direct query with LIMIT 1
    let t = Instant::now();
    for i in 0..n {
        let lo = ((i * 37) % 4000) as i64;
        let _ = db.query(
            "SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1",
            [Value::Integer(lo), Value::Integer(lo + 100)],
        );
    }
    println!("direct  BETWEEN LIMIT 1 : {:>7.1} µs/q", t.elapsed().as_secs_f64() * 1e6 / n as f64);

    // 2. streaming statement path (like the sqlx driver's fetch_optional)
    let t = Instant::now();
    for i in 0..n {
        let lo = ((i * 37) % 4000) as i64;
        let mut stmt = db.prepare("SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1").unwrap();
        let _ = stmt.bind_all(&[Value::Integer(lo), Value::Integer(lo + 100)]);
        let _ = stmt.step();
    }
    println!("step    BETWEEN LIMIT 1 : {:>7.1} µs/q", t.elapsed().as_secs_f64() * 1e6 / n as f64);

    // 3. direct query, LIMIT 10
    let t = Instant::now();
    for i in 0..n {
        let lo = ((i * 37) % 4000) as i64;
        let _ = db.query(
            "SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 10",
            [Value::Integer(lo), Value::Integer(lo + 100)],
        );
    }
    println!("direct  BETWEEN LIMIT 10: {:>7.1} µs/q", t.elapsed().as_secs_f64() * 1e6 / n as f64);

    // 4. direct query, no LIMIT (full scan, ~50 matches)
    let t = Instant::now();
    for i in 0..n {
        let lo = ((i * 37) % 4000) as i64;
        let _ = db.query(
            "SELECT a FROM bench WHERE a BETWEEN ? AND ?",
            [Value::Integer(lo), Value::Integer(lo + 50)],
        );
    }
    println!("direct  BETWEEN no LIMIT: {:>7.1} µs/q", t.elapsed().as_secs_f64() * 1e6 / n as f64);
}
