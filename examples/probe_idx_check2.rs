fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100i64 {
        db.execute("INSERT INTO t (val) VALUES (?)", [rustqlite::Value::Integer(i)]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    let tests = [
        ("val = 50", "SELECT COUNT(*) FROM t WHERE val = 50"),
        ("val > 90", "SELECT COUNT(*) FROM t WHERE val > 90"),
        ("val >= 90", "SELECT COUNT(*) FROM t WHERE val >= 90"),
        ("val BETWEEN 10 AND 20", "SELECT COUNT(*) FROM t WHERE val BETWEEN 10 AND 20"),
        ("val < 10 (no index?)", "SELECT COUNT(*) FROM t WHERE val < 10"),
        ("non-sargable val+0 > 90", "SELECT COUNT(*) FROM t WHERE val + 0 > 90"),
        ("ORDER BY val LIMIT 3", "SELECT val FROM t ORDER BY val LIMIT 3"),
    ];
    for (name, sql) in tests {
        let r = db.query(sql, []).unwrap();
        println!("{:<28} -> {:?}", name, r.iter().map(|row| row[0].clone()).collect::<Vec<_>>());
    }
    // Full scan check.
    let r = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("count(*) = {:?}", r[0][0]);
}
