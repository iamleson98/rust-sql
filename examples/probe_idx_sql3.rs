fn main() {
    let path = "/tmp/probe_idx_sql3.db";
    let _ = std::fs::remove_file(path);
    let mut db = rustqlite::Database::open(path).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                rustqlite::Value::Text(format!("name{}", i).into()),
                rustqlite::Value::Integer(i * 2),
                rustqlite::Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // In the SAME session:
    let r = db
        .query("SELECT COUNT(*) FROM t WHERE val = 1236", [])
        .unwrap();
    println!("same-session val=1236: {:?} (expect 1)", r[0][0]);

    // Reopen (catalog reloads from the schema table):
    drop(db);
    let db2 = rustqlite::Database::open(path).unwrap();
    let r2 = db2
        .query("SELECT COUNT(*) FROM t WHERE val = 1236", [])
        .unwrap();
    println!("reopened   val=1236: {:?} (expect 1)", r2[0][0]);
    let r3 = db2
        .query("SELECT COUNT(*) FROM t WHERE val > 0", [])
        .unwrap();
    println!("reopened   val>0:    {:?} (expect 10000)", r3[0][0]);
}
