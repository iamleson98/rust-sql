//! Manual phase-instrumentation probe: where does multi-row INSERT time go?
use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    {
        let values: Vec<String> = (1..=200i64).map(|i| format!("('w{}', {}, {})", i, i, i as f64)).collect();
        db.execute(&format!("INSERT INTO t (name, val, score) VALUES {}", values.join(", ")), []).unwrap();
    }
    let n = 10_000i64;
    let batch = 500;

    // Variant A: WITH index (baseline)
    let start = Instant::now();
    for chunk_start in (1..=n).step_by(batch) {
        let chunk_end = (chunk_start + batch as i64 - 1).min(n);
        let values: Vec<String> = (chunk_start..=chunk_end)
            .map(|i| format!("('name{}', {}, {})", i, i * 2, i as f64 * 1.5))
            .collect();
        db.execute(&format!("INSERT INTO t (name, val, score) VALUES {}", values.join(", ")), []).unwrap();
    }
    println!("with index:    {:?}  idx_append hits/misses: {:?}", start.elapsed(), (0,0));

    // Variant B: same inserts on a table WITHOUT an index (isolates index cost)
    let mut db2 = rustqlite::Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    {
        let values: Vec<String> = (1..=200i64).map(|i| format!("('w{}', {}, {})", i, i, i as f64)).collect();
        db2.execute(&format!("INSERT INTO t (name, val, score) VALUES {}", values.join(", ")), []).unwrap();
    }
    let start = Instant::now();
    for chunk_start in (1..=n).step_by(batch) {
        let chunk_end = (chunk_start + batch as i64 - 1).min(n);
        let values: Vec<String> = (chunk_start..=chunk_end)
            .map(|i| format!("('name{}', {}, {})", i, i * 2, i as f64 * 1.5))
            .collect();
        db2.execute(&format!("INSERT INTO t (name, val, score) VALUES {}", values.join(", ")), []).unwrap();
    }
    println!("without index: {:?}", start.elapsed());

    // Variant C: SQLite without index for reference
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)").unwrap();
    let start = Instant::now();
    for chunk_start in (1..=n).step_by(batch) {
        let chunk_end = (chunk_start + batch as i64 - 1).min(n);
        let values: Vec<String> = (chunk_start..=chunk_end)
            .map(|i| format!("('name{}', {}, {})", i, i * 2, i as f64 * 1.5))
            .collect();
        conn.execute(&format!("INSERT INTO t (name, val, score) VALUES {}", values.join(", ")), []).unwrap();
    }
    println!("sqlite no idx: {:?}", start.elapsed());
}
