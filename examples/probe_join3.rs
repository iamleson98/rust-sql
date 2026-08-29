//! Probe the 3-table join regression: isolate which leg is slow.

use rustqlite::{Database, Value};

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)", []).unwrap();
    db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)", []).unwrap();
    db.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)", []).unwrap();
    db.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
    db.execute("CREATE INDEX idx_items_order ON items(order_id)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000 {
        db.execute(&format!("INSERT INTO users (name, dept) VALUES ('user{}', 'eng')", i), []).unwrap();
    }
    for i in 1..=10000 {
        db.execute(&format!("INSERT INTO orders (user_id, total) VALUES ({}, {})", (i % 1000) + 1, i * 10), []).unwrap();
    }
    for i in 1..=50000 {
        db.execute(&format!("INSERT INTO items (order_id, name, price) VALUES ({}, 'item{}', {})", (i % 10000) + 1, i, i as f64 * 0.5), []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let n = 200;

    // 1. users point lookup
    let t = std::time::Instant::now();
    for _ in 0..n {
        let r = db.query("SELECT name FROM users WHERE id = ?", [Value::Integer(500)]).unwrap();
        assert_eq!(r.len(), 1);
    }
    println!("users by PK        : {:>9.2?}", t.elapsed() / n);

    // 2. orders by indexed user_id
    let t = std::time::Instant::now();
    for _ in 0..n {
        let r = db.query("SELECT total FROM orders WHERE user_id = ?", [Value::Integer(500)]).unwrap();
        assert_eq!(r.len(), 10);
    }
    println!("orders by idx      : {:>9.2?}", t.elapsed() / n);

    // 3. items by indexed order_id
    let t = std::time::Instant::now();
    for _ in 0..n {
        let r = db.query("SELECT name, price FROM items WHERE order_id = ?", [Value::Integer(500)]).unwrap();
        assert_eq!(r.len(), 5);
    }
    println!("items by idx       : {:>9.2?}", t.elapsed() / n);

    // 4. 2-table join
    let sql2 = "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = ?";
    let _ = db.query(sql2, [Value::Integer(1)]).unwrap();
    let t = std::time::Instant::now();
    for _ in 0..n {
        let r = db.query(sql2, [Value::Integer(500)]).unwrap();
        assert_eq!(r.len(), 10);
    }
    println!("2-table join       : {:>9.2?}", t.elapsed() / n);

    // 5. 3-table join
    let sql3 = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = ?";
    let _ = db.query(sql3, [Value::Integer(1)]).unwrap();
    let t = std::time::Instant::now();
    for _ in 0..n {
        let r = db.query(sql3, [Value::Integer(500)]).unwrap();
        assert_eq!(r.len(), 50);
    }
    println!("3-table join       : {:>9.2?}", t.elapsed() / n);
}
