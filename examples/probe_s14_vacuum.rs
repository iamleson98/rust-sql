//! Profile the S14 churn+reclaim VACUUM: 500K rows, delete 90%, VACUUM.
//! Times the whole execute("VACUUM") plus phase-level breakdown hints.
use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let rows = 500_000i64;
    let keep = rows / 10;
    let path = "/tmp/probe_s14.rq.db";
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
    let mut db = Database::open(path).unwrap();
    db.execute("PRAGMA journal_mode=WAL", []).unwrap();
    db.execute("PRAGMA synchronous=OFF", []).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let t = Instant::now();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=rows {
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
    println!("insert: {:7.1} ms", t.elapsed().as_secs_f64() * 1e3);

    let t = Instant::now();
    db.execute("DELETE FROM t WHERE id > ?", [Value::Integer(keep)])
        .unwrap();
    println!("delete: {:7.1} ms", t.elapsed().as_secs_f64() * 1e3);

    let t = Instant::now();
    let vac = db.execute("VACUUM", []);
    let ms = t.elapsed().as_secs_f64() * 1e3;
    println!("vacuum: {:7.1} ms (ok={})", ms, vac.is_ok());

    let cnt: i64 = match db
        .query("SELECT COUNT(*) FROM t", [])
        .unwrap()
        .first()
        .and_then(|r| r.first())
    {
        Some(Value::Integer(n)) => *n,
        other => panic!("{other:?}"),
    };
    println!("count after: {cnt} (want {keep})");
    let sz = std::fs::metadata(path).unwrap().len() as f64 / 1024.0 / 1024.0;
    println!("file after: {sz:.2} MB");

    // Second vacuum on the already-compact DB (steady-state shape).
    let t = Instant::now();
    db.execute("VACUUM", []).unwrap();
    println!(
        "vacuum2: {:7.1} ms (steady state)",
        t.elapsed().as_secs_f64() * 1e3
    );
}
