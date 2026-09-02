//! Batch insert example: insert 1000 rows efficiently.

use rustqlite::Database;
use std::time::Instant;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();

    let n = 10_000;
    let start = Instant::now();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        let sql = format!("INSERT INTO t (name, val) VALUES ('item{}', {})", i, i * 2);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let elapsed = start.elapsed();
    println!(
        "Inserted {} rows in {:.2?} ({:.0} rows/sec)",
        n,
        elapsed,
        n as f64 / elapsed.as_secs_f64()
    );

    let start = Instant::now();
    let rows = db.query("SELECT COUNT(*), SUM(val) FROM t", []).unwrap();
    let elapsed = start.elapsed();
    println!(
        "Aggregated {} rows in {:.2?}: count={} sum={}",
        n, elapsed, rows[0][0], rows[0][1]
    );

    let start = Instant::now();
    let rows = db.query("SELECT * FROM t WHERE id = 5000", []).unwrap();
    let elapsed = start.elapsed();
    if rows.is_empty() {
        println!(
            "Point lookup in {:.2?}: (no rows — rowid index not yet used)",
            elapsed
        );
    } else {
        println!("Point lookup in {:.2?}: {:?}", elapsed, rows[0]);
    }
}
