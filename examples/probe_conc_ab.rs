//! Exact-shape A/B for the bench_full_vs_sqlite "Concurrent writes" row
//! (ubuntu CI gate flip 1.12x WIN -> 0.93x LOSS): 4 writer threads each
//! doing 250 autocommit INSERTs through an Arc<RwLock<Database>>, i.e.
//! interleaved per-statement autocommit DML under the outer write lock.
//! Reports per-iteration ops/s so run-to-run variance is visible.
use rustqlite::Value;
use std::sync::{Arc, RwLock};
use std::time::Instant;

fn run_once() -> f64 {
    let db = Arc::new(RwLock::new({
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
            .unwrap();
        db
    }));
    let mut handles = Vec::new();
    for tid in 0..4usize {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for local in (tid * 1000)..(tid * 1000 + 250) {
                let mut guard = db.write().unwrap();
                guard
                    .execute(
                        "INSERT INTO t (val) VALUES (?)",
                        [Value::Integer(local as i64)],
                    )
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    1000.0
}

fn main() {
    let mut samples = Vec::new();
    // 2 warmup + 9 measured, like the bench's measure() shape.
    for _ in 0..2 {
        run_once();
    }
    for _ in 0..9 {
        let start = Instant::now();
        let ops = run_once();
        let el = start.elapsed().as_secs_f64();
        samples.push(ops / el);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let best = samples.last().unwrap();
    let median = samples[samples.len() / 2];
    let worst = samples.first().unwrap();
    println!(
        "concurrent-writes shape: best {:>9.0} ops/s | median {:>9.0} | worst {:>9.0}",
        best, median, worst
    );
    println!(
        "samples: {:?}",
        samples.iter().map(|v| v.round() as u64).collect::<Vec<_>>()
    );
}
