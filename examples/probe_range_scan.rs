//! Probe: where does small-range-scan per-query time go?

use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10_000i64 {
        db.execute(ins, [
            rustqlite::Value::Text(format!("name{}", i).into()),
            rustqlite::Value::Integer(i * 2),
            rustqlite::Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Range scan 10 rows — the bench shape.
    let sql = "SELECT * FROM t WHERE id BETWEEN ? AND ?";
    let n = 2000;
    let start = Instant::now();
    for i in 0..n {
        let base = 100 + (i % 5000);
        let _ = db.query(sql, [rustqlite::Value::Integer(base), rustqlite::Value::Integer(base + 9)]).unwrap();
    }
    println!("SELECT * range 10 rows:  {:?}/query", start.elapsed().as_nanos() / (n as u128));

    // Narrower projection.
    let sql2 = "SELECT name FROM t WHERE id BETWEEN ? AND ?";
    let start = Instant::now();
    for i in 0..n {
        let base = 100 + (i % 5000);
        let _ = db.query(sql2, [rustqlite::Value::Integer(base), rustqlite::Value::Integer(base + 9)]).unwrap();
    }
    println!("SELECT name range 10:    {:?}/query", start.elapsed().as_nanos() / (n as u128));

    // Single-row point lookup via BETWEEN (same plan shape, 1 row).
    let sql3 = "SELECT * FROM t WHERE id BETWEEN ? AND ?";
    let start = Instant::now();
    for i in 0..n {
        let base = 100 + (i % 5000);
        let _ = db.query(sql3, [rustqlite::Value::Integer(base), rustqlite::Value::Integer(base)]).unwrap();
    }
    println!("SELECT * BETWEEN 1 row:  {:?}/query", start.elapsed().as_nanos() / (n as u128));

    // Rowid equality (RowidLookup plan).
    let sql4 = "SELECT * FROM t WHERE id = ?";
    let start = Instant::now();
    for i in 0..n {
        let base = 100 + (i % 5000);
        let _ = db.query(sql4, [rustqlite::Value::Integer(base)]).unwrap();
    }
    println!("SELECT * WHERE id = ?:   {:?}/query", start.elapsed().as_nanos() / (n as u128));
}
