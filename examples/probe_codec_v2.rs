//! Probe: file size after codec v2 + (soon) append-mode splits.

fn main() {
    let path = "/tmp/probe_size_v2.db";
    let _ = std::fs::remove_file(path);
    let mut db = rustqlite::Database::open(path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10_000i64 {
        db.execute(sql, [
            rustqlite::Value::Text(format!("name{}", i)),
            rustqlite::Value::Integer(i * 2),
            rustqlite::Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // Verify row correctness (rowid-alias elision round-trip).
    let rows = db.query("SELECT id, name, val, score FROM t WHERE id = 5000", []).unwrap();
    println!("row 5000: {:?}", rows[0]);
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("count: {:?}", n[0][0]);
    let sum = db.query("SELECT SUM(val) FROM t", []).unwrap();
    println!("sum(val): {:?}", sum[0][0]);
    drop(db);
    let sz = std::fs::metadata(path).unwrap().len();
    println!("file size (10k rows, codec v2): {} bytes ({:.1} KB)", sz, sz as f64 / 1024.0);
    println!("bytes/row: {:.1}", sz as f64 / 10_000.0);
}
