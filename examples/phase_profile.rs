//! Phase-level profile of INSERT path: parse / plan / cache / exec.

use rustqlite::Database;

fn main() {
    rustqlite::api::profile::ENABLED.store(true, std::sync::atomic::Ordering::SeqCst);
    let n = 3000;
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();

    // Warm up
    for i in 0..500 {
        let sql = format!("INSERT INTO t (name, val) VALUES ('w{}', {})", i, i);
        db.execute(&sql, []).unwrap();
    }

    db.execute("BEGIN", []).unwrap();
    let sqls: Vec<String> = (1..=n)
        .map(|i| format!("INSERT INTO t (name, val) VALUES ('name{}', {})", i, i))
        .collect();
    let t0 = std::time::Instant::now();
    for sql in &sqls {
        db.execute(sql, []).unwrap();
    }
    let full = t0.elapsed();
    db.execute("COMMIT", []).unwrap();
    println!(
        "full path (txn)  : {:>9.2?}  ({:.3} us/insert)",
        full,
        full.as_nanos() as f64 / n as f64 / 1000.0
    );
    rustqlite::api::profile::dump();

    // Same with cache DISABLED to isolate cache overhead
    db.set_stmt_cache_capacity(0);
    db.execute(
        "CREATE TABLE t2 (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let t1 = std::time::Instant::now();
    for i in 1..=n {
        let sql = format!("INSERT INTO t2 (name, val) VALUES ('name{}', {})", i, i);
        db.execute(&sql, []).unwrap();
    }
    let full2 = t1.elapsed();
    db.execute("COMMIT", []).unwrap();
    println!(
        "no-cache (txn)   : {:>9.2?}  ({:.3} us/insert)",
        full2,
        full2.as_nanos() as f64 / n as f64 / 1000.0
    );
}
