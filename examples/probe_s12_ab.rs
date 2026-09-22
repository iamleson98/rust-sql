// Direct rq-vs-sq A/B for the S12 insert shape (5 secondary indexes,
// scattered/monotonic/cycling key mix), the exact comparison the CI
// torture gate makes.
fn main() {
    let rows: i64 = 25_000;
    let idx_sql = [
        "CREATE INDEX ia ON m(a)",
        "CREATE INDEX ib ON m(b)",
        "CREATE INDEX ic ON m(c)",
        "CREATE INDEX idd ON m(d)",
        "CREATE INDEX ida ON m(d, a)",
    ];

    // ---- rq ----
    let rq_ms = {
        let mut db = rustqlite::Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d INTEGER)",
            [],
        )
        .unwrap();
        for s in idx_sql {
            db.execute(s, []).unwrap();
        }
        db.execute("BEGIN", []).unwrap();
        let t = std::time::Instant::now();
        for i in 1..=rows {
            db.execute(
                "INSERT INTO m (id, a, b, c, d) VALUES (?, ?, ?, ?, ?)",
                [
                    rustqlite::Value::Integer(i),
                    rustqlite::Value::Integer((i * 7919) % 100_003),
                    rustqlite::Value::Real(i as f64 * 0.5),
                    rustqlite::Value::Text(format!("c{i}").into()),
                    rustqlite::Value::Integer(i % 1000),
                ],
            )
            .unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        db.execute("COMMIT", []).unwrap();
        ms
    };

    // ---- sq ----
    let sq_ms = {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d INTEGER)",
            [],
        )
        .unwrap();
        for s in idx_sql {
            conn.execute(s, []).unwrap();
        }
        conn.execute("BEGIN", []).unwrap();
        let t = std::time::Instant::now();
        for i in 1..=rows {
            conn.execute(
                "INSERT INTO m (id, a, b, c, d) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    i,
                    (i * 7919) % 100_003,
                    i as f64 * 0.5,
                    format!("c{i}"),
                    i % 1000
                ],
            )
            .unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        conn.execute("COMMIT", []).unwrap();
        ms
    };

    println!(
        "rq {rq_ms:.1}ms vs sq {sq_ms:.1}ms  -> {:.3}x {}",
        rq_ms / sq_ms,
        if rq_ms <= sq_ms * 1.15 {
            "PASS(<15%)"
        } else {
            "LOSS(>15%)"
        }
    );
}
