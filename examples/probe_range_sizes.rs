//! Profile range scan fixed cost across sizes.
use std::time::Instant;

fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        let sql = format!("INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})", i, i, i as f64);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    for range in [1usize, 5, 10, 50, 100, 500, 1000] {
        // Warm
        for _ in 0..5 {
            let _ = db.query(sql, [rustqlite::Value::Integer(1000), rustqlite::Value::Integer(1000 + range as i64 - 1)]).unwrap();
        }
        let start = Instant::now();
        let iters = 100;
        for _ in 0..iters {
            let _ = db.query(sql, [rustqlite::Value::Integer(1000), rustqlite::Value::Integer(1000 + range as i64 - 1)]).unwrap();
        }
        let d = start.elapsed() / iters;
        println!("range {:>5}: {:>10.?} ({:>6.0} ns/row)", range, d, d.as_nanos() as f64 / range as f64);
    }

    // SQLite reference
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)").unwrap();
    for i in 1..=10_000i64 {
        let sql = format!("INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})", i, i, i as f64);
        conn.execute(&sql, []).unwrap();
    }
    for range in [1usize, 10, 100, 1000] {
        let mut stmt = conn.prepare("SELECT name, val, score FROM t WHERE id BETWEEN ?1 AND ?2").unwrap();
        for _ in 0..5 {
            let mut rows = stmt.query(rusqlite::params![1000, 1000 + range as i64 - 1]).unwrap();
            while rows.next().unwrap().is_some() {}
        }
        let start = Instant::now();
        let iters = 100;
        for _ in 0..iters {
            let mut rows = stmt.query(rusqlite::params![1000, 1000 + range as i64 - 1]).unwrap();
            while rows.next().unwrap().is_some() {}
        }
        let d = start.elapsed() / iters;
        println!("sqlite range {:>4}: {:>10.?} ({:>6.0} ns/row)", range, d, d.as_nanos() as f64 / range as f64);
    }
}
