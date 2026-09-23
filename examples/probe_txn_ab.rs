//! Exact-shape A/B for the bench_full_vs_sqlite INSERT rows (macOS gate
//! loss): fresh in-memory DB per iteration, BEGIN + 1k single-row `?`-param
//! INSERTs + COMMIT (txn variant) or 1k autocommit INSERTs (autocommit
//! variant), 2 warmup + 5 measured iterations — identical to the bench
//! harness. Prints ns/row for rustqlite plus a rusqlite reference.
use rusqlite::params;
use rustqlite::Value;
use std::time::Instant;

fn run_txn_rustqlite(iters_label: &str) -> f64 {
    let n = 1000usize;
    let mut all = Vec::new();
    // warmup x2 + measure x5, like bench_full's `measure`
    for it in 0..7 {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        let start = Instant::now();
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [
                    Value::Text(format!("name{}", i).into()),
                    Value::Integer(i as i64),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        let el = start.elapsed();
        // include BEGIN/COMMIT + open/create like the bench's closure does
        all.push(el.as_secs_f64() * 1e9 / n as f64);
        let _ = it;
    }
    let body: Vec<f64> = all[2..].to_vec();
    let best = body.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = body.iter().sum::<f64>() / body.len() as f64;
    println!(
        "{:44} best {:8.1} ns/row   mean {:8.1} ns/row",
        iters_label, best, mean
    );
    mean
}

fn run_autocommit_rustqlite(iters_label: &str) -> f64 {
    let n = 1000usize;
    let mut all = Vec::new();
    for _ in 0..7 {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        let start = Instant::now();
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [
                    Value::Text(format!("name{}", i).into()),
                    Value::Integer(i as i64),
                ],
            )
            .unwrap();
        }
        let el = start.elapsed();
        all.push(el.as_secs_f64() * 1e9 / n as f64);
    }
    let body: Vec<f64> = all[2..].to_vec();
    let best = body.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = body.iter().sum::<f64>() / body.len() as f64;
    println!(
        "{:44} best {:8.1} ns/row   mean {:8.1} ns/row",
        iters_label, best, mean
    );
    mean
}

fn run_txn_sqlite() -> f64 {
    let n = 1000usize;
    let mut all = Vec::new();
    for _ in 0..7 {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        conn.execute("BEGIN", []).unwrap();
        let start = Instant::now();
        for i in 1..=n {
            conn.execute(
                "INSERT INTO t (name, val) VALUES (?1, ?2)",
                params![format!("name{}", i), i as i64],
            )
            .unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
        let el = start.elapsed();
        all.push(el.as_secs_f64() * 1e9 / n as f64);
    }
    let body: Vec<f64> = all[2..].to_vec();
    let best = body.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = body.iter().sum::<f64>() / body.len() as f64;
    println!(
        "{:44} best {:8.1} ns/row   mean {:8.1} ns/row",
        "sqlite txn (reference)", best, mean
    );
    mean
}

fn main() {
    println!("== rustqlite ==");
    let rq_auto = run_autocommit_rustqlite("rustqlite autocommit 1k (bench shape)");
    let rq_txn = run_txn_rustqlite("rustqlite txn 1k (bench shape)");
    let sq_txn = run_txn_sqlite();
    println!();
    println!(
        "txn vs autocommit overhead: {:+.1} ns/row",
        rq_txn - rq_auto
    );
    println!(
        "txn ratio vs sqlite: {:.2}x (need >= 0.95)",
        sq_txn / rq_txn
    );
}
