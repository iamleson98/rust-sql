//! Benchmark: JOIN performance.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rusqlite::params;

fn setup_rusqlite(n: i64) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
    conn.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)", []).unwrap();
    conn.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
    for i in 1..=n {
        conn.execute("INSERT INTO users (name) VALUES (?1)", params![format!("user{}", i)]).unwrap();
        for j in 0..5 {
            conn.execute(
                "INSERT INTO orders (user_id, total) VALUES (?1, ?2)",
                params![i, j * 100 + i],
            ).unwrap();
        }
    }
    conn
}

fn setup_rustqlite(n: i64) -> rustqlite::Database {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
    db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)", []).unwrap();
    for i in 1..=n {
        let sql = format!("INSERT INTO users (name) VALUES ('user{}')", i);
        db.execute(&sql, []).unwrap();
        for j in 0..5 {
            let sql = format!("INSERT INTO orders (user_id, total) VALUES ({}, {})", i, j * 100 + i);
            db.execute(&sql, []).unwrap();
        }
    }
    db
}

fn bench_rusqlite_join(c: &mut Criterion) {
    let conn = setup_rusqlite(100);
    c.bench_function("rusqlite_join", |b| {
        b.iter(|| {
            let mut stmt = conn.prepare(
                "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = ?1",
            ).unwrap();
            let mut rows = stmt.query(params![black_box(50)]).unwrap();
            while let Some(_) = rows.next().unwrap() {}
        })
    });
}

fn bench_rustqlite_join(c: &mut Criterion) {
    let db = setup_rustqlite(100);
    c.bench_function("rustqlite_join", |b| {
        b.iter(|| {
            let _rows = db.query(
                "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 50",
                [],
            ).unwrap();
            let _ = black_box(50);
        })
    });
}

criterion_group!(benches, bench_rusqlite_join, bench_rustqlite_join);
criterion_main!(benches);
