//! Reproduce the bench's range-5000 context: deferred_flush on, warm-up
//! with a tiny range, insert-storm context before the read.
use std::time::Instant;
use rustqlite::types::Value;
use rustqlite::Database;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 * 1e-3
}

fn main() {
    // === A: exactly like bench_compare: deferred flush + small warm-up ===
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("name{}", i).into()), Value::Integer(i * 2), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    // bench's warm-up: 2-row range
    let _ = db.query(sql, [Value::Integer(1), Value::Integer(2)]).unwrap();
    let start = Instant::now();
    let rows = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    println!("A bench-style single-shot:  {:>8.1} us ({} rows)", us(start.elapsed()), rows.len());

    // second call — steady?
    let _ = db.query(sql, [Value::Integer(1), Value::Integer(2)]).unwrap();
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    println!("  second shot:              {:>8.1} us", us(start.elapsed()));

    // === B: warm-up with the SAME big range ===
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    println!("B same-range warm single:   {:>8.1} us", us(start.elapsed()));

    // === C: an insert STORM right before (simulating bench section flow) ===
    db.execute("BEGIN", []).unwrap();
    for i in 20000..=30000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("name{}", i).into()), Value::Integer(i * 2), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let _ = db.query(sql, [Value::Integer(1), Value::Integer(2)]).unwrap();
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    println!("C after 10k storm:          {:>8.1} us", us(start.elapsed()));

    // === D: repeat after storm a few times ===
    for k in 0..3 {
        let _ = db.query(sql, [Value::Integer(1), Value::Integer(2)]).unwrap();
        let start = Instant::now();
        let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
        println!("D post-storm run {}:         {:>8.1} us", k, us(start.elapsed()));
    }

    // === E: without deferred flush for reference ===
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db2.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db2.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("name{}", i).into()), Value::Integer(i * 2), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db2.execute("COMMIT", []).unwrap();
    let _ = db2.query(sql, [Value::Integer(1), Value::Integer(2)]).unwrap();
    let start = Instant::now();
    let _ = db2.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    println!("E no-deferred single-shot:  {:>8.1} us", us(start.elapsed()));
}
