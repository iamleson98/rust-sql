//! Failure mode: SQLite file with >page index keys loading into rustqlite.
fn main() {
    let path = std::env::temp_dir().join("rq_idx_ovf.db");
    let _ = std::fs::remove_file(&path);
    // Build with real SQLite.
    let conn = rusqlite::Connection::open(path.to_str().unwrap()).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, long_text TEXT);
         CREATE INDEX idx_long ON t(long_text);",
    )
    .unwrap();
    for i in 0..20i64 {
        let big = "x".repeat(9000 + (i as usize) * 100);
        conn.execute(
            "INSERT INTO t (id, long_text) VALUES (?, ?)",
            rusqlite::params![i, big],
        )
        .unwrap();
    }
    drop(conn);
    // Load in rustqlite.
    match rustqlite::Database::open(path.to_str().unwrap()) {
        Ok(db) => {
            let rows = db
                .query("SELECT COUNT(*) FROM t WHERE long_text > ''", [])
                .unwrap();
            println!("LOADED OK: count = {:?}", rows[0][0]);
            let _ = std::fs::remove_file(&path);
        }
        Err(e) => println!("LOAD FAILED: {:?}", e),
    }
}
