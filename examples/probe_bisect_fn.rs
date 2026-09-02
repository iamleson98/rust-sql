//! Exact bisect: does wrapping storm+query in a function change the wake?
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

fn query_first_point(db: &mut Database) -> std::time::Duration {
    let t0 = Instant::now();
    let _ = db
        .query(
            "SELECT name, val, score FROM t WHERE id = ?",
            [Value::Integer(1)],
        )
        .unwrap();
    t0.elapsed()
}

fn main() {
    // variant A: storm, then timed query — direct code (bisect style)
    {
        let db = fresh_db();
        let d = {
            let t0 = Instant::now();
            let _ = db
                .query(
                    "SELECT name, val, score FROM t WHERE id = ?",
                    [Value::Integer(1)],
                )
                .unwrap();
            t0.elapsed()
        };
        println!("A direct:            {:>7.1} µs", d.as_secs_f64() * 1e6);
    }
    // variant B: storm, then timed query via a FUNCTION (rounds style)
    {
        let mut db = fresh_db();
        let d = query_first_point(&mut db);
        println!("B via function:      {:>7.1} µs", d.as_secs_f64() * 1e6);
    }
    // variant C: storm in a function, query direct
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
            "C storm-fn, direct:  {:>7.1} µs",
            t0.elapsed().as_secs_f64() * 1e6
        );
    }
}
