//! Point-lookup shape attribution probe (CI diagnostic).
//!
//! Runs the EXACT query/table shapes of the two benches that disagree on
//! some CI hardware (notably Apple Silicon): bench_compare's
//! "Point lookup by rowid" (4-col table, single-row inserts, SELECT
//! name,val,score) vs bench_full_vs_sqlite's point lookup (3-col table,
//! multi-VALUES inserts, SELECT id,name,val) — plus the indexed variant.
//! Per-shape ns/query output makes the divergent stage obvious from the
//! CI log alone.

use std::time::Instant;

use rustqlite::{Database, Value};

fn best_of_10(mut f: impl FnMut() -> usize) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..10 {
        let start = Instant::now();
        let n = f();
        let d = start.elapsed();
        assert!(n > 0);
        if d < best {
            best = d;
        }
    }
    best
}

fn main() {
    // ---- Shape A: bench_compare's exact table + insert pattern --------
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    let sql_rowid = "SELECT name, val, score FROM t WHERE id = ?";
    let _ = db.query(sql_rowid, [Value::Integer(1)]).unwrap();
    let d = best_of_10(|| {
        let mut n = 0;
        for i in 1..=1000i64 {
            let target = (i % 1000) + 1;
            if db.query(sql_rowid, [Value::Integer(target)]).unwrap().len() == 1 {
                n += 1;
            }
        }
        n
    });
    println!(
        "shape A rowid    (bench_compare): {:?}/1000 = {:?}/query",
        d,
        d / 1000
    );

    let sql_idx = "SELECT name, val, score FROM t WHERE val = ?";
    let _ = db.query(sql_idx, [Value::Integer(2)]).unwrap();
    let d = best_of_10(|| {
        let mut n = 0;
        for i in 1..=1000i64 {
            let target = (i % 1000) + 1;
            n += db.query(sql_idx, [Value::Integer(target)]).unwrap().len();
        }
        n
    });
    println!(
        "shape A indexed  (bench_compare): {:?}/1000 = {:?}/query",
        d,
        d / 1000
    );

    // ---- Shape B: bench_full_vs_sqlite's exact table + insert pattern --
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    let chunk = 100;
    let mut i = 1i64;
    while i <= 10_000 {
        let end = (i + chunk - 1).min(10_000);
        let values: String = (i..=end)
            .map(|j| format!("('name{}', {})", j, j))
            .collect::<Vec<_>>()
            .join(",");
        db2.execute(&format!("INSERT INTO t (name, val) VALUES {}", values), [])
            .unwrap();
        i = end + 1;
    }
    let sql_b = "SELECT id, name, val FROM t WHERE id = ?";
    let _ = db2.query(sql_b, [Value::Integer(1)]).unwrap();
    let d = best_of_10(|| {
        let mut n = 0;
        for i in 1..=1000i64 {
            let target = (i % 1000) + 1;
            if db2.query(sql_b, [Value::Integer(target)]).unwrap().len() == 1 {
                n += 1;
            }
        }
        n
    });
    println!(
        "shape B rowid    (gate 1):        {:?}/1000 = {:?}/query",
        d,
        d / 1000
    );
}
