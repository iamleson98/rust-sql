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
    let t0 = Instant::now();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    println!("CREATE INDEX backfill (10k rows): {:?}", t0.elapsed());
    println!("pages after index: {}", db.page_count());

    let sql = "SELECT name, val, score FROM t WHERE val = ?";
    // warm
    let _ = db.query(sql, [rustqlite::Value::Integer(1000)]).unwrap();
    let n = 1000;
    let start = Instant::now();
    let mut found = 0;
    for i in 0..n {
        let v = 2 + (i % 10_000) * 2;
        let rows = db.query(sql, [rustqlite::Value::Integer(v)]).unwrap();
        found += rows.len();
    }
    let d = start.elapsed();
    println!("indexed point lookup ({} ops): {:?} ({:?}/op), rows found: {}", n, d, d.as_nanos() / (n as u128), found);

    // Comparison: rowid point lookup same table.
    let sql2 = "SELECT name, val, score FROM t WHERE id = ?";
    let _ = db.query(sql2, [rustqlite::Value::Integer(1)]).unwrap();
    let start = Instant::now();
    for i in 0..n {
        let _ = db.query(sql2, [rustqlite::Value::Integer(1 + (i % 10_000))]).unwrap();
    }
    let d2 = start.elapsed();
    println!("rowid point lookup  ({} ops): {:?} ({:?}/op)", n, d2, d2.as_nanos() / (n as u128));
}
