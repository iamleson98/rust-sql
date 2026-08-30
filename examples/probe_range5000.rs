//! Decompose range-5000 per-row cost: scan-only vs decode+materialize.
use std::time::Instant;
use rustqlite::types::Value;
use rustqlite::Database;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 * 1e-3
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("name{}", i).into()), Value::Integer(i * 2), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    // warm
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    let n = 300;
    let mut full = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for _ in 0..n {
            let _ = db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
        }
        full = full.min(us(start.elapsed()) / n as f64);
    }
    println!("full 5000-row query:      {:>8.1} us  ({:.1} ns/row)", full, full * 1000.0 / 5000.0);

    // COUNT over the same range: same scan, no row materialization
    // (aggregation counts matches without building rows).
    let sql2 = "SELECT COUNT(*) FROM t WHERE id BETWEEN ? AND ?";
    let _ = db.query(sql2, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    let mut count = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for _ in 0..n {
            let _ = db.query(sql2, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
        }
        count = count.min(us(start.elapsed()) / n as f64);
    }
    println!("COUNT same range:         {:>8.1} us  ({:.1} ns/row)", count, count * 1000.0 / 5000.0);

    // SUM over the range: decode ONE column per row (val), no Text.
    let sql3 = "SELECT SUM(val) FROM t WHERE id BETWEEN ? AND ?";
    let _ = db.query(sql3, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
    let mut sum1 = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        for _ in 0..n {
            let _ = db.query(sql3, [Value::Integer(1000), Value::Integer(5999)]).unwrap();
        }
        sum1 = sum1.min(us(start.elapsed()) / n as f64);
    }
    println!("SUM(val) same range:      {:>8.1} us  ({:.1} ns/row)", sum1, sum1 * 1000.0 / 5000.0);

    // narrower ranges for scaling reference
    for (lo, hi, label) in [(1000i64, 1099i64, "100"), (1000, 1999, "1000")] {
        let w = (hi - lo + 1) as f64;
        let _ = db.query(sql, [Value::Integer(lo), Value::Integer(hi)]).unwrap();
        let mut t = f64::MAX;
        for _ in 0..3 {
            let start = Instant::now();
            for _ in 0..n {
                let _ = db.query(sql, [Value::Integer(lo), Value::Integer(hi)]).unwrap();
            }
            t = t.min(us(start.elapsed()) / n as f64);
        }
        println!("full {}-row query:      {:>8.1} us  ({:.1} ns/row)", label, t, t * 1000.0 / w);
    }

    // Does the result SIZE matter? query and immediately drop vs query and
    // keep: measures drop cost.
    let start = Instant::now();
    let mut keep: Option<Vec<_>> = None;
    for _ in 0..n { keep = Some(db.query(sql, [Value::Integer(1000), Value::Integer(5999)]).unwrap()); }
    println!("(query+keep last:        {:>8.1} us)", us(start.elapsed()) / n as f64);
    drop(keep);
}
