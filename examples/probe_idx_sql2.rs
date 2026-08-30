fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute("INSERT INTO t (name, val, score) VALUES (?, ?, ?)", [
            rustqlite::Value::Text(format!("name{}", i).into()),
            rustqlite::Value::Integer(i * 2),
            rustqlite::Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // Is the data in the index at all? Range scans over different spans.
    for (lo, hi) in [(0i64, 100000i64), (1234, 100000), (1236, 1236), (2000, 100000), (5000, 100000)] {
        let r = db.query("SELECT COUNT(*) FROM t WHERE val > ? AND val <= ?", [rustqlite::Value::Integer(lo), rustqlite::Value::Integer(hi)]).unwrap();
        println!("val in ({}, {}]: {:?}", lo, hi, r[0][0]);
    }
    // Ordered index scan.
    let r = db.query("SELECT val FROM t WHERE val > 0 ORDER BY val LIMIT 3", []).unwrap();
    println!("first 3 by val: {:?}", r);
    let r = db.query("SELECT val FROM t WHERE val > 1200 ORDER BY val LIMIT 3", []).unwrap();
    println!("first 3 after 1200: {:?}", r);
}
