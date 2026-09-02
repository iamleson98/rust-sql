//! Probe: multi-row literal VALUES INSERT — fixed per-batch cost vs
//! per-row cost, and where the fixed cost goes (scanner parse vs
//! exec vs epilogue). Compares against SQLite at each batch size.

use rustqlite::Database;
use std::time::Instant;

fn time_batches(db: &mut Database, batch: usize, n_rows: usize, tag: &str) {
    let mut i = 1;
    let start = Instant::now();
    while i <= n_rows {
        let end = (i + batch - 1).min(n_rows);
        let values: String = (i..=end)
            .map(|j| format!("('name{}', {})", j, j))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("INSERT INTO t (name, val) VALUES {}", values);
        db.execute(&sql, []).unwrap();
        i = end + 1;
    }
    let d = start.elapsed();
    println!(
        "{:>10}: {:>8.2?}/row  ({:>9.1} rows/s)  batch={} total_rows={}",
        tag,
        d / n_rows as u32,
        n_rows as f64 / d.as_secs_f64(),
        batch,
        n_rows
    );
}

fn main() {
    // rustqlite
    {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        for &b in &[10usize, 50, 100, 500, 1000] {
            time_batches(&mut db, b, 10_000, "rustqlite");
        }
    }
    // sqlite
    {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        for &b in &[10usize, 50, 100, 500, 1000] {
            let mut i = 1;
            let n_rows = 10_000;
            let start = Instant::now();
            while i <= n_rows {
                let end = (i + b - 1).min(n_rows);
                let values: String = (i..=end)
                    .map(|j| format!("('name{}', {})", j, j))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!("INSERT INTO t (name, val) VALUES {}", values);
                conn.execute(&sql, []).unwrap();
                i = end + 1;
            }
            let d = start.elapsed();
            println!(
                "{:>10}: {:>8.2?}/row  ({:>9.1} rows/s)  batch={} total_rows={}",
                "sqlite",
                d / n_rows as u32,
                n_rows as f64 / d.as_secs_f64(),
                b,
                n_rows
            );
        }
    }

    // Scanner parse cost alone: try_fast_insert_parse is private, so
    // approximate with a 10000-row single statement vs 100 x 100-row
    // statements (same total rows, 100x the fixed cost).
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    let one_big: String = (1..=10_000)
        .map(|j| format!("('name{}', {})", j, j))
        .collect::<Vec<_>>()
        .join(",");
    let start = Instant::now();
    db.execute(&format!("INSERT INTO t (name, val) VALUES {}", one_big), [])
        .unwrap();
    println!(
        "one 10000-row statement: {:?} total ({:?}/row)",
        start.elapsed(),
        start.elapsed() / 10_000
    );
}
