//! Isolate S06 per-row costs: (a) raw selective scan (sum only),
//! (b) full step() path, (c) step() with row-buffer pool bypassed.
use rustqlite::{Database, StepResult, Value};

fn build(n: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i * 3)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn main() {
    let n = 1_000_000i64;
    let span = 100_000i64;
    let iters = 5;
    let db = build(n);

    // warm statement machinery
    {
        let mut stmt = db
            .prepare("SELECT id, val FROM t WHERE id BETWEEN 1 AND ?")
            .unwrap();
        stmt.bind(1, Value::Integer(1000)).unwrap();
        while let Ok(StepResult::Row) = stmt.step() {
            let _ = stmt.row();
        }
    }

    // (b) full step path
    let t = std::time::Instant::now();
    let mut acc = 0i64;
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
    let step_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "step() path : {step_ms:6.2} ms ({:.1} ns/row)",
        step_ms * 1e6 / (span as f64 * iters as f64)
    );

    // (a) raw scan via query (materialized, same decode work)
    let t = std::time::Instant::now();
    let mut acc2 = 0i64;
    for _ in 0..iters {
        let rows = db
            .query("SELECT id, val FROM t WHERE id BETWEEN 1 AND 100000", [])
            .unwrap();
        for r in &rows {
            if let Some(Value::Integer(v)) = r.get(1) {
                acc2 = acc2.wrapping_add(*v);
            }
        }
    }
    let q_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "query() path: {q_ms:6.2} ms ({:.1} ns/row) incl. materialize+drop",
        q_ms * 1e6 / (span as f64 * iters as f64)
    );
    println!("acc {acc} {acc2}");
}
