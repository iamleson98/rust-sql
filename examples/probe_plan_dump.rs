fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
    for sql in [
        "EXPLAIN QUERY PLAN SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1",
        "EXPLAIN QUERY PLAN SELECT a FROM bench WHERE a BETWEEN ? AND ?",
        "EXPLAIN QUERY PLAN SELECT a FROM bench WHERE a % 10 = 0 LIMIT 5",
    ] {
        println!("--- {sql}");
        for row in db.query(sql, ()).unwrap() {
            let parts: Vec<String> = row.iter().map(|v| format!("{v:?}")).collect();
            println!("   {parts:?}");
        }
    }
}
