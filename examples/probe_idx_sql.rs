fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10_000i64 {
        db.execute(
            ins,
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

    // Test every val from 2..2000 even.
    let mut missing: Vec<i64> = Vec::new();
    for v in (2..=2000i64).step_by(2) {
        let rows = db
            .query(
                "SELECT id FROM t WHERE val = ?",
                [rustqlite::Value::Integer(v)],
            )
            .unwrap();
        if rows.len() != 1 {
            missing.push(v);
        }
    }
    println!("missing lookups: {} of 1000", missing.len());
    println!("first missing: {:?}", &missing[..missing.len().min(10)]);

    // For the first missing value, try alternate query shapes.
    if let Some(v) = missing.first() {
        let a = db
            .query(
                "SELECT COUNT(*) FROM t WHERE val = ?",
                [rustqlite::Value::Integer(*v)],
            )
            .unwrap();
        println!("val={} count via index: {:?}", v, a[0][0]);
        let b = db
            .query(
                "SELECT COUNT(*) FROM t WHERE val + 0 = ?",
                [rustqlite::Value::Integer(*v)],
            )
            .unwrap();
        println!("val={} count via full scan: {:?}", v, b[0][0]);
        let c = db
            .query(
                "SELECT id FROM t WHERE val BETWEEN ? AND ?",
                [rustqlite::Value::Integer(*v), rustqlite::Value::Integer(*v)],
            )
            .unwrap();
        println!("val={} BETWEEN count: {}", v, c.len());
        let d = db
            .query(
                "SELECT id FROM t WHERE val > ? AND val < ?",
                [
                    rustqlite::Value::Integer(*v - 1),
                    rustqlite::Value::Integer(*v + 1),
                ],
            )
            .unwrap();
        println!("val={} range count: {}", v, d.len());
    }
}
