use std::time::Instant;
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..5000i64 {
        db.execute("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Real(i as f64 * 0.5), Value::Text(format!("name-{i}").into())]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let n = 5000;

    // (a) full query() each time
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT a FROM bench WHERE a BETWEEN 0 AND 100 LIMIT 1", ()); }
    println!("query()          : {:>7.2} µs/q", t.elapsed().as_secs_f64()*1e6/n as f64);

    // (b) prepare once + bind/step per iteration
    {
        let mut stmt = db.prepare("SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1").unwrap();
        let t = Instant::now();
        for _ in 0..n {
            stmt.bind_all(&[Value::Integer(0), Value::Integer(100)]);
            let _ = stmt.step();
        }
        println!("prepare+step     : {:>7.2} µs/q", t.elapsed().as_secs_f64()*1e6/n as f64);
    }

    // (c) query() but with the WHERE on the rowid PK (indexed, no scan)
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT a FROM bench WHERE id BETWEEN 0 AND 1 LIMIT 1", ()); }
    println!("rowid BETWEEN    : {:>7.2} µs/q", t.elapsed().as_secs_f64()*1e6/n as f64);

    // (d) query() with equality on non-indexed a (full scan, 1 match)
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT a FROM bench WHERE a = 0 LIMIT 1", ()); }
    println!("a=0 LIMIT 1      : {:>7.2} µs/q", t.elapsed().as_secs_f64()*1e6/n as f64);
}
