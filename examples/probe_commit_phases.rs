//! PROBE: decompose one autocommit INSERT/UPDATE on a large
//! SQLite-format file — statement execution vs commit machinery — to
//! find the per-commit fixed residual.
//! Run: `cargo run --release --example probe_commit_phases -- [rows] [m]`
use rustqlite::{Database, Value};
use std::time::Instant;

const PAD: usize = 96;

fn pad_for(i: i64) -> String {
    let mut s = format!("p{i}-");
    while s.len() < PAD {
        s.push('x');
    }
    s
}

fn main() {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);
    let m: i64 = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(300);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.db");
    let mut db = Database::open_sqlite_format(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("a{i}").into()),
                Value::Integer(i.wrapping_mul(48271).rem_euclid(rows)),
                Value::Text(pad_for(i).into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Warm: a few autocommit rounds first (cache population).
    for i in 0..20 {
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("w{i}").into()),
                Value::Integer(i),
                Value::Text(pad_for(900_000 + i).into()),
            ],
        )
        .unwrap();
    }

    // Phase timing: explicit BEGIN ... INSERT ... COMMIT.
    let (mut t_begin, mut t_stmt, mut t_commit) = (0.0f64, 0.0f64, 0.0f64);
    for i in 1..=m {
        let t0 = Instant::now();
        db.execute("BEGIN", []).unwrap();
        let t1 = Instant::now();
        db.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer(i.wrapping_mul(7919).rem_euclid(rows)),
                Value::Text(pad_for(800_000 + i).into()),
            ],
        )
        .unwrap();
        let t2 = Instant::now();
        db.execute("COMMIT", []).unwrap();
        let t3 = Instant::now();
        t_begin += (t1 - t0).as_secs_f64();
        t_stmt += (t2 - t1).as_secs_f64();
        t_commit += (t3 - t2).as_secs_f64();
    }
    let mm = m as f64;
    println!(
        "INSERT txn at {rows} rows: begin {:.1} us, stmt {:.1} us, commit {:.1} us",
        t_begin / mm * 1e6,
        t_stmt / mm * 1e6,
        t_commit / mm * 1e6
    );

    let (mut t_begin, mut t_stmt, mut t_commit) = (0.0f64, 0.0f64, 0.0f64);
    for i in 1..=m {
        let t0 = Instant::now();
        db.execute("BEGIN", []).unwrap();
        let t1 = Instant::now();
        db.execute(
            "UPDATE t SET b = ? WHERE id = ?",
            [
                Value::Integer(i.wrapping_mul(104729).rem_euclid(rows)),
                Value::Integer(i),
            ],
        )
        .unwrap();
        let t2 = Instant::now();
        db.execute("COMMIT", []).unwrap();
        let t3 = Instant::now();
        t_begin += (t1 - t0).as_secs_f64();
        t_stmt += (t2 - t1).as_secs_f64();
        t_commit += (t3 - t2).as_secs_f64();
    }
    println!(
        "UPDATE txn at {rows} rows: begin {:.1} us, stmt {:.1} us, commit {:.1} us",
        t_begin / mm * 1e6,
        t_stmt / mm * 1e6,
        t_commit / mm * 1e6
    );

    // Same statements against the NATIVE in-memory container (no
    // publish machinery): the pure engine-side statement cost.
    let mut mem = Database::open_in_memory().unwrap();
    mem.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, pad TEXT)",
        [],
    )
    .unwrap();
    mem.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    mem.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
        mem.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("a{i}").into()),
                Value::Integer(i.wrapping_mul(48271).rem_euclid(rows)),
                Value::Text(pad_for(i).into()),
            ],
        )
        .unwrap();
    }
    mem.execute("COMMIT", []).unwrap();
    let t0 = Instant::now();
    for i in 1..=m {
        mem.execute(
            "INSERT INTO t(a, b, pad) VALUES (?, ?, ?)",
            [
                Value::Text(format!("n{i}").into()),
                Value::Integer(i.wrapping_mul(7919).rem_euclid(rows)),
                Value::Text(pad_for(800_000 + i).into()),
            ],
        )
        .unwrap();
    }
    println!(
        "native in-memory autocommit INSERT (statement+engine commit): {:.1} us",
        t0.elapsed().as_secs_f64() / mm * 1e6
    );
    let t0 = Instant::now();
    for i in 1..=m {
        mem.execute(
            "UPDATE t SET b = ? WHERE id = ?",
            [
                Value::Integer(i.wrapping_mul(104729).rem_euclid(rows)),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    println!(
        "native in-memory autocommit UPDATE: {:.1} us",
        t0.elapsed().as_secs_f64() / mm * 1e6
    );
}
