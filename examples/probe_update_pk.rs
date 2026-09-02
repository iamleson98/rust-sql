//! Profile UPDATE by PK: find the regression.
use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=5_000i64 {
        let sql = format!(
            "INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})",
            i, i, i as f64
        );
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // Warm
    for i in 1..=50 {
        db.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            [
                rustqlite::Value::Real(i as f64),
                rustqlite::Value::Integer(i),
            ],
        )
        .unwrap();
    }
    let start = Instant::now();
    for i in 51..=1050 {
        db.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            [
                rustqlite::Value::Real(i as f64),
                rustqlite::Value::Integer(i),
            ],
        )
        .unwrap();
    }
    let d_r = start.elapsed();
    println!(
        "rustqlite UPDATE by PK 1k: {:?} ({} ns/op)",
        d_r,
        d_r.as_nanos() as f64 / 1000.0
    );

    // no-match fixed cost
    let start = Instant::now();
    for _i in 0..1000 {
        db.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            [rustqlite::Value::Real(0.0), rustqlite::Value::Integer(-1)],
        )
        .unwrap();
    }
    println!(
        "rustqlite UPDATE no-match 1k: {:?} ({} ns/op)",
        start.elapsed(),
        start.elapsed().as_nanos() as f64 / 1000.0
    );

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL); CREATE INDEX idx_val ON t(val);").unwrap();
    for i in 1..=5_000i64 {
        let sql = format!(
            "INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})",
            i, i, i as f64
        );
        conn.execute(&sql, []).unwrap();
    }
    for i in 1..=50 {
        conn.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            rusqlite::params![i as f64, i],
        )
        .unwrap();
    }
    let start = Instant::now();
    for i in 51..=1050 {
        conn.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            rusqlite::params![i as f64, i],
        )
        .unwrap();
    }
    let d_s = start.elapsed();
    println!(
        "sqlite     UPDATE by PK 1k: {:?} ({} ns/op)",
        d_s,
        d_s.as_nanos() as f64 / 1000.0
    );
    println!(
        "ratio: {:.2}x {}",
        d_s.as_secs_f64() / d_r.as_secs_f64(),
        if d_r < d_s { "FASTER" } else { "slower" }
    );
}
