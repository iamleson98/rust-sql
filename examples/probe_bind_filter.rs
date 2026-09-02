//! Probe: bind-parameter range filter vs literal range filter — does the
//! vectorized scan filter handle ParameterRef comparisons?

use std::time::Instant;
use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..5000i64 {
        db.execute(
            "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Real(i as f64 * 0.5), Value::Text(format!("name-{i}").into())],
        ).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let n = 2000;

    // literal range + agg
    let t = Instant::now();
    for i in 0..n {
        let lo = (i * 2) % 4000;
        let sql = format!("SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN {lo} AND {lo}+50");
        let _ = db.query(&sql, []);
    }
    println!("literal BETWEEN + COUNT/AVG : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);

    // bound range + agg
    let t = Instant::now();
    for i in 0..n {
        let lo = ((i * 2) % 4000) as i64;
        let _ = db.query("SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?", [Value::Integer(lo), Value::Integer(lo+50)]);;
    }
    println!("bind    BETWEEN + COUNT/AVG : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);

    // literal range, plain select
    let t = Instant::now();
    for i in 0..n {
        let lo = (i * 2) % 4000;
        let sql = format!("SELECT a FROM bench WHERE a BETWEEN {lo} AND {lo}+50");
        let _ = db.query(&sql, []);
    }
    println!("literal BETWEEN (no agg)    : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);

    // bound range, plain select
    let t = Instant::now();
    for i in 0..n {
        let lo = ((i * 2) % 4000) as i64;
        let _ = db.query("SELECT a FROM bench WHERE a BETWEEN ? AND ?", [Value::Integer(lo), Value::Integer(lo+50)]);;
    }
    println!("bind    BETWEEN (no agg)    : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);

    // bound equality point filter (non-indexed)
    let t = Instant::now();
    for i in 0..n {
        let v = (i * 7) % 5000;
        let _ = db.query("SELECT a FROM bench WHERE a = ?", [Value::Integer(v)]);
    }
    println!("bind    equality (no idx)   : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);

    // literal inequality
    let t = Instant::now();
    for _ in 0..n {
        let _ = db.query("SELECT COUNT(*) FROM bench WHERE a > 4500", []);
    }
    println!("literal a > 4500 + COUNT    : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);

    // bound inequality
    let t = Instant::now();
    for _ in 0..n {
        let _ = db.query("SELECT COUNT(*) FROM bench WHERE a > ?", [Value::Integer(4500)]);
    }
    println!("bind    a > 4500 + COUNT    : {:>8.1} ms ({:>6.1} µs/q)", t.elapsed().as_secs_f64()*1e3, t.elapsed().as_secs_f64()*1e6/n as f64);
}
