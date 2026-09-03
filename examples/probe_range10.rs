//! Perf-attribution probe for the 10-row range scan (the macOS CI
//! bench-gate's borderline row). Runs the exact bench query 1M times so
//! `perf record` gets a clean profile.
//!
//! Run: cargo run --release --example probe_range10
//!      perf record -g --call-graph dwarf target/release/examples/probe_range10
//!      perf report

use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=20_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name-{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 / 3.0),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    // warm
    let _ = db
        .query(sql, [Value::Integer(1000), Value::Integer(1009)])
        .unwrap();

    const N: u32 = 1_000_000;
    let mut sink = 0usize;
    let t = Instant::now();
    for _ in 0..N {
        let rows = db
            .query(sql, [Value::Integer(1000), Value::Integer(1009)])
            .unwrap();
        sink += rows.len();
    }
    let ns = t.elapsed().as_secs_f64() * 1e9 / N as f64;
    println!(
        "range10 (name TEXT): {ns:.1} ns/query ({sink} rows total) — per-row ≈ {:.1} ns",
        (ns - 300.0) / 10.0
    );

    // Same shape, NO text column: isolates the Box<str> alloc + drop cost.
    let sql2 = "SELECT val, score, id FROM t WHERE id BETWEEN ? AND ?";
    let _ = db
        .query(sql2, [Value::Integer(1000), Value::Integer(1009)])
        .unwrap();
    let t = Instant::now();
    for _ in 0..N {
        let rows = db
            .query(sql2, [Value::Integer(1000), Value::Integer(1009)])
            .unwrap();
        sink += rows.len();
    }
    let ns2 = t.elapsed().as_secs_f64() * 1e9 / N as f64;
    println!(
        "range10 (scalars only): {ns2:.1} ns/query — per-row ≈ {:.1} ns",
        (ns2 - 300.0) / 10.0
    );
    println!("text column cost ≈ {:.1} ns/row", (ns - ns2) / 10.0);
    std::hint::black_box(sink);
}
