//! Focused probe: UPDATE by PK and 2-table join — compare against pre-sprint.

use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10_000i64 {
        db.execute(ins, [
            rustqlite::Value::Text(format!("name{}", i).into()),
            rustqlite::Value::Integer(i * 2),
            rustqlite::Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // UPDATE by PK (1000 ops, same shape as bench_compare).
    {
        let sql = "UPDATE t SET score = ? WHERE id = ?";
        let start = Instant::now();
        for i in 1..=1000i64 {
            let score = i as f64 * 2.5;
            let id = (i % 1000) + 1;
            db.execute(sql, [rustqlite::Value::Real(score), rustqlite::Value::Integer(id)]).unwrap();
        }
        let d = start.elapsed();
        println!("UPDATE by PK (1k ops): {:?} ({:.2?}/op)", d, d / 1000);
    }

    // DELETE by PK.
    {
        let sql = "DELETE FROM t WHERE id = ?";
        let start = Instant::now();
        for i in 1..=1000i64 {
            db.execute(sql, [rustqlite::Value::Integer(i)]).unwrap();
        }
        let d = start.elapsed();
        println!("DELETE by PK (1k ops): {:?} ({:.2?}/op)", d, d / 1000);
    }

    // Point lookup by rowid.
    {
        let sql = "SELECT name, val, score FROM t WHERE id = ?";
        let start = Instant::now();
        for i in 2000..3000i64 {
            let _ = db.query(sql, [rustqlite::Value::Integer(i)]).unwrap();
        }
        let d = start.elapsed();
        println!("Point lookup (1k ops): {:?} ({:.2?}/op)", d, d / 1000);
    }

    // 2-table join filtered by PK.
    {
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
        db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total REAL)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=1000i64 {
            db.execute("INSERT INTO users (name) VALUES (?)", [rustqlite::Value::Text(format!("user{}", i).into())]).unwrap();
        }
        for i in 1..=10_000i64 {
            db.execute("INSERT INTO orders (user_id, total) VALUES (?, ?)", [rustqlite::Value::Integer((i % 1000) + 1), rustqlite::Value::Real(i as f64)]).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("CREATE INDEX idx_orders_user ON orders(user_id)", []).unwrap();
        let sql = "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = ?";
        let start = Instant::now();
        for i in 1..=100i64 {
            let _ = db.query(sql, [rustqlite::Value::Integer(i)]).unwrap();
        }
        let d = start.elapsed();
        println!("2-table join by PK (100 ops): {:?} ({:.2?}/op)", d, d / 100);
    }
}
