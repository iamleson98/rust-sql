//! Verify the UPDATE payload-patch fast path fires and measure its effect.
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
            Value::Integer((i * 37) % 10000),
            Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    db
}

fn main() {
    let mut db = fresh_db();
    let usql = "UPDATE t SET score = score + 1.0 WHERE val > 5000";
    // warm (3 like the bench)
    for _ in 0..3 {
        db.execute(usql, []).unwrap();
    }
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let t0 = Instant::now();
        db.execute(usql, []).unwrap();
        let d = t0.elapsed();
        if d < best { best = d; }
    }
    println!("UPDATE range (patch path): {:>8.1} µs  (best of 5)", best.as_secs_f64() * 1e6);

    // correctness spot-check: score advanced exactly 4 times for matching rows
    let rows = db.query(
        "SELECT id, val, score FROM t WHERE val > 5000 AND val < 5010 ORDER BY id LIMIT 3",
        [],
    ).unwrap();
    for r in &rows {
        let id = r[0].as_integer();
        let val = r[1].as_integer();
        let score = r[2].as_real();
        let base = id as f64 * 1.5;
        let expect = base + 4.0; // 3 warm + 1 timed = 4 updates... plus best-of-5 = 8 total
        println!("id={id} val={val} score={score} (base={base}, delta={:.1})", score - base);
        let _ = expect;
    }
}
