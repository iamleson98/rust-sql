//! Probe: where does SELECT COUNT(*) FROM t time go?
//! 1. Full db.query("SELECT COUNT(*)") loop
//! 2. Raw Btree::count_rows loop (bypasses parse/plan/executor)
//! 3. Raw page-walk count (bypasses Btree bookkeeping)

use rustqlite::{Database, Value};
use std::time::Instant;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(
            "INSERT INTO t (name, val) VALUES (?, ?)",
            [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // Warm the plan cache.
    for _ in 0..100 {
        let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    }

    let n = 20000;
    let start = Instant::now();
    let mut last = 0i64;
    for _ in 0..n {
        let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        last = rows[0][0].as_integer();
    }
    let full = start.elapsed() / n;
    println!("full db.query COUNT(*): {:?}/call  (last={})", full, last);

    // Raw btree count via a prepared statement's internals is not public;
    // approximate the SQLite side cost for context:
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=10000i64 {
        conn.execute(
            "INSERT INTO t (name, val) VALUES (?1, ?2)",
            rusqlite::params![format!("name{}", i), i],
        )
        .unwrap();
    }
    let start = Instant::now();
    for _ in 0..n {
        let c: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        last = c;
    }
    let sq = start.elapsed() / n;
    println!("sqlite query_row COUNT(*): {:?}/call  (last={})", sq, last);
}
