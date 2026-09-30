fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE f1 (id INTEGER PRIMARY KEY, x INT, t TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE f2 (id INTEGER PRIMARY KEY, x INT, u TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=50_000i64 {
        db.execute(
            "INSERT INTO f1 (x, t) VALUES (?, 't')",
            [rustqlite::Value::Integer(i % 300)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO f2 (x, u) VALUES (?, 'u')",
            [rustqlite::Value::Integer(i % 300)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let (cols, rows) = db
        .query_with_columns(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM f1, f2 WHERE f1.x = f2.x",
            [],
        )
        .unwrap();
    for r in rows {
        println!("PLAN: {:?}", r);
    }
    let _ = cols;
    use std::time::Instant;
    let t = Instant::now();
    let n = db
        .query("SELECT COUNT(*) FROM f1, f2 WHERE f1.x = f2.x", [])
        .unwrap();
    println!(
        "count-join: {:?} in {:.1} ms",
        n.first().and_then(|r| r.first().cloned()),
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    let n2 = db
        .query("SELECT COUNT(*) FROM f1 JOIN f2 ON f1.x = f2.x", [])
        .unwrap();
    println!(
        "count-join-on: {:?} in {:.1} ms",
        n2.first().and_then(|r| r.first().cloned()),
        t.elapsed().as_secs_f64() * 1e3
    );
}
