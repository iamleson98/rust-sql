// A/B proof probe: page-fetch accounting for the scattered-index insert
// loop. If the re-pin walk is gone, per-row cache fetches for a scattered
// 1-index insert drop by ~2 (the right-edge walk at depth 2).
use rustqlite::api::Database;
use rustqlite::Value;

fn main() {
    for (label, n_idx) in [("0 idx", 0usize), ("1 idx", 1), ("5 idx", 5)] {
        let db = Database::open_in_memory().unwrap();
        let mut db = db;
        db.execute(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d INTEGER)",
            [],
        )
        .unwrap();
        for k in 0..n_idx {
            db.execute(&format!("CREATE INDEX i{k} ON m(a)"), [])
                .unwrap();
        }
        let rows: i64 = 20_000;
        db.execute("BEGIN", []).unwrap();
        // warm the tree so depth-2+ structure exists before measuring
        for i in 1..=2_000i64 {
            db.execute(
                "INSERT INTO m (id, a, b, c, d) VALUES (?, ?, ?, ?, ?)",
                [
                    Value::Integer(i),
                    Value::Integer((i * 7919) % 100_003),
                    Value::Real(i as f64 * 0.5),
                    Value::Text(format!("c{i}").into()),
                    Value::Integer(i % 1000),
                ],
            )
            .unwrap();
        }
        let fetch0 = db.pager().cache_hits() + db.pager().cache_misses();
        let t = std::time::Instant::now();
        for i in 2_001..=rows {
            db.execute(
                "INSERT INTO m (id, a, b, c, d) VALUES (?, ?, ?, ?, ?)",
                [
                    Value::Integer(i),
                    Value::Integer((i * 7919) % 100_003),
                    Value::Real(i as f64 * 0.5),
                    Value::Text(format!("c{i}").into()),
                    Value::Integer(i % 1000),
                ],
            )
            .unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let fetch1 = db.pager().cache_hits() + db.pager().cache_misses();
        let n = (rows - 2000) as f64;
        println!(
            "{label}: {ms:.1}ms  ({:.2}us/row)  fetches/row: {:.2}",
            ms * 1000.0 / n,
            (fetch1 - fetch0) as f64 / n,
        );
        db.execute("COMMIT", []).unwrap();
    }
}
