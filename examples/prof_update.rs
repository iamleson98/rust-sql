fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10_000i64 {
        db.execute(ins, [
            rustqlite::Value::Text(format!("name{}", i)),
            rustqlite::Value::Integer(i * 2),
            rustqlite::Value::Real(i as f64 * 1.5),
        ]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // warmup
    for i in 1..=100i64 {
        db.execute("UPDATE t SET score = ? WHERE id = ?", [rustqlite::Value::Real(i as f64), rustqlite::Value::Integer(i)]).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let start = std::time::Instant::now();
    for i in 1..=2000i64 {
        let score = i as f64 * 2.5;
        let id = (i % 1000) + 1;
        db.execute("UPDATE t SET score = ? WHERE id = ?", [rustqlite::Value::Real(score), rustqlite::Value::Integer(id)]).unwrap();
    }
    println!("2000 updates: {:?}", start.elapsed());
}
