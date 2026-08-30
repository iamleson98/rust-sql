//! join3 first-call decomposition: is the 175us penalty (a) storm-related
//! cache eviction, (b) first-execution-of-statement cost, or (c) something
//! in the plan/execute setup? Sequence: warm stmt -> storm -> measure.
use std::time::Instant;
use rustqlite::types::Value;
use rustqlite::Database;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 * 1e-3
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)", []).unwrap();
    db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)", []).unwrap();
    db.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)", []).unwrap();
    db.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
    db.execute("CREATE INDEX idx_items_order ON items(order_id)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute("INSERT INTO users (name, dept) VALUES (?, 'eng')",
            [Value::Text(format!("user{}", i).into())]).unwrap();
    }
    for i in 1..=10000i64 {
        db.execute("INSERT INTO orders (user_id, total) VALUES (?, ?)",
            [Value::Integer((i % 1000) + 1), Value::Integer(i * 10)]).unwrap();
    }
    for i in 1..=50000i64 {
        db.execute("INSERT INTO items (order_id, name, price) VALUES (?, ?, ?)",
            [Value::Integer((i % 10000) + 1), Value::Text(format!("item{}", i).into()), Value::Real(i as f64 * 0.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = ?";

    // (1) FIRST call ever: parse + plan + first execute + cold cache
    let start = Instant::now();
    let rows = db.query(sql, [Value::Integer(1)]).unwrap();
    println!("1st call ever:           {:>7.1} us  ({} rows)", us(start.elapsed()), rows.len());

    // (2) immediate second call
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1)]).unwrap();
    println!("2nd call:                {:>7.1} us", us(start.elapsed()));

    // (3) 500 more to reach steady state
    for _ in 0..500 { let _ = db.query(sql, [Value::Integer(1)]).unwrap(); }
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1)]).unwrap();
    println!("steady (id=1):           {:>7.1} us", us(start.elapsed()));

    // (4) NOW a small write storm, then the same CACHED statement:
    db.execute("BEGIN", []).unwrap();
    for i in 1..=200i64 {
        db.execute("INSERT INTO items (order_id, name, price) VALUES (?, ?, ?)",
            [Value::Integer(1), Value::Text(format!("x{}", i).into()), Value::Real(1.0)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1)]).unwrap();
    println!("after 200-row storm:     {:>7.1} us", us(start.elapsed()));

    // (5) same again — bigger storm via a different table shape
    db.execute("BEGIN", []).unwrap();
    for i in 1..=5000i64 {
        db.execute("INSERT INTO items (order_id, name, price) VALUES (?, ?, ?)",
            [Value::Integer((i % 10000) + 1), Value::Text(format!("y{}", i).into()), Value::Real(1.0)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1)]).unwrap();
    println!("after 5k-row storm:      {:>7.1} us", us(start.elapsed()));
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(1)]).unwrap();
    println!("  next call:             {:>7.1} us", us(start.elapsed()));

    // (6) Parse+plan cost isolated: same statement text + suffix comment.
    let v: String = format!("{} /* {} */", sql, 1);
    let start = Instant::now();
    let _ = db.query(&v, [Value::Integer(1)]).unwrap();
    println!("fresh parse+plan+exec:   {:>7.1} us", us(start.elapsed()));
    let v2: String = format!("{} /* {} */", sql, 2);
    let start = Instant::now();
    let _ = db.query(&v2, [Value::Integer(1)]).unwrap();
    println!("fresh parse+plan+exec 2: {:>7.1} us", us(start.elapsed()));

    // (7) user 500 vs user 1 — different data spread
    let start = Instant::now();
    let rows = db.query(sql, [Value::Integer(500)]).unwrap();
    println!("steady (id=500):         {:>7.1} us ({} rows)", us(start.elapsed()), rows.len());
}
