//! perf-profile target: tight loops of the three losing DML shapes so perf
//! can attribute the per-op cost. ~2s per shape.
use rustqlite::{Database, Value};
use std::time::{Duration, Instant};

fn main() {
    let shape = std::env::args().nth(1).unwrap_or_default();
    let n = 10_000i64;
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let run = Duration::from_secs(2);
    match shape.as_str() {
        "update" => {
            let t0 = Instant::now();
            let mut i = 0i64;
            while t0.elapsed() < run {
                let id = (i % n) + 1;
                db.execute(
                    "UPDATE t SET val = ? WHERE id = ?",
                    [Value::Integer(i * 2), Value::Integer(id)],
                )
                .unwrap();
                i += 1;
            }
            println!("update ops: {i}");
        }
        "insert" => {
            db.execute(
                "CREATE TABLE iu (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            let t0 = Instant::now();
            let mut batch = 0i64;
            while t0.elapsed() < run {
                db.execute("BEGIN", []).unwrap();
                for i in 1..=1000i64 {
                    db.execute(
                        "INSERT INTO iu (name, val) VALUES (?, ?)",
                        [
                            Value::Text(format!("name{}", i).into()),
                            Value::Integer(batch * 1_000_000 + i),
                        ],
                    )
                    .unwrap();
                }
                db.execute("COMMIT", []).unwrap();
                batch += 1;
            }
            println!("insert ops: {}", batch * 1000);
        }
        "cycle" => {
            let t0 = Instant::now();
            let mut i = 0i64;
            while t0.elapsed() < run {
                let id = (i % n) + 1;
                db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(id)])
                    .unwrap();
                db.execute(
                    "INSERT INTO t (id, name, val, score) VALUES (?, ?, ?, ?)",
                    [
                        Value::Integer(id),
                        Value::Text(format!("name{}", id).into()),
                        Value::Integer(id),
                        Value::Real(id as f64),
                    ],
                )
                .unwrap();
                i += 1;
            }
            println!("cycle pairs: {i}");
        }
        _ => println!("usage: prof_dml_loop [update|insert|cycle]"),
    }
}
