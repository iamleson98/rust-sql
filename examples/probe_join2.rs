//! A/B the 2-table join point query (the CI-marginal row): users PK
//! point + INLJ into orders via idx_orders_user (~10 rows).
use rustqlite::Value;
use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    for ddl in [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        "CREATE INDEX idx_orders_user ON orders(user_id)",
    ] {
        db.execute(ddl, []).unwrap();
    }
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute(
            "INSERT INTO users (name, dept) VALUES (?, ?)",
            [Value::Text(format!("user{i}").into()), Value::Text("eng".into())],
        )
        .unwrap();
    }
    for i in 1..=10_000i64 {
        db.execute(
            "INSERT INTO orders (user_id, total) VALUES (?, ?)",
            [Value::Integer((i % 1000) + 1), Value::Integer(i * 10)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = ?";
    let _ = db.query(sql, [Value::Integer(1)]).unwrap();
    let n = 5000;
    let t = Instant::now();
    for i in 0..n {
        let _ = db.query(sql, [Value::Integer((i % 1000) + 1)]).unwrap();
    }
    let rq_us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!("rustqlite: {rq_us:6.2} us/query");

    // SQLite side.
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF;")
        .unwrap();
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)", []).unwrap();
    conn.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)", []).unwrap();
    conn.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
    {
        let txn = conn.transaction().unwrap();
        for i in 1..=1000i64 {
            txn.execute("INSERT INTO users (name, dept) VALUES (?1, 'eng')", rusqlite::params![format!("user{i}")]).unwrap();
        }
        for i in 1..=10_000i64 {
            txn.execute("INSERT INTO orders (user_id, total) VALUES (?1, ?2)", rusqlite::params![(i % 1000) + 1, i * 10]).unwrap();
        }
        txn.commit().unwrap();
    }
    let mut stmt = conn
        .prepare("SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = ?1")
        .unwrap();
    let _ = stmt.query(rusqlite::params![1]).unwrap();
    let t = Instant::now();
    for i in 0..n {
        let mut rows = stmt.query(rusqlite::params![(i % 1000) + 1]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let _ = r.get::<_, String>(0).unwrap();
            let _ = r.get::<_, i64>(1).unwrap();
        }
    }
    let sq_us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!("sqlite  : {sq_us:6.2} us/query  (prepared stmt cached, like the engine path)");
    println!("ratio: {:.2}x", rq_us / sq_us);
}
