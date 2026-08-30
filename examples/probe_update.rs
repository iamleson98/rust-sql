//! Profile the UPDATE-by-PK execute() pipeline with the built-in counters.
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn main() {
    rustqlite::api::profile::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("user{}", i).into()), Value::Integer(i), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    let sql = "UPDATE t SET score = ? WHERE id = ?";
    // warm
    for i in 0..50 {
        db.execute(sql, [Value::Real(i as f64), Value::Integer((i % 1000) + 1)]).unwrap();
    }
    rustqlite::api::profile::reset();
    let n = 2000;
    let start = Instant::now();
    for i in 0..n as i64 {
        db.execute(sql, [Value::Real(i as f64 * 2.5), Value::Integer((i % 1000) + 1)]).unwrap();
    }
    let total = start.elapsed();
    println!("UPDATE by PK: {:.0} ns/op", total.as_nanos() as f64 / n as f64);
    rustqlite::api::profile::dump();
    println!("(parse+plan+cache+exec sums only the instrumented spans; exec dominates)");

    // DELETE by PK for comparison
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE t_del (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db2.execute("BEGIN", []).unwrap();
    for i in 1..=2000i64 {
        db2.execute("INSERT INTO t_del (x) VALUES (?)", [Value::Integer(i)]).unwrap();
    }
    db2.execute("COMMIT", []).unwrap();
    let sql2 = "DELETE FROM t_del WHERE id = ?";
    for i in 0..20 {
        db2.execute(sql2, [Value::Integer((i % 2000) + 1)]).unwrap();
    }
    let start = Instant::now();
    for i in 0..500i64 {
        db2.execute(sql2, [Value::Integer((i % 1900) + 30)]).unwrap();
    }
    println!("DELETE by PK: {:.0} ns/op", start.elapsed().as_nanos() as f64 / 500.0);
}
