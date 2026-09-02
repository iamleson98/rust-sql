//! Measure SQLite's steady-state 3-table join cost vs ours — decompose
//! the single-shot benchmark gap into steady-work vs cold-data cost.
use rustqlite::types::Value;
use std::time::Instant;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 * 1e-3
}

const SQL: &str = "SELECT u.name, o.total, i.name, i.price FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id WHERE u.id = ?";

fn main() {
    // ---------- rustqlite ----------
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_orders_user ON orders(user_id)", [])
        .unwrap();
    db.execute("CREATE INDEX idx_items_order ON items(order_id)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute(
            "INSERT INTO users (name, dept) VALUES (?, 'eng')",
            [Value::Text(format!("user{}", i).into())],
        )
        .unwrap();
    }
    for i in 1..=10000i64 {
        db.execute(
            "INSERT INTO orders (user_id, total) VALUES (?, ?)",
            [Value::Integer((i % 1000) + 1), Value::Integer(i * 10)],
        )
        .unwrap();
    }
    for i in 1..=50000i64 {
        db.execute(
            "INSERT INTO items (order_id, name, price) VALUES (?, ?, ?)",
            [
                Value::Integer((i % 10000) + 1),
                Value::Text(format!("item{}", i).into()),
                Value::Real(i as f64 * 0.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // warm + steady
    let _ = db.query(SQL, [Value::Integer(1)]).unwrap();
    for _ in 0..100 {
        let _ = db.query(SQL, [Value::Integer(1)]).unwrap();
    }
    let n = 500;
    let start = Instant::now();
    for _ in 0..n {
        let _ = db.query(SQL, [Value::Integer(1)]).unwrap();
    }
    println!(
        "rustqlite steady (id=1):  {:>7.1} us/op",
        us(start.elapsed()) / n as f64
    );

    // single-shot with the SAME param (data warm) — isolates fixed cost
    let start = Instant::now();
    let rows = db.query(SQL, [Value::Integer(1)]).unwrap();
    println!(
        "rustqlite single (warm):   {:>7.1} us ({} rows)",
        us(start.elapsed()),
        rows.len()
    );

    // ---------- SQLite ----------
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF;")
        .ok();
    conn.execute(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, dept TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
        [],
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, order_id INTEGER, name TEXT, price REAL)",
        [],
    )
    .unwrap();
    conn.execute("CREATE INDEX idx_orders_user ON orders(user_id)", [])
        .unwrap();
    conn.execute("CREATE INDEX idx_items_order ON items(order_id)", [])
        .unwrap();
    let mut stmt_i = conn
        .prepare("INSERT INTO users (name, dept) VALUES (?1, 'eng')")
        .unwrap();
    for i in 1..=1000i64 {
        stmt_i
            .execute(rusqlite::params![format!("user{}", i)])
            .unwrap();
    }
    let mut stmt_o = conn
        .prepare("INSERT INTO orders (user_id, total) VALUES (?1, ?2)")
        .unwrap();
    for i in 1..=10000i64 {
        stmt_o
            .execute(rusqlite::params![(i % 1000) + 1, i * 10])
            .unwrap();
    }
    let mut stmt_it = conn
        .prepare("INSERT INTO items (order_id, name, price) VALUES (?1, ?2, ?3)")
        .unwrap();
    for i in 1..=50000i64 {
        stmt_it
            .execute(rusqlite::params![
                (i % 10000) + 1,
                format!("item{}", i),
                i as f64 * 0.5
            ])
            .unwrap();
    }

    let mut stmt = conn.prepare(SQL).unwrap();
    // warm
    for _ in 0..100 {
        let mut rows = stmt.query(rusqlite::params![1]).unwrap();
        while rows.next().unwrap().is_some() {}
    }
    let start = Instant::now();
    for _ in 0..n {
        let mut rows = stmt.query(rusqlite::params![1]).unwrap();
        while rows.next().unwrap().is_some() {}
    }
    println!(
        "sqlite steady (id=1):     {:>7.1} us/op",
        us(start.elapsed()) / n as f64
    );

    // single-shot per-call query (fresh prepare each time, like the bench)
    let start = Instant::now();
    {
        let mut rows = stmt.query(rusqlite::params![1]).unwrap();
        while rows.next().unwrap().is_some() {}
    }
    println!("sqlite single (warm stmt):{:>7.1} us", us(start.elapsed()));

    // cold DATA: user 500 (never touched since setup)
    let start = Instant::now();
    let cnt = {
        let rows = db.query(SQL, [Value::Integer(500)]).unwrap();
        rows.len()
    };
    println!(
        "rustqlite single (id=500 cold): {:>7.1} us ({} rows)",
        us(start.elapsed()),
        cnt
    );
    let start = Instant::now();
    let cnt2 = {
        let mut rows = stmt.query(rusqlite::params![500]).unwrap();
        let mut c = 0;
        while rows.next().unwrap().is_some() {
            c += 1
        }
        c
    };
    println!(
        "sqlite single (id=500 cold):    {:>7.1} us ({} rows)",
        us(start.elapsed()),
        cnt2
    );

    // and again now that 500 is warm
    let start = Instant::now();
    let _ = db.query(SQL, [Value::Integer(500)]).unwrap();
    println!(
        "rustqlite single (id=500 warm): {:>7.1} us",
        us(start.elapsed())
    );
    let start = Instant::now();
    let mut rows = stmt.query(rusqlite::params![500]).unwrap();
    while rows.next().unwrap().is_some() {}
    println!(
        "sqlite single (id=500 warm):    {:>7.1} us",
        us(start.elapsed())
    );
}
