//! Does a mid-storm drain survive 8k more inserts?
use rustqlite::{Database, Value};
use std::time::Instant;

fn drain() {
    let mut sink: Vec<Vec<u8>> = Vec::with_capacity(512);
    for i in 0..512usize {
        sink.push(vec![0u8; (i % 16) * 8 + 8]);
    }
    drop(sink);
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(
            sql,
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
        if i == 2000 {
            // mid-storm drain (where note_alloc_burst would fire)
            drain();
        }
    }
    db.execute("COMMIT", []).unwrap();
    let t0 = Instant::now();
    let _ = db
        .query(
            "SELECT name, val, score FROM t WHERE id = ?",
            [Value::Integer(1)],
        )
        .unwrap();
    println!(
        "mid-storm drain, query {:>7.1} µs",
        t0.elapsed().as_secs_f64() * 1e6
    );

    // control: drain AFTER the whole storm
    let mut db2 = Database::open_in_memory().unwrap();
    db2.set_deferred_flush(true);
    db2.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db2.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db2.execute(
            sql,
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db2.execute("COMMIT", []).unwrap();
    drain();
    let t1 = Instant::now();
    let _ = db2
        .query(
            "SELECT name, val, score FROM t WHERE id = ?",
            [Value::Integer(1)],
        )
        .unwrap();
    println!(
        "post-storm drain, query {:>7.1} µs",
        t1.elapsed().as_secs_f64() * 1e6
    );
}
