fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    let _ = db.execute("DROP TABLE IF EXISTS t", []);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES (1,'apple'),(2,'APPLE'),(3,'Banana'),(4,'banana'),(5,'cherry'),(6,'Cherry'),(7,'date'),(8,'éclair'),(9,'apple pie'),(10,'BANANA split')", []).unwrap();
    for q in [
        "SELECT v, count(*) FROM t WHERE id > 2 GROUP BY v ORDER BY v",
        "SELECT v, count(*) FROM t WHERE id BETWEEN 3 AND 10 GROUP BY v ORDER BY v",
        "SELECT v, count(*) FROM t WHERE v != 'apple' GROUP BY v ORDER BY v",
        "SELECT v, count(*) FROM t WHERE id > 2 AND v != 'apple' GROUP BY v ORDER BY v",
    ] {
        println!("--- {q}");
        for r in &db.query(q, []).unwrap() {
            println!("  {:?}", r);
        }
    }
}
