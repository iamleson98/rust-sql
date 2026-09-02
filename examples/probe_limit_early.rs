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

    let n = 5000;
    // Match at the START of the table: LIMIT 1 should stop after ~1 row.
    let t = Instant::now();
    for _ in 0..n {
        let _ = db.query("SELECT a FROM bench WHERE a BETWEEN 0 AND 100 LIMIT 1", ());
    }
    println!("BETWEEN 0..100 LIMIT 1 : {:>7.2} µs/q (expect ~few µs if pushdown fires)", t.elapsed().as_secs_f64() * 1e6 / n as f64);

    // Match at the END: must scan ~everything.
    let t = Instant::now();
    for _ in 0..n {
        let _ = db.query("SELECT a FROM bench WHERE a BETWEEN 4900 AND 5000 LIMIT 1", ());
    }
    println!("BETWEEN 4900.. LIMIT 1  : {:>7.2} µs/q (expect ~full-scan cost)", t.elapsed().as_secs_f64() * 1e6 / n as f64);

    // No limit, full scan for comparison.
    let t = Instant::now();
    for _ in 0..n {
        let _ = db.query("SELECT a FROM bench WHERE a BETWEEN 0 AND 100", ());
    }
    println!("BETWEEN 0..100 no LIMIT : {:>7.2} µs/q", t.elapsed().as_secs_f64() * 1e6 / n as f64);
}
