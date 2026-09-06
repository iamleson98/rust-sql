//! S06 shape: 1M rows, 5 iterations of a 100K-row range scan via the
//! streaming step() API vs rusqlite's SQLite. Isolates per-row costs.
use rustqlite::{Database, StepResult, Value};

fn build_rq(n: i64) -> Database {
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

    // --- rustqlite ---
    let db = build_rq(n);
    let mut acc = 0i64;
    // warm
    {
        let mut stmt = db
            .prepare("SELECT id, val FROM t WHERE id BETWEEN 1 AND ?")
            .unwrap();
        stmt.bind(1, Value::Integer(1000)).unwrap();
        while let Ok(StepResult::Row) = stmt.step() {
            if let Some(r) = stmt.row() {
                if let Some(Value::Integer(v)) = r.get(1) {
                    acc = acc.wrapping_add(*v);
                }
            }
        }
    }
    let t = std::time::Instant::now();
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
    let rq_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "rustqlite: {rq_ms:6.2} ms  ({:.1} ns/row)  acc={}",
        rq_ms * 1e6 / (span as f64 * iters as f64),
        acc % 1000
    );

    // --- SQLite via rusqlite ---
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF;")
        .unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    let mut acc2 = 0i64;
    {
        let txn = conn.transaction().unwrap();
        {
            let mut stmt = txn.prepare("INSERT INTO t (val) VALUES (?1)").unwrap();
            for i in 1..=n {
                stmt.execute(rusqlite::params![i * 3]).unwrap();
            }
        }
        txn.commit().unwrap();
    }
    let t = std::time::Instant::now();
    for _ in 0..iters {
        let mut stmt = conn
            .prepare("SELECT id, val FROM t WHERE id BETWEEN 1 AND ?1")
            .unwrap();
        let mut rows = stmt.query(rusqlite::params![span]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            acc2 = acc2.wrapping_add(r.get::<_, i64>(1).unwrap());
        }
    }
    let sq_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "sqlite  : {sq_ms:6.2} ms  ({:.1} ns/row)  acc={}",
        sq_ms * 1e6 / (span as f64 * iters as f64),
        acc2 % 1000
    );
    println!("ratio: {:.2}x (rustqlite/sqlite)", rq_ms / sq_ms);
}
