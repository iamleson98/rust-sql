fn main() {
    // Index created FIRST, then 10k inserts across MANY autocommit
    // statements — index splits happen mid-sequence; roots must carry over.
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute("INSERT INTO t (val) VALUES (?)", [rustqlite::Value::Integer(i * 3)]).unwrap();
    }
    let c1 = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("rows: {:?} (expect 10000)", c1[0][0]);
    // Every row findable via the index?
    let mut missing = 0;
    for i in 1..=10_000i64 {
        let r = db.query("SELECT id FROM t WHERE val = ?", [rustqlite::Value::Integer(i * 3)]).unwrap();
        if r.len() != 1 { missing += 1; }
    }
    println!("index lookups missing: {} (expect 0)", missing);
    // Range query via index.
    let r = db.query("SELECT COUNT(*) FROM t WHERE val > 15000", []).unwrap();
    println!("val > 15000: {:?} (expect 5000)", r[0][0]);
    // UPDATE via index.
    db.execute("UPDATE t SET val = val + 1 WHERE val > 29990", []).unwrap();
    let r = db.query("SELECT COUNT(*) FROM t WHERE val > 29990", []).unwrap();
    println!("after UPDATE val>29990: {:?} (expect 11)", r[0][0]);
    // DELETE via index + persistence across reopen.
    db.execute("DELETE FROM t WHERE val <= 3", []).unwrap();
    let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!("after DELETE val<=3: {:?} (expect 9999)", c[0][0]);
}
