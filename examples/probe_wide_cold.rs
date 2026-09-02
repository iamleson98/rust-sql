//! COLD-process test: does the WIDE tap (8..1024+) fully absorb the wake?
use rustqlite::{Database, Value};
use std::time::Instant;

fn fresh_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(
            sql,
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn main() {
    // COLD: wide tap FIRST thing in the process (only prior work: the insert txn)
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let mut sink: Vec<Vec<u8>> = Vec::with_capacity(384);
        for i in 0..384u32 {
            // cycle 8..128 (small classes) and 256..2048 (page/vec classes)
            let sz = if i % 3 == 0 {
                ((i * 13) % 128 + 8) as usize
            } else if i % 3 == 1 {
                256 + ((i * 29) % 768) as usize
            } else {
                1024 + ((i * 17) % 1024) as usize
            };
            sink.push(vec![0u8; sz]);
        }
        drop(sink);
        let t1 = Instant::now();
        let _ = db
            .query(
                "SELECT name, val, score FROM t WHERE id = ?",
                [Value::Integer(1)],
            )
            .unwrap();
        println!(
            "COLD wide tap: tap {:>7.1} µs, query {:>7.1} µs",
            t0.elapsed().as_secs_f64() * 1e6,
            t1.elapsed().as_secs_f64() * 1e6
        );
    }
    // COLD control: no tap
    {
        let db = fresh_db();
        let t0 = Instant::now();
        let _ = db
            .query(
                "SELECT name, val, score FROM t WHERE id = ?",
                [Value::Integer(1)],
            )
            .unwrap();
        println!(
            "COLD no tap:   query {:>7.1} µs",
            t0.elapsed().as_secs_f64() * 1e6
        );
    }
}
