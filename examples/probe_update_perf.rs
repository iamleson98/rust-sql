//! Probe: time the per-statement flush overhead in an in-memory DB.
//!
//! Hypothesis: the 743x UPDATE-PK regression is caused by `file.sync_all()`
//! being called inside `Pager::flush()` on every single-statement UPDATE.
//! On a tmpfs-backed tempfile, fsync is cheap (~10-50us) but not free, and
//! 1000 fsyncs add up to ~50ms; the remaining 1.4s must come from the
//! cache iteration in flush (`self.cache.iter().filter(dirty)` walks 2048
//! pages per flush).

use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)", [])
        .unwrap();

    // Bulk-load 10k rows inside a transaction (fast path).
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!("INSERT INTO t (val, score) VALUES ({}, {})", i, i as f64);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // 1k single-statement UPDATEs, each outside a transaction.
    let start = Instant::now();
    for i in 1..=1_000i64 {
        let sql = format!("UPDATE t SET score = {} WHERE id = {}", i as f64 * 2.5, (i % 1000) + 1);
        db.execute(&sql, []).unwrap();
    }
    let auto = start.elapsed();
    println!("1000 single UPDATEs (auto-commit): {:?}", auto);

    // 1k UPDATEs inside BEGIN/COMMIT.
    let start = Instant::now();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1_000i64 {
        let sql = format!("UPDATE t SET score = {} WHERE id = {}", i as f64 * 2.5, (i % 1000) + 1);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let txn = start.elapsed();
    println!("1000 UPDATEs in BEGIN/COMMIT:      {:?}", txn);

    println!();
    println!("auto-commit overhead per op: {:?}", auto / 1000);
    println!("txn overhead per op:         {:?}", txn / 1000);
    println!("ratio (auto/txn):            {:.2}x", auto.as_secs_f64() / txn.as_secs_f64());
}
