use std::time::Instant;
use rustqlite::{Database, Value};

fn run(n_rows: i64) {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..n_rows {
        db.execute("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Real(i as f64 * 0.5), Value::Text(format!("name-{i}").into())]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let n = 3000;
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT a FROM bench WHERE a = 0 LIMIT 1", ()); }
    println!("{n_rows:>6} rows: a=0 LIMIT 1 : {:>7.2} µs/q", t.elapsed().as_secs_f64()*1e6/n as f64);
}

fn main() {
    run(10);
    run(100);
    run(1000);
    run(5000);
}
