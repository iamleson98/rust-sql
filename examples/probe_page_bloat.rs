//! Probe #3: confirm the suspected "page bloat → split per UPDATE" bug.
//!
//! Expectation: after N updates to a small set of rowids, the page count
//! of the DB file has grown dramatically (each split allocates a new page),
//! proving that deletes don't reclaim payload space.

use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!("INSERT INTO t (val, score) VALUES ({}, {})", i, i as f64);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    println!("After load (10k rows): page_count = {}, cache = {:?}", db.page_count(), db.cache_stats());

    // Do 1000 UPDATEs to the SAME 100 rowids repeatedly.
    let start = Instant::now();
    for i in 1..=1_000i64 {
        let target = (i % 100) + 1;  // always 1..=100
        let sql = format!("UPDATE t SET score = {} WHERE id = {}", i as f64, target);
        db.execute(&sql, []).unwrap();
    }
    let dur = start.elapsed();
    println!("After 1k updates to 100 rowids: page_count = {}, took {:?} ({:?}/op)",
             db.page_count(), dur, dur / 1000);

    // Now try updates across all 1000 rowids (no reuse).
    let start = Instant::now();
    for i in 1..=1_000i64 {
        let sql = format!("UPDATE t SET score = {} WHERE id = {}", i as f64 * 2.0, i);
        db.execute(&sql, []).unwrap();
    }
    let dur2 = start.elapsed();
    println!("After 1k updates to 1000 distinct rowids: page_count = {}, took {:?} ({:?}/op)",
             db.page_count(), dur2, dur2 / 1000);

    // Now do 5000 more updates to 5000 distinct rowids (still in 10k table).
    let start = Instant::now();
    for i in 1..=5_000i64 {
        let sql = format!("UPDATE t SET score = {} WHERE id = {}", i as f64 * 3.0, i);
        db.execute(&sql, []).unwrap();
    }
    let dur3 = start.elapsed();
    println!("After 5k updates to 5000 distinct rowids: page_count = {}, took {:?} ({:?}/op)",
             db.page_count(), dur3, dur3 / 5000);

    println!();
    println!("If page_count grew AND first run was much slower than second, splits are the bug.");
}
