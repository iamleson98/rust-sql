fn main() {
    let db = rustqlite::Database::open_in_memory().unwrap();
    for sql in [
        "EXPLAIN SELECT - 10 BETWEEN 8 AND 95",
        "EXPLAIN SELECT ( - 10 ) BETWEEN 8 AND 95",
    ] {
        println!("== {sql}");
        match db.query(sql, []) {
            Ok(rows) => {
                for r in rows.iter().take(14) {
                    println!(
                        "   {}",
                        r.iter()
                            .map(|v| v.as_text().to_string())
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                }
            }
            Err(e) => println!("   ERR {e}"),
        }
    }
}
