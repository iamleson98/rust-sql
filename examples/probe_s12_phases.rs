// S12 shape profiling: single-row parameterized INSERTs into a 5-index
// table, 15k rows, inside one transaction — the exact torture workload.
fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE m (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d INTEGER)",
        [],
    )
    .unwrap();
    for idx_sql in [
        "CREATE INDEX ia ON m(a)",
        "CREATE INDEX ib ON m(b)",
        "CREATE INDEX ic ON m(c)",
        "CREATE INDEX idd ON m(d)",
        "CREATE INDEX ida ON m(d, a)",
    ] {
        db.execute(idx_sql, []).unwrap();
    }
    let rows: i64 = 15_000;
    rustqlite::api::profile::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    rustqlite::api::profile::reset();
    let t = std::time::Instant::now();
    db.execute("BEGIN", []).unwrap();
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
    db.execute("COMMIT", []).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    rustqlite::api::profile::dump();
    println!(
        "insert total: {ms:.1}ms ({:.2}us/row over {rows} rows)",
        ms * 1000.0 / rows as f64
    );

    // Per-phase attribution of the whole loop, using the profiler above:
    // parse / plan / cache / exec. exec includes encode + 5 index appends.
    rustqlite::api::profile::ENABLED.store(false, std::sync::atomic::Ordering::Relaxed);
    per_index_attr(rows);
}

// Per-index attribution (appended by the profiling session).
#[allow(dead_code)]
fn per_index_attr(rows: i64) {
    for (label, n_idx) in [("0 idx", 0usize), ("1 idx", 1), ("5 idx", 5)] {
        let mut db2 = rustqlite::Database::open_in_memory().unwrap();
        db2.execute(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT, d INTEGER)",
            [],
        )
        .unwrap();
        for k in 0..n_idx {
            db2.execute(&format!("CREATE INDEX i{k} ON m(a)"), [])
                .unwrap();
        }
        let t = std::time::Instant::now();
        db2.execute("BEGIN", []).unwrap();
        for i in 1..=rows {
            db2.execute(
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
        db2.execute("COMMIT", []).unwrap();
        println!(
            "{label}: {:.2}us/row",
            t.elapsed().as_secs_f64() * 1e6 / rows as f64
        );
    }
}
