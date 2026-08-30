//! Probe: where does the FIRST execution of a statement spend time?
//! Measures cold parse+plan (unique SQL text) vs steady-state for the
//! indexed point lookup, and the raw cost of parse-only.
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
            [Value::Text(format!("user{}", i).into()), Value::Integer(i), Value::Real(i as f64 * 1.5)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // 1. Cold first-call cost of the bench statement (parse + plan +
    //    fast-path build + cache insert + execute).
    let sql = "SELECT id, name, score FROM t WHERE val = ?";
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(2000)]).unwrap();
    println!("idx-point FIRST call:  {:>8.1} us", us(start.elapsed()));

    // 2. Steady state.
    for _ in 0..100 { let _ = db.query(sql, [Value::Integer(2000)]).unwrap(); }
    let n = 2000;
    let start = Instant::now();
    for i in 0..n {
        let t = ((i % 1000) + 1) * 2;
        let _ = db.query(sql, [Value::Integer(t)]).unwrap();
    }
    println!("idx-point steady:      {:>8.1} ns/op", us(start.elapsed()) / n as f64 * 1000.0);

    // 3. Cold cost of a semantically identical statement with DIFFERENT
    //    text (forces fresh parse+plan; cache holds an entry already).
    let sql_variant: String = format!("SELECT id, name, score FROM t WHERE val = ? /* {} */", 7);
    let start = Instant::now();
    let _ = db.query(&sql_variant, [Value::Integer(2000)]).unwrap();
    println!("idx-point variant 1st: {:>8.1} us  (parse+plan only, cache warm path)", us(start.elapsed()));

    // Repeat a few variants to average.
    let mut total = 0.0;
    for k in 0..20 {
        let v: String = format!("SELECT id, name, score FROM t WHERE val = ? /* v{} */", k);
        let start = Instant::now();
        let _ = db.query(&v, [Value::Integer(2000)]).unwrap();
        total += us(start.elapsed());
    }
    println!("idx-point variant avg: {:>8.1} us  (20 fresh parses+plans)", total / 20.0);

    // 4. Rowid point lookup first call for comparison.
    let sql3 = "SELECT name, val, score FROM t WHERE id = ?";
    let start = Instant::now();
    let _ = db.query(sql3, [Value::Integer(2000)]).unwrap();
    println!("rowid-point FIRST call:{:>8.1} us", us(start.elapsed()));

    // 5. Parse-only cost (execute on a throwaway memdb? use same db, an
    //    identical-semantics but never-executed shape).
    for k in 0..20 {
        let v: String = format!("SELECT id, name FROM t WHERE val = ? /* p{} */", k);
        let start = Instant::now();
        let _ = db.query(&v, [Value::Integer(2000)]).unwrap();
        if k == 0 {
            println!("(2-col variant first: {:.1} us)", us(start.elapsed()));
        }
    }

    // 6. Cold cost AFTER clearing the plan cache? Simulate with the
    //    longest-ago statement: range scan.
    let sql4 = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    let start = Instant::now();
    let _ = db.query(sql4, [Value::Integer(1000), Value::Integer(1009)]).unwrap();
    println!("range10 FIRST call:    {:>8.1} us", us(start.elapsed()));

    // 7. 3-table join first call (the other gap).
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

    let sql5 = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = ?";
    let start = Instant::now();
    let _ = db.query(sql5, [Value::Integer(1)]).unwrap();
    println!("join3 FIRST call:      {:>8.1} us", us(start.elapsed()));

    // steady state
    for _ in 0..50 { let _ = db.query(sql5, [Value::Integer(1)]).unwrap(); }
    let n = 500;
    let start = Instant::now();
    for _ in 0..n { let _ = db.query(sql5, [Value::Integer(500)]).unwrap(); }
    println!("join3 steady (id=500): {:>8.1} us/op", us(start.elapsed()) / n as f64);

    // Param 1 vs 500 — different row counts (1 has 10 orders? user_id 1 -> orders 1,1001,...)
    let start = Instant::now();
    for _ in 0..n { let _ = db.query(sql5, [Value::Integer(1)]).unwrap(); }
    println!("join3 steady (id=1):   {:>8.1} us/op", us(start.elapsed()) / n as f64);
}
