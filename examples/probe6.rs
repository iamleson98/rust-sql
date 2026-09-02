fn main() {
    // 1: 1MiB text row
    {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE s (id INTEGER PRIMARY KEY, t TEXT, b BLOB)",
            [],
        )
        .unwrap();
        let big = "x".repeat(1_048_576);
        let r = db.execute(
            "INSERT INTO s (t) VALUES (?)",
            [rustqlite::Value::Text(big.into())],
        );
        println!("1MiB text insert: {:?}", r.is_ok());
        let r = db.query("SELECT length(t) FROM s", []);
        println!("  read back: {:?}", r.iter().cloned().collect::<Vec<_>>());
    }
    println!("--- case 1 done");
}
