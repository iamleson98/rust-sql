//! Probe: multi-row VALUES insert with an index — before/after the index
//! append-hint + pre-resolved columns optimization.
use std::time::Instant;

fn main() {
    // rustqlite
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    // warmup
    {
        let values: Vec<String> = (1..=200i64)
            .map(|i| format!("('w{}', {}, {})", i, i, i as f64))
            .collect();
        db.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let n = 10_000i64;
    let batch = 500;
    let start = Instant::now();
    for chunk_start in (1..=n).step_by(batch) {
        let chunk_end = (chunk_start + batch as i64 - 1).min(n);
        let values: Vec<String> = (chunk_start..=chunk_end)
            .map(|i| format!("('name{}', {}, {})", i, i * 2, i as f64 * 1.5))
            .collect();
        db.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let d_r = start.elapsed();
    let cnt: i64 = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer();
    let idx_cnt: i64 = db
        .query("SELECT COUNT(*) FROM t WHERE val > 0", [])
        .unwrap()[0][0]
        .as_integer();
    println!(
        "rustqlite multirow 10k (indexed): {:?} rows={} idx_scan={}",
        d_r, cnt, idx_cnt
    );

    // SQLite
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL); CREATE INDEX idx_val ON t(val);").unwrap();
    {
        let values: Vec<String> = (1..=200i64)
            .map(|i| format!("('w{}', {}, {})", i, i, i as f64))
            .collect();
        conn.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let start = Instant::now();
    for chunk_start in (1..=n).step_by(batch) {
        let chunk_end = (chunk_start + batch as i64 - 1).min(n);
        let values: Vec<String> = (chunk_start..=chunk_end)
            .map(|i| format!("('name{}', {}, {})", i, i * 2, i as f64 * 1.5))
            .collect();
        conn.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let d_s = start.elapsed();
    println!("sqlite     multirow 10k (indexed): {:?}", d_s);
    println!(
        "ratio: {:.2}x {}",
        if d_r < d_s { "FASTER" } else { "slower" },
        d_s.as_secs_f64() / d_r.as_secs_f64()
    );

    // Random-order variant: the hint must fall back gracefully.
    let mut db2 = rustqlite::Database::open_in_memory().unwrap();
    db2.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db2.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let vals: Vec<i64> = (0..10_000).map(|_| (rand() % 1_000_000) as i64).collect();
    let start = Instant::now();
    for chunk in vals.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|v| format!("('n{}', {}, 1.5)", v, v))
            .collect();
        db2.execute(
            &format!(
                "INSERT INTO t (name, val, score) VALUES {}",
                values.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    let d_rand = start.elapsed();
    let cnt2: i64 = db2
        .query("SELECT COUNT(*) FROM t WHERE val BETWEEN 100 AND 200", [])
        .unwrap()[0][0]
        .as_integer();
    println!(
        "rustqlite random-order 10k: {:?} (sanity idx count={})",
        d_rand, cnt2
    );

    // SQLite random
    let conn2 = rusqlite::Connection::open_in_memory().unwrap();
    conn2.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL); CREATE INDEX idx_val ON t(val);").unwrap();
    let start = Instant::now();
    for chunk in vals.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|v| format!("('n{}', {}, 1.5)", v, v))
            .collect();
        conn2
            .execute(
                &format!(
                    "INSERT INTO t (name, val, score) VALUES {}",
                    values.join(", ")
                ),
                [],
            )
            .unwrap();
    }
    let d_rand_s = start.elapsed();
    println!(
        "sqlite     random-order 10k: {:?} (ratio {:.2}x)",
        d_rand_s,
        d_rand_s.as_secs_f64() / d_rand.as_secs_f64()
    );
}
