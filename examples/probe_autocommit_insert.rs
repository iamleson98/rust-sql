//! Isolate the auto-commit single-row INSERT cost: general path (`?` params)
//! vs literal fast path vs in-txn, plus where sub-time goes.
use rustqlite::Value;
use std::time::Instant;

fn bench(db: &mut rustqlite::Database, n: usize, label: &str) -> f64 {
    // warm the statement cache + chain
    for i in 0..50 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("w{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64),
            ],
        )
        .unwrap();
    }
    let start = Instant::now();
    for i in 0..n as i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    let us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!("{label:42} {us:7.3} us/row");
    us
}

fn main() {
    let n = 20_000usize;

    // --- variant 1: auto-commit + deferred (bench_compare shape) ---
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    bench(&mut db, n, "auto-commit, deferred (general path)");

    // --- variant 2: in-txn ---
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    bench(&mut db, n, "in-txn (general path)");

    // --- variant 3: literal inserts (chain fast path) ---
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    for i in 0..50 {
        db.execute(
            &format!("INSERT INTO t (name, val, score) VALUES ('w{i}', {i}, {i}.0)"),
            [],
        )
        .unwrap();
    }
    let start = Instant::now();
    for i in 0..n as i64 {
        db.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES ('name{}', {}, {})",
                i,
                i * 2,
                i as f64 * 1.5
            ),
            [],
        )
        .unwrap();
    }
    let us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!("{:42} {us:7.3} us/row", "auto-commit, literal (chain path)");

    // --- SQLite reference ---
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;")
        .ok();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    use rusqlite::params;
    for i in 0..50 {
        conn.execute(
            "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
            params![format!("w{i}"), i, i as f64],
        )
        .unwrap();
    }
    let start = Instant::now();
    for i in 0..n as i64 {
        conn.execute(
            "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
            params![format!("name{}", i), i * 2, i as f64 * 1.5],
        )
        .unwrap();
    }
    let us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!("{:42} {us:7.3} us/row", "sqlite auto-commit (reference)");
}
