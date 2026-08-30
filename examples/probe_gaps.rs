//! Micro-profiler for the remaining perf gaps: decomposes each workload into
//! pipeline layers (btree-only / fast-path / full query) to find fixed costs.
use std::time::Instant;
use rustqlite::types::Value;
use rustqlite::Database;

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e3 + d.as_nanos() as f64 / 1e6
}
fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 / 1e3
}

fn main() {
    // ---------- 1. Range scan 10 rows ----------
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [Value::Text(format!("user{}", i)), Value::Integer(i), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    // warm
    for _ in 0..50 {
        let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    }
    let n = 2000;
    let start = Instant::now();
    for _ in 0..n {
        let _ = db.query(sql, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    }
    let full = us(start.elapsed()) / n as f64;
    println!("range10 full query():      {:>8.1} ns/op", full * 1000.0);

    // empty range (touches tree but emits 0 rows): measures descent cost
    let start = Instant::now();
    for _ in 0..n {
        let _ = db.query(sql, [Value::Integer(100000), Value::Integer(100009)]).unwrap();
    }
    let empty = us(start.elapsed()) / n as f64;
    println!("range10 EMPTY range:       {:>8.1} ns/op  (descent-only cost)", empty * 1000.0);

    // Same via raw plan inspection: get the plan and time execute() alone
    // (parse+cache amortized away, like a prepared statement would).

    // ---------- 2. Point lookup indexed ----------
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    let sql2 = "SELECT id, name, score FROM t WHERE val = ?";
    for _ in 0..50 {
        let _ = db.query(sql2, [Value::Integer(2000)]).unwrap();
    }
    let n2 = 2000;
    let start = Instant::now();
    for i in 0..n2 {
        let target = ((i % 1000) + 1) * 2;
        let _ = db.query(sql2, [Value::Integer(target)]).unwrap();
    }
    println!("idx-point full query():    {:>8.1} ns/op", us(start.elapsed()) / n2 as f64 * 1000.0);

    // missing key (index descent + no row fetch)
    let start = Instant::now();
    for _ in 0..n2 {
        let _ = db.query(sql2, [Value::Integer(-999999)]).unwrap();
    }
    println!("idx-point MISSING key:     {:>8.1} ns/op  (index descent only)", us(start.elapsed()) / n2 as f64 * 1000.0);

    // ---------- 3. rowid point lookup (control) ----------
    let sql3 = "SELECT name, val, score FROM t WHERE id = ?";
    for _ in 0..50 {
        let _ = db.query(sql3, [Value::Integer(500)]).unwrap();
    }
    let start = Instant::now();
    for i in 0..n2 {
        let target = (i % 1000) + 1;
        let _ = db.query(sql3, [Value::Integer(target)]).unwrap();
    }
    println!("rowid-point full query():  {:>8.1} ns/op", us(start.elapsed()) / n2 as f64 * 1000.0);

    // ---------- 4. 3-table join ----------
    let mut dbj = Database::open_in_memory().unwrap();
    dbj.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)", []).unwrap();
    dbj.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL)", []).unwrap();
    dbj.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)", []).unwrap();
    dbj.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        dbj.execute("INSERT INTO users VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Text(format!("u{}", i)), Value::Text(format!("d{}", i % 10))]).unwrap();
    }
    for i in 1..=10000i64 {
        dbj.execute("INSERT INTO orders VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Integer((i % 1000) + 1), Value::Real(i as f64)]).unwrap();
    }
    for i in 1..=50000i64 {
        dbj.execute("INSERT INTO items VALUES (?, ?, ?, ?)",
            [Value::Integer(i), Value::Integer((i % 10000) + 1), Value::Text(format!("item{}", i)), Value::Real(i as f64 * 0.5)]).unwrap();
    }
    dbj.execute("COMMIT", []).unwrap();
    dbj.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
    dbj.execute("CREATE INDEX idx_items_order ON items(order_id)", []).unwrap();

    let sql4 = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = ?";
    for _ in 0..20 {
        let _ = dbj.query(sql4, [Value::Integer(1)]).unwrap();
    }
    let n4 = 500;
    let start = Instant::now();
    for _ in 0..n4 {
        let _ = dbj.query(sql4, [Value::Integer(500)]).unwrap();
    }
    println!("3-table join full:         {:>8.1} us/op", us(start.elapsed()) / n4 as f64);

    let _ = ms;
}
