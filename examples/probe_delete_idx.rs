//! DELETE by PK WITH an index (the bench scenario) — verify fast path fires.

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
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // Warm up.
    for i in 9000..9100i64 {
        db.execute("DELETE FROM t WHERE id = ?", [rustqlite::Value::Integer(i)]).unwrap();
    }
    let sql = "DELETE FROM t WHERE id = ?";
    let start = Instant::now();
    for i in 1..=1000i64 {
        db.execute(sql, [rustqlite::Value::Integer(i)]).unwrap();
    }
    println!("DELETE by PK w/ index (1k ops): {:?} ({:.2?}/op)", start.elapsed(), start.elapsed() / 1000);

    // Verify correctness: rows gone, index consistent.
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("count after: {:?}", n[0][0]);
    let v = db.query("SELECT COUNT(*) FROM t WHERE val > 10000", []).unwrap();
    println!("indexed count (val > 10000, expect 10000-9900... ): {:?}", v[0][0]);
    let row = db.query("SELECT id, name, val FROM t WHERE id = 5000", []).unwrap();
    println!("row 5000 (should be present): {:?}", row[0]);
    let row2 = db.query("SELECT id FROM t WHERE id = 500", []).unwrap();
    println!("row 500 (should be gone): {} rows", row2.len());
}
