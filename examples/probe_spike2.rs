//! Isolate the post-sleep spike: is it mimalloc purge or CPU idle wake-up?
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 / 1e3
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("user{}", i).into()), Value::Integer(i), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    for _ in 0..1000 {
        let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    }

    // Case A: sleep with NO prior free storm (steady loop then sleep).
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    println!("steady (no sleep):        {:>7.2} us", us(start.elapsed()));
    std::thread::sleep(std::time::Duration::from_millis(50));
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    println!("after 50ms sleep:         {:>7.2} us", us(start.elapsed()));

    // Case B: re-warm, then free storm (drop big Vecs), then sleep.
    for _ in 0..1000 {
        let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    }
    {
        let mut v: Vec<Vec<u8>> = Vec::new();
        for i in 0..1000 {
            v.push(vec![0u8; (i % 64) + 16]);
        }
        drop(v);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    println!("storm + 50ms sleep:       {:>7.2} us", us(start.elapsed()));

    // Case C: does a pure computational lambda also spike after sleep?
    // (isolates CPU frequency / C-state wake latency)
    let mut acc = 0u64;
    let mut f = |x: u64| {
        let mut s = x;
        for _ in 0..200 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        }
        s
    };
    for _ in 0..1000 {
        acc ^= f(acc);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let start = Instant::now();
    acc ^= f(acc);
    println!("pure compute after sleep: {:>7.2} us  (CPU wake cost)  acc={}", us(start.elapsed()), acc & 1);

    // Case D: bench pattern — 1000 point lookups (free storm via results),
    // no sleep, immediate range query.
    let sql_pt = "SELECT name, val, score FROM t WHERE id = ?";
    for _ in 0..1000 {
        let _ = db.query(sql_pt, [Value::Integer(500)]).unwrap();
    }
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    println!("storm, no sleep:          {:>7.2} us", us(start.elapsed()));
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    println!("second query:             {:>7.2} us", us(start.elapsed()));
}
