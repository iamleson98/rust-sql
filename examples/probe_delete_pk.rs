//! Profile DELETE by PK loop vs SQLite.
use std::time::Instant;

fn main() {
    // rustqlite
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=5_000i64 {
        let sql = format!("INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})", i, i, i as f64);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // Warm
    for i in 1..=50i64 {
        db.execute("DELETE FROM t WHERE id = ?", [rustqlite::Value::Integer(i)]).unwrap();
    }
    let start = Instant::now();
    for i in 51..=1050i64 {
        db.execute("DELETE FROM t WHERE id = ?", [rustqlite::Value::Integer(i)]).unwrap();
    }
    let d_r = start.elapsed();
    // Fixed-overhead: DELETE matching nothing.
    let start = Instant::now();
    for i in 0..1000 {
        db.execute("DELETE FROM t WHERE id = ?", [rustqlite::Value::Integer(-1)]).unwrap();
    }
    println!("rustqlite DELETE no-match 1k: {:?} ({} ns/op)", start.elapsed(), start.elapsed().as_nanos() as f64 / 1000.0);
    let cnt: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer();
    println!("rustqlite DELETE by PK 1k: {:?} ({} ns/op) rows left={}", d_r, d_r.as_nanos() as f64 / 1000.0, cnt);

    // SQLite
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL); CREATE INDEX idx_val ON t(val);").unwrap();
    for i in 1..=5_000i64 {
        let sql = format!("INSERT INTO t (name, val, score) VALUES ('user{}', {}, {})", i, i, i as f64);
        conn.execute(&sql, []).unwrap();
    }
    for i in 1..=50i64 {
        conn.execute("DELETE FROM t WHERE id = ?", [i]).unwrap();
    }
    let start = Instant::now();
    for i in 51..=1050i64 {
        conn.execute("DELETE FROM t WHERE id = ?", [i]).unwrap();
    }
    let d_s = start.elapsed();
    println!("sqlite     DELETE by PK 1k: {:?} ({} ns/op)", d_s, d_s.as_nanos() as f64 / 1000.0);
    println!("ratio: {:.2}x {}", d_s.as_secs_f64() / d_r.as_secs_f64(), if d_r < d_s { "FASTER" } else { "slower" });
}
