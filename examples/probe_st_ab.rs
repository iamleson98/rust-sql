//! Single-threaded isolation for the concurrent-writes A/B: the exact
//! per-statement shape (autocommit `?`-param INSERT, id INTEGER PRIMARY
//! KEY table) minus the 4-thread RwLock handoff. If this is identical
//! across two builds, any concurrent-writes delta is scheduler cadence,
//! not a per-statement engine cost.
use rustqlite::Value;
use std::time::Instant;

fn run_once() -> f64 {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    let start = Instant::now();
    for i in 0..1000i64 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    let el = start.elapsed().as_secs_f64();
    1000.0 / el
}

fn main() {
    for _ in 0..2 {
        run_once();
    }
    let mut samples = Vec::new();
    for _ in 0..7 {
        samples.push(run_once());
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "single-thread autocommit: best {:>9.0} ops/s | median {:>9.0}",
        samples.last().unwrap(),
        samples[samples.len() / 2]
    );
}
