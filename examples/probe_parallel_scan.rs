//! Probe: intra-statement parallel aggregation + top-N scaling.
//!
//! Builds a 1M-row table, then times the same big aggregate / top-N
//! queries with `PRAGMA parallel_scan=0` (serial) vs the default ON
//! (worker split). Prints per-query times, speedup, and the worker
//! count.

use std::time::Instant;

use rustqlite::{Database, Value};

const ROWS: i64 = 1_000_000;

fn build() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, r REAL, cat INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let mut i = 0i64;
    while i < ROWS {
        let hi = (i + 5_000).min(ROWS);
        let mut sql = String::from("INSERT INTO t (id, v, r, cat) VALUES ");
        for j in i..hi {
            if j > i {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, {}, {}, {})",
                j,
                (j * 37 + 11) % 1_000_000,
                (j as f64) * 0.25,
                j % 100
            ));
        }
        db.execute(&sql, []).unwrap();
        i = hi;
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn time_query(db: &Database, sql: &str, iters: u32) -> f64 {
    // Warmup (page cache, plan cache, thread pool spin-up).
    let _ = db.query(sql, []);
    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        let rows = db.query(sql, []).unwrap();
        let dt = t.elapsed().as_secs_f64() * 1e3;
        // Touch the result so the query cannot be elided.
        if let Value::Integer(n) = &rows[0][0] {
            assert!(*n > 0, "COUNT must be positive");
        }
        best = best.min(dt);
    }
    best
}

fn time_groupby(db: &Database, sql: &str, iters: u32) -> f64 {
    let _ = db.query(sql, []);
    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        let rows = db.query(sql, []).unwrap();
        let dt = t.elapsed().as_secs_f64() * 1e3;
        assert_eq!(rows.len(), 100, "100 groups");
        best = best.min(dt);
    }
    best
}

fn time_topn(db: &Database, sql: &str, iters: u32) -> f64 {
    let _ = db.query(sql, []);
    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        let rows = db.query(sql, []).unwrap();
        let dt = t.elapsed().as_secs_f64() * 1e3;
        assert_eq!(rows.len(), 25, "top-25 rows");
        best = best.min(dt);
    }
    best
}

fn main() {
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("hardware threads: {hw}, rows: {ROWS}");
    println!();

    let mut ser = build();
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();
    let par = build();

    let queries = [
        ("COUNT(*) bare          ", "SELECT COUNT(*) FROM t"),
        (
            "COUNT+SUM+AVG+MIN+MAX  ",
            "SELECT COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM t",
        ),
        (
            "filtered aggregate     ",
            "SELECT COUNT(*), SUM(v), AVG(r) FROM t WHERE v > 500000",
        ),
        (
            "REAL aggregate         ",
            "SELECT SUM(r), AVG(r), MIN(r), MAX(r) FROM t",
        ),
    ];
    for (name, sql) in queries {
        let s = time_query(&ser, sql, 7);
        let p = time_query(&par, sql, 7);
        println!(
            "{name} serial {s:8.3} ms   parallel {p:8.3} ms   speedup {:5.2}x",
            s / p
        );
    }

    let g = "SELECT cat, COUNT(*), SUM(v), AVG(r) FROM t GROUP BY cat";
    let s = time_groupby(&ser, g, 7);
    let p = time_groupby(&par, g, 7);
    println!(
        "GROUP BY 100 buckets    serial {s:8.3} ms   parallel {p:8.3} ms   speedup {:5.2}x",
        s / p
    );

    // Top-N shapes: unique REAL key (r = j/4), full-row projection.
    let t25 = "SELECT id, v, r, cat FROM t ORDER BY r DESC LIMIT 25";
    let s = time_topn(&ser, t25, 7);
    let p = time_topn(&par, t25, 7);
    println!(
        "top-25 ORDER BY r DESC  serial {s:8.3} ms   parallel {p:8.3} ms   speedup {:5.2}x",
        s / p
    );
    let a = ser.query(t25, []).unwrap();
    let b = par.query(t25, []).unwrap();
    assert_eq!(a, b, "parallel top-N vs serial mismatch");

    // Same answer check (cheap sanity on top of tests/parallel_scan.rs).
    let a = ser
        .query("SELECT COUNT(*), SUM(v) FROM t WHERE v > 500000", [])
        .unwrap();
    let b = par
        .query("SELECT COUNT(*), SUM(v) FROM t WHERE v > 500000", [])
        .unwrap();
    assert_eq!(a, b, "parallel vs serial mismatch");
    println!("\nresults identical: {a:?}");
}
