fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    for sql in [
        "EXPLAIN SELECT COUNT(*), SUM(a.x) FROM a JOIN b ON b.k = a.k",
        "EXPLAIN SELECT a.x, b.y FROM a JOIN b ON b.k = a.k",
        "EXPLAIN SELECT COUNT(*), SUM(x) FROM a JOIN b ON b.k = a.k",
    ] {
        println!("--- {}", sql);
        let rows = db.query(sql, []).unwrap();
        for r in &rows {
            println!("  {:?}", r);
        }
    }
}
