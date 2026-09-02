//! Probe #2: profile each stage of an UPDATE-by-PK op.
//! Compare: parse-only, parse+plan, full execute (rowid lookup only),
//! full execute (delete+insert = UPDATE).

use std::time::{Duration, Instant};

fn fmt(d: Duration) -> String {
    if d.as_millis() > 0 {
        format!("{:.3}ms", d.as_secs_f64() * 1e3)
    } else if d.as_micros() > 0 {
        format!("{:.3}us", d.as_secs_f64() * 1e6)
    } else {
        format!("{}ns", d.as_nanos())
    }
}

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!("INSERT INTO t (val, score) VALUES ({}, {})", i, i as f64);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    println!("Loaded 10k rows.");

    // Stage A: parse-only via a fresh in-memory DB each time (no executor side effects).
    // This isolates the parse+plan+open cost.
    let sql = "UPDATE t SET score = 1.5 WHERE id = 5";
    let n = 1000;
    let start = Instant::now();
    for _ in 0..n {
        let mut tmp = rustqlite::Database::open_in_memory().unwrap();
        let _ = tmp.execute(sql, []);
    }
    let parse_only = start.elapsed();
    println!(
        "{}x parse+plan (no-op):      {}  ({}/op)",
        n,
        fmt(parse_only),
        fmt(parse_only / n as u32)
    );

    // Stage B: parse + execute SELECT rowid lookup (read-only)
    let start = Instant::now();
    for i in 1..=n as i64 {
        let sql = format!("SELECT * FROM t WHERE id = {}", (i % 10000) + 1);
        let _ = db.query(&sql, []).unwrap();
    }
    let select_pk = start.elapsed();
    println!(
        "{}x SELECT by PK (full path): {}  ({}/op)",
        n,
        fmt(select_pk),
        fmt(select_pk / n as u32)
    );

    // Stage C: parse + execute UPDATE
    let start = Instant::now();
    for i in 1..=n as i64 {
        let sql = format!(
            "UPDATE t SET score = {} WHERE id = {}",
            i as f64 * 2.5,
            (i % 10000) + 1
        );
        db.execute(&sql, []).unwrap();
    }
    let update_pk = start.elapsed();
    println!(
        "{}x UPDATE by PK (full path): {}  ({}/op)",
        n,
        fmt(update_pk),
        fmt(update_pk / n as u32)
    );

    // Stage D: 1k INSERTs (already shown in main bench to be ~13us each in txn)
    let start = Instant::now();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n as i64 {
        let sql = format!(
            "INSERT INTO t (val, score) VALUES ({}, {})",
            i + 50000,
            i as f64
        );
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let insert_txn = start.elapsed();
    println!(
        "{}x INSERT in BEGIN/COMMIT:  {}  ({}/op)",
        n,
        fmt(insert_txn),
        fmt(insert_txn / n as u32)
    );

    println!();
    println!("--- Summary ---");
    println!(
        "parse-only cost:    {:>8}/op  (inherent baseline)",
        fmt(parse_only / n as u32)
    );
    println!(
        "select-by-pk cost:  {:>8}/op  (parse + plan + execute read)",
        fmt(select_pk / n as u32)
    );
    println!(
        "update-by-pk cost:  {:>8}/op  (parse + plan + execute write)",
        fmt(update_pk / n as u32)
    );
    println!(
        "delta(select→up):   {:>8}/op  (delete+insert overhead)",
        fmt((update_pk - select_pk) / n as u32)
    );
    println!(
        "insert-in-txn cost: {:>8}/op  (parse + plan + execute insert)",
        fmt(insert_txn / n as u32)
    );
}

// (No external helpers needed — `parse_stmt` is approximated inline above
// by running execute() against a fresh in-memory DB.)
