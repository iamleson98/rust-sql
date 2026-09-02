fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100i64 {
        db.execute(
            "INSERT INTO t (val, score) VALUES (?, ?)",
            [rustqlite::Value::Integer(i), rustqlite::Value::Real(1.0)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // UPDATE range — how many rows does it actually touch?
    db.execute("UPDATE t SET score = score + 1.0 WHERE val > 50", [])
        .unwrap();
    let r = db
        .query("SELECT COUNT(*) FROM t WHERE score > 1.0", [])
        .unwrap();
    println!("rows actually updated (expect 50): {:?}", r[0][0]);

    // Insert AFTER index creation — do lookups work for new rows?
    db.execute(
        "INSERT INTO t (val, score) VALUES (?, ?)",
        [rustqlite::Value::Integer(1000), rustqlite::Value::Real(1.0)],
    )
    .unwrap();
    let r2 = db.query("SELECT id FROM t WHERE val = 1000", []).unwrap();
    println!("lookup of post-index-insert row (expect 1): {}", r2.len());
    let r3 = db.query("SELECT id FROM t WHERE val = 50", []).unwrap();
    println!("lookup of pre-index row (expect 1): {}", r3.len());
}
