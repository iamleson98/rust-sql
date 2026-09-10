fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    let _ = db.execute("DROP TABLE IF EXISTS t", []);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
        [],
    )
    .unwrap();
    for (id, v) in [(1, "apple"), (2, "APPLE"), (3, "banana")] {
        db.execute(
            "INSERT INTO t VALUES (?, ?)",
            vec![
                rustqlite::Value::Integer(id),
                rustqlite::Value::Text(v.into()),
            ],
        )
        .unwrap();
    }
    db.execute("CREATE INDEX ix ON t(v)", []).unwrap();
    for q in [
        "EXPLAIN SELECT sum(id) FROM t WHERE v = 'APPLE'",
        "EXPLAIN SELECT count(*) FROM t WHERE v = 'APPLE'",
        "SELECT sum(id) FROM t WHERE v = 'APPLE'",
        "SELECT group_concat(v) FROM t WHERE v = 'APPLE'",
        "SELECT max(id) FROM t WHERE v = 'APPLE'",
    ] {
        println!("--- {q}");
        for r in &db.query(q, []).unwrap() {
            println!("  {:?}", r);
        }
    }
}
