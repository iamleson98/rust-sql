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
    let cases: Vec<(&str, Box<dyn Fn()>)> = vec![
        ("SELECT a FROM bench LIMIT 1", Box::new(|| { let _ = db.query("SELECT a FROM bench LIMIT 1", ()); })),
    ];
    for (name, f) in cases { let t = Instant::now(); for _ in 0..n { f(); } println!("{name:40} {:>7.2} µs/q", t.elapsed().as_secs_f64()*1e6/n as f64); }

    // point lookup baseline (fast path)
    let t = Instant::now();
    for i in 0..n { let _ = db.query("SELECT a FROM bench WHERE id = ?", [Value::Integer((i % 5000) as i64)]); }
    println!("{:40} {:>7.2} µs/q", "SELECT a WHERE id=? (fast path)", t.elapsed().as_secs_f64()*1e6/n as f64);

    // count fast path
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT COUNT(*) FROM bench WHERE a > 4500", ()); }
    println!("{:40} {:>7.2} µs/q", "COUNT(*) WHERE a>4500", t.elapsed().as_secs_f64()*1e6/n as f64);

    // between limit 1 at start
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT a FROM bench WHERE a BETWEEN 0 AND 100 LIMIT 1", ()); }
    println!("{:40} {:>7.2} µs/q", "BETWEEN 0..100 LIMIT 1", t.elapsed().as_secs_f64()*1e6/n as f64);

    // between limit 1 with bind
    let t = Instant::now();
    for _ in 0..n { let _ = db.query("SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1", [Value::Integer(0), Value::Integer(100)]); }
    println!("{:40} {:>7.2} µs/q", "BETWEEN 0..100 LIMIT 1 (bind)", t.elapsed().as_secs_f64()*1e6/n as f64);
}
