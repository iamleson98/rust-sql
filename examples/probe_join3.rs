//! Decompose the 3-table join single-shot cost: bench pattern (1 warmup +
//! 1 timed) vs steady state, and page-residency effects.
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 / 1e3
}

fn setup() -> Database {
    let mut dbj = Database::open_in_memory().unwrap();
    dbj.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)", []).unwrap();
    dbj.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL)", []).unwrap();
    dbj.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)", []).unwrap();
    dbj.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        dbj.execute("INSERT INTO users VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Text(format!("u{}", i).into()), Value::Text(format!("d{}", i % 10).into())]).unwrap();
    }
    for i in 1..=10000i64 {
        dbj.execute("INSERT INTO orders VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Integer((i % 1000) + 1), Value::Real(i as f64)]).unwrap();
    }
    for i in 1..=50000i64 {
        dbj.execute("INSERT INTO items VALUES (?, ?, ?, ?)",
            [Value::Integer(i), Value::Integer((i % 10000) + 1), Value::Text(format!("item{}", i).into()), Value::Real(i as f64 * 0.5)]).unwrap();
    }
    dbj.execute("COMMIT", []).unwrap();
    dbj.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
    dbj.execute("CREATE INDEX idx_items_order ON items(order_id)", []).unwrap();
    dbj
}

fn main() {
    let sql = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = ?";

    // Decay pattern: warm once, then time 8 consecutive new-param queries.
    {
        let mut db = setup();
        let _ = db.query(sql, [Value::Integer(1)]).unwrap();
        for k in [300u64, 301, 302, 303, 400, 401, 700, 701] {
            let start = Instant::now();
            let _ = db.query(sql, [Value::Integer(k as i64)]).unwrap();
            println!("decay: param={k:>4}: {:>8.1} us", us(start.elapsed()));
        }
    }

    // Case 1: bench pattern — exactly one warmup (param 1), then time param 500.
    {
        let mut db = setup();
        let _ = db.query(sql, [Value::Integer(1)]).unwrap();
        let start = Instant::now();
        let _ = db.query(sql, [Value::Integer(500)]).unwrap();
        println!("bench pattern (warm=1, time=500):  {:>8.1} us", us(start.elapsed()));
    }
    // Case 2: warmup with SAME param, then time.
    {
        let mut db = setup();
        let _ = db.query(sql, [Value::Integer(500)]).unwrap();
        let m0 = db.pager().cache_misses();
        let start = Instant::now();
        let _ = db.query(sql, [Value::Integer(500)]).unwrap();
        println!("same-param warm + timed:           {:>8.1} us (misses +{})", us(start.elapsed()), db.pager().cache_misses() - m0);
    }
    // Case 1b: how many misses does the new-param query pay?
    {
        let mut db = setup();
        let _ = db.query(sql, [Value::Integer(1)]).unwrap();
        let m0 = db.pager().cache_misses();
        let start = Instant::now();
        let _ = db.query(sql, [Value::Integer(500)]).unwrap();
        println!("bench pattern:                     {:>8.1} us (misses +{})", us(start.elapsed()), db.pager().cache_misses() - m0);
    }
    // Case 3: steady state (500 iterations of param 500).
    {
        let mut db = setup();
        for _ in 0..20 {
            let _ = db.query(sql, [Value::Integer(500)]).unwrap();
        }
        let start = Instant::now();
        for _ in 0..500 {
            let _ = db.query(sql, [Value::Integer(500)]).unwrap();
        }
        println!("steady state avg:                 {:>8.1} us", us(start.elapsed()) / 500.0);
    }
    // Case 4: interleaved single shots, params cycling 100..600 — simulates
    // the bench's single-shot measurement across different users.
    {
        let mut db = setup();
        let _ = db.query(sql, [Value::Integer(1)]).unwrap();
        for k in 100..601i64 {
            let start = Instant::now();
            let _ = db.query(sql, [Value::Integer(k)]).unwrap();
            let e = us(start.elapsed());
            if k <= 102 || k >= 598 {
                println!("  single-shot param={k}: {e:>8.1} us");
            }
        }
    }
}
