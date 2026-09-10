//! S06 step-path cost attribution probe.
//!
//! Splits the `SELECT id, val FROM t WHERE id BETWEEN 1 AND ?` step-path
//! time into: (A) full Statement::step machinery, (B) raw pooled selective
//! b-tree scan (decode only), (C) b-tree walk + rowid parse only (no record
//! decode) — the SQLite-column-accessor equivalent's floor.

use rustqlite::{Database, StepResult, Value};
use std::time::Instant;

fn build(rows: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    {
        let mut stmt = db
            .prepare("INSERT INTO t (name, val, score) VALUES (?, ?, ?)")
            .unwrap();
        for i in 1..=rows {
            stmt.bind(1, Value::Text(format!("name{i}").into()))
                .unwrap();
            stmt.bind(2, Value::Integer(i)).unwrap();
            stmt.bind(3, Value::Real(i as f64 * 1.5)).unwrap();
            while stmt.step().unwrap() == StepResult::Row {}
            stmt.reset();
        }
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn main() {
    let rows: i64 = 1_000_000;
    let span: i64 = 100_000;
    let iters = 5;
    let db = build(rows);

    engine_side(&db, span, iters);
    projections(&db, span, iters);
    sqlite_side(rows, span, iters);
}

fn time_variant(db: &Database, span: i64, iters: usize, sql: &str) -> f64 {
    let t = Instant::now();
    let mut acc = 0i64;
    for _ in 0..iters {
        let mut stmt = db.prepare(sql).unwrap();
        stmt.bind(1, Value::Integer(span)).unwrap();
        while let Ok(StepResult::Row) = stmt.step() {
            if let Some(r) = stmt.row() {
                if let Some(Value::Integer(v)) = r.first() {
                    acc = acc.wrapping_add(*v);
                }
            }
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let _ = acc;
    ms
}

fn projections(db: &Database, span: i64, iters: usize) {
    // Walk + rowid parse + Row serve floor (rowid alias: no record decode).
    let a = time_variant(db, span, iters, "SELECT id FROM t WHERE id BETWEEN 1 AND ?");
    // + 1 decoded column.
    let b = time_variant(
        db,
        span,
        iters,
        "SELECT id, val FROM t WHERE id BETWEEN 1 AND ?",
    );
    // + 2 decoded columns.
    let c = time_variant(
        db,
        span,
        iters,
        "SELECT id, val, score FROM t WHERE id BETWEEN 1 AND ?",
    );
    // + TEXT column (allocation path).
    let d = time_variant(
        db,
        span,
        iters,
        "SELECT id, name FROM t WHERE id BETWEEN 1 AND ?",
    );
    let rows = span as f64 * iters as f64;
    println!(
        "id only (walk+serve floor) : {:8.2} ms  {:6.1} ns/row",
        a,
        a * 1e6 / rows
    );
    println!(
        "id+val (+1 decode)         : {:8.2} ms  {:6.1} ns/row",
        b,
        b * 1e6 / rows
    );
    println!(
        "id+val+score (+2 decodes)  : {:8.2} ms  {:6.1} ns/row",
        c,
        c * 1e6 / rows
    );
    println!(
        "id+name (+TEXT alloc)      : {:8.2} ms  {:6.1} ns/row",
        d,
        d * 1e6 / rows
    );
}

fn engine_side(db: &Database, span: i64, iters: usize) {
    // --- (A) full step path
    let mut acc = 0i64;
    let t = Instant::now();
    for _ in 0..iters {
        let mut stmt = db
            .prepare("SELECT id, val FROM t WHERE id BETWEEN 1 AND ?")
            .unwrap();
        stmt.bind(1, Value::Integer(span)).unwrap();
        while let Ok(StepResult::Row) = stmt.step() {
            if let Some(r) = stmt.row() {
                if let Some(Value::Integer(v)) = r.get(1) {
                    acc = acc.wrapping_add(*v);
                }
            }
        }
    }
    let a_ms = t.elapsed().as_secs_f64() * 1000.0;

    println!("sink: {}", acc % 1000);
    println!(
        "A full step path           : {:8.2} ms  ({} rows/iter)",
        a_ms,
        span as f64 * iters as f64
    );
    let ns_per_row = a_ms * 1e6 / (span as f64 * iters as f64);
    println!("ns/row A                   : {:8.1}", ns_per_row);
}

fn sqlite_side(rows: i64, span: i64, iters: usize) {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    conn.execute("BEGIN", []).unwrap();
    {
        let mut stmt = conn
            .prepare("INSERT INTO t (name, val, score) VALUES (?, ?, ?)")
            .unwrap();
        for i in 1..=rows {
            stmt.execute(rusqlite::params![format!("name{i}"), i, (i as f64) * 1.5])
                .unwrap();
        }
    }
    conn.execute("COMMIT", []).unwrap();

    let mut acc = 0i64;
    let t = Instant::now();
    for _ in 0..iters {
        let mut stmt = conn
            .prepare("SELECT id, val FROM t WHERE id BETWEEN 1 AND ?1")
            .unwrap();
        let mut rows_it = stmt.query(rusqlite::params![span]).unwrap();
        while let Some(r) = rows_it.next().unwrap() {
            acc = acc.wrapping_add(r.get::<_, i64>(1).unwrap());
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("sqlite sink: {}", acc % 1000);
    println!("S sqlite step path         : {:8.2} ms", ms);
    println!(
        "ns/row sqlite              : {:8.1}",
        ms * 1e6 / (span as f64 * iters as f64)
    );
}
