//! Reproduce the Mixed 80/20 macOS CI regression: 4 reads + 1 write
//! interleaved on one connection, 5000 ops. Times phases separately.
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    // Same shape as bench_compare's section 1 setup (db_r).
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let q_sql = "SELECT name, val FROM t WHERE id = ?";
    let ins_sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    let upd_sql = "UPDATE t SET score = ? WHERE id = ?";

    // warm
    for i in 0..500 {
        let phase = i % 5;
        match phase {
            0..=3 => {
                let _ = db
                    .query(q_sql, [Value::Integer(((i % 1000) + 1) as i64)])
                    .unwrap();
            }
            _ => {
                db.execute(
                    ins_sql,
                    [
                        Value::Text(format!("w{i}").into()),
                        Value::Integer(i as i64),
                        Value::Real(i as f64),
                    ],
                )
                .unwrap();
            }
        }
    }

    // Full mixed run.
    let start = Instant::now();
    let mut next_id = 100_001i64;
    let mut q_ns: u128 = 0;
    let mut ins_ns: u128 = 0;
    let mut upd_ns: u128 = 0;
    for i in 0..5000usize {
        let phase = i % 5;
        match phase {
            0..=3 => {
                let target = ((i % 1000) + 1) as i64;
                let t = Instant::now();
                let _ = db.query(q_sql, [Value::Integer(target)]).unwrap();
                q_ns += t.elapsed().as_nanos();
            }
            4 => {
                if i % 2 == 0 {
                    next_id += 1;
                    let t = Instant::now();
                    db.execute(
                        ins_sql,
                        [
                            Value::Text(format!("new{}", next_id).into()),
                            Value::Integer(next_id * 2),
                            Value::Real(next_id as f64),
                        ],
                    )
                    .unwrap();
                    ins_ns += t.elapsed().as_nanos();
                } else {
                    let t = Instant::now();
                    db.execute(
                        upd_sql,
                        [
                            Value::Real(i as f64),
                            Value::Integer(((i % 1000) + 1) as i64),
                        ],
                    )
                    .unwrap();
                    upd_ns += t.elapsed().as_nanos();
                }
            }
            _ => unreachable!(),
        }
    }
    let total_ms = start.elapsed().as_secs_f64() * 1e3;
    println!("total: {total_ms:.2} ms");
    println!(
        "reads : {:7.2} ms (4000 x {:6.0} ns)",
        q_ns as f64 / 1e6,
        q_ns as f64 / 4000.0
    );
    println!(
        "inserts: {:7.2} ms (500 x {:6.0} ns)",
        ins_ns as f64 / 1e6,
        ins_ns as f64 / 500.0
    );
    println!(
        "updates: {:7.2} ms (500 x {:6.0} ns)",
        upd_ns as f64 / 1e6,
        upd_ns as f64 / 500.0
    );
}
