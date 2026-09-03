//! Scan-aggregate probe for the 8-conn mixed R/W bench's read side.
//!
//! The CI bench-gate loses "8-conn mixed R/W 80/20" on macOS (0.85x). 80% of
//! that workload is `SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?`
//! (no index on a → full scan + aggregate) plus a PK point lookup. This probe
//! isolates both queries, rustqlite vs SQLite (rusqlite), single-threaded, so
//! the scan/aggregate efficiency ratio is measurable without contention.
//!
//! Run: cargo run --release --features sqlx --example probe_mixed_reads

use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..7000i64 {
        db.execute(
            "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Real(i as f64 * 0.5),
                Value::Text("name".into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    sqlite
        .execute_batch(
            "CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL);
             BEGIN;",
        )
        .unwrap();
    {
        let mut stmt = sqlite
            .prepare("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
            .unwrap();
        for i in 0..7000i64 {
            stmt.execute(rusqlite::params![i, i as f64 * 0.5, "name"])
                .unwrap();
        }
    }
    sqlite.execute_batch("COMMIT;").unwrap();

    const N: u32 = 2_560; // iterations in one CI task's read share

    // ---- rustqlite: scan aggregate ----
    {
        // warm
        for v in 0..10 {
            let _ = db
                .query(
                    "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?",
                    [Value::Integer(v), Value::Integer(v + 50)],
                )
                .unwrap();
        }
        let t = Instant::now();
        let mut checksum = 0i64;
        for i in 0..N {
            let v = 1 + ((i as i64) % 2000);
            let rows = db
                .query(
                    "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?",
                    [Value::Integer(v), Value::Integer(v + 50)],
                )
                .unwrap();
            if let Some(row) = rows.first() {
                if let Some(Value::Integer(n)) = row.first() {
                    checksum = checksum.wrapping_add(*n);
                }
            }
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / N as f64;
        println!("rustqlite scan-agg:  {us:8.2} µs/query  (checksum {checksum})");
    }

    // ---- SQLite: scan aggregate ----
    {
        let mut stmt = sqlite
            .prepare("SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?")
            .unwrap();
        // warm
        for v in 0..10 {
            let _ = stmt.query(rusqlite::params![v, v + 50]).unwrap();
        }
        let t = Instant::now();
        let mut checksum = 0i64;
        for i in 0..N {
            let v = 1 + ((i as i64) % 2000);
            let mut rows = stmt.query(rusqlite::params![v, v + 50]).unwrap();
            if let Some(row) = rows.next().unwrap() {
                checksum = checksum.wrapping_add(row.get::<_, i64>(0).unwrap());
            }
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / N as f64;
        println!("SQLite   scan-agg:  {us:8.2} µs/query  (checksum {checksum})");
    }

    // ---- decomposition: selective-only vs full decode ----
    {
        // Nothing passes (a < 0): selective decode + predicate eval ONLY.
        let t = Instant::now();
        for _ in 0..200 {
            let _ = db
                .query(
                    "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN -10 AND -1",
                    [],
                )
                .unwrap();
        }
        let us_no = t.elapsed().as_secs_f64() * 1e6 / 200.0;
        // Everything passes: + full row decode + aggregate per row.
        let t = Instant::now();
        for _ in 0..200 {
            let _ = db
                .query(
                    "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN -1 AND 9999999",
                    [],
                )
                .unwrap();
        }
        let us_all = t.elapsed().as_secs_f64() * 1e6 / 200.0;
        // Bare scan with trivially-true predicate (1=1): full decode, no BETWEEN eval.
        let t = Instant::now();
        for _ in 0..200 {
            let _ = db
                .query("SELECT COUNT(*), AVG(b) FROM bench WHERE 1=1", [])
                .unwrap();
        }
        let us_11 = t.elapsed().as_secs_f64() * 1e6 / 200.0;
        // SQLite equivalents for reference.
        let t = Instant::now();
        for _ in 0..200 {
            sqlite
                .query_row(
                    "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN -1 AND 9999999",
                    [],
                    |_| Ok(()),
                )
                .unwrap();
        }
        let sq_all = t.elapsed().as_secs_f64() * 1e6 / 200.0;
        println!(
            "decomp (7000 rows): no-match {us_no:.1}µs | all-match {us_all:.1}µs | 1=1 {us_11:.1}µs | sqlite all-match {sq_all:.1}µs"
        );
        println!(
            "  per row: selective+eval {:.1}ns | full decode extra {:.1}ns",
            us_no * 1000.0 / 7000.0,
            (us_all - us_no) * 1000.0 / 7000.0
        );
    }

    // ---- rustqlite: PK point lookup ----
    {
        let t = Instant::now();
        for i in 0..N {
            let rows = db
                .query(
                    "SELECT a FROM bench WHERE id = ?",
                    [Value::Integer(1 + (i as i64 % 7000))],
                )
                .unwrap();
            std::hint::black_box(rows.len());
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / N as f64;
        println!("rustqlite PK lookup: {us:7.2} µs/query");
    }

    // ---- SQLite: PK point lookup ----
    {
        let mut stmt = sqlite.prepare("SELECT a FROM bench WHERE id = ?").unwrap();
        let t = Instant::now();
        for i in 0..N {
            let mut rows = stmt
                .query(rusqlite::params![1 + (i as i64 % 7000)])
                .unwrap();
            let _ = rows.next().unwrap();
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / N as f64;
        println!("SQLite   PK lookup: {us:7.2} µs/query");
    }
}
