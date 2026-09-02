//! Count leaf mid-splits + measure per-insert index cost.
use std::time::Instant;

fn main() {
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let vals: Vec<i64> = (0..10_000).map(|_| (rand() % 1_000_000) as i64).collect();

    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    let start = Instant::now();
    for chunk in vals.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|v| format!("('n{}', {}, 1.5)", v, v))
            .collect();
        db.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let d = start.elapsed();
    let stats = (0, 0);
    println!(
        "random indexed: {:?} ({} ns/row) append hits={} misses={}",
        d,
        d.as_nanos() as f64 / 10_000.0,
        stats.0,
        stats.1
    );

    // Sorted ascending with index (append path).
    let mut db2 = rustqlite::Database::open_in_memory().unwrap();
    db2.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db2.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    let sorted: Vec<i64> = {
        let mut v = vals.clone();
        v.sort();
        v
    };
    let start = Instant::now();
    for chunk in sorted.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|v| format!("('n{}', {}, 1.5)", v, v))
            .collect();
        db2.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let d2 = start.elapsed();
    println!(
        "sorted indexed: {:?} ({} ns/row)",
        d2,
        d2.as_nanos() as f64 / 10_000.0
    );
}
