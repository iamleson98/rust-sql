//! Probe: bind-parameter scan/filter queries — rustqlite vs SQLite (rusqlite)
//! on identical 5000-row data, identical query shapes, plan cache warm.

use std::time::Instant;
use rustqlite::{Database, Value};

fn main() {
    // ---- rustqlite ----
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..5000i64 {
        db.execute(
            "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Real(i as f64 * 0.5), Value::Text(format!("name-{i}").into())],
        ).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    // ---- SQLite ----
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    conn.execute("BEGIN", []).unwrap();
    {
        let mut stmt = conn.prepare("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)").unwrap();
        for i in 0..5000i64 {
            stmt.execute(rusqlite::params![i, i as f64 * 0.5, format!("name-{i}")]).unwrap();
        }
    }
    conn.execute("COMMIT", []).unwrap();

    let n = 2000i64;
    let queries: Vec<(&str, Box<dyn Fn(i64) -> Vec<Value>>, Box<dyn Fn(i64) -> Vec<i64>>)> = vec![
        (
            "bind BETWEEN + COUNT/AVG",
            Box::new(|i: i64| vec![Value::Integer((i*2) % 4000), Value::Integer((i*2) % 4000 + 50)]),
            Box::new(|i: i64| vec![(i*2) % 4000, (i*2) % 4000 + 50]),
        ),
        (
            "bind BETWEEN (no agg)",
            Box::new(|i: i64| vec![Value::Integer((i*2) % 4000), Value::Integer((i*2) % 4000 + 50)]),
            Box::new(|i: i64| vec![(i*2) % 4000, (i*2) % 4000 + 50]),
        ),
        (
            "bind equality (no idx)",
            Box::new(|i: i64| vec![Value::Integer((i*7) % 5000)]),
            Box::new(|i: i64| vec![(i*7) % 5000]),
        ),
        (
            "bind a > ? + COUNT",
            Box::new(|_i: i64| vec![Value::Integer(4500)]),
            Box::new(|_i: i64| vec![4500i64]),
        ),
    ];

    for (name, rq_params, sq_params) in queries {
        let sql_rq = match name {
            s if s.contains("COUNT/AVG") => "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?",
            s if s.contains("BETWEEN") => "SELECT a FROM bench WHERE a BETWEEN ? AND ?",
            s if s.contains("equality") => "SELECT a FROM bench WHERE a = ?",
            _ => "SELECT COUNT(*) FROM bench WHERE a > ?",
        };
        let sql_sq = sql_rq;

        // warm both plan caches
        let _ = db.query(sql_rq, rq_params(0).clone());
        {
            let mut st = conn.prepare(sql_sq).unwrap();
            let _ = st.query(rusqlite::params_from_iter(sq_params(0))).unwrap();
        }

        let t = Instant::now();
        for i in 0..n {
            let _ = db.query(sql_rq, rq_params(i));
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3 / n as f64;

        let t = Instant::now();
        for i in 0..n {
            let mut st = conn.prepare(sql_sq).unwrap();
            let mut rows = st.query(rusqlite::params_from_iter(sq_params(i))).unwrap();
            while rows.next().unwrap().is_some() {}
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3 / n as f64;

        println!(
            "{:<28} rustqlite {:>8.1} µs   SQLite {:>8.1} µs   {}",
            name,
            rq_ms * 1e3,
            sq_ms * 1e3,
            if rq_ms <= sq_ms { "WIN" } else { "LOSE" }
        );
    }
}
