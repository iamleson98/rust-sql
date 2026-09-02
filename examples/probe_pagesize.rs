fn main() {
    // Compare perf with 8192 (default) vs 4096 page size on the hot workloads.
    for page_size in [8192u32, 4096] {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(&format!("PRAGMA page_size = {page_size}"), [])
            .unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 0..10000i64 {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
                [
                    rustqlite::Value::Text(format!("item_{i}").into()),
                    rustqlite::Value::Integer(i),
                    rustqlite::Value::Real(i as f64),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();

        let t = std::time::Instant::now();
        for _ in 0..5 {
            db.execute("UPDATE t SET score = score + 1.0 WHERE val > 5000", [])
                .unwrap();
        }
        let upd = t.elapsed().as_secs_f64() * 1e3 / 5.0;

        let t = std::time::Instant::now();
        for _ in 0..50 {
            let _ = db.query("SELECT COUNT(*), SUM(val) FROM t WHERE val > 5000", ());
        }
        let agg = t.elapsed().as_secs_f64() * 1e3 / 50.0;

        let t = std::time::Instant::now();
        for _ in 0..50 {
            let _ = db.query("SELECT name FROM t WHERE id = 5000", ());
        }
        let pt = t.elapsed().as_secs_f64() * 1e3 / 50.0 * 1000.0;

        println!("page_size={page_size}: UPDATE range {upd:.2}ms | agg+filter {agg:.3}ms | point {pt:.1}µs");
    }
}
