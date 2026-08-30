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

    // Before deletes.
    let v0 = db.query("SELECT COUNT(*) FROM t WHERE val > 10000", []).unwrap();
    println!("before deletes: val>10000 = {:?} (expect 5000)", v0[0][0]);

    // Delete via generic path (BETWEEN forces RowidRange, not RowidLookup).
    for i in 1..=100i64 {
        db.execute("DELETE FROM t WHERE id BETWEEN ? AND ?", [rustqlite::Value::Integer(i), rustqlite::Value::Integer(i)]).unwrap();
    }
    let v1 = db.query("SELECT COUNT(*) FROM t WHERE val > 10000", []).unwrap();
    println!("after generic-path deletes (1..100, all val<=200): {:?} (expect 5000)", v1[0][0]);

    // Delete via fast path (id = ?).
    for i in 5000..=5100i64 {
        db.execute("DELETE FROM t WHERE id = ?", [rustqlite::Value::Integer(i)]).unwrap();
    }
    let v2 = db.query("SELECT COUNT(*) FROM t WHERE val > 10000", []).unwrap();
    println!("after fast-path deletes (5000..5100): {:?} (expect 4899)", v2[0][0]);
    let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("count: {:?} (expect 9799)", c[0][0]);
}
