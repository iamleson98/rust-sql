//! Diagnose random-order insert cost: no-index vs indexed, and split counts.
use std::time::Instant;

fn main() {
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let vals: Vec<i64> = (0..10_000).map(|_| (rand() % 1_000_000) as i64).collect();

    // A: random order, NO index.
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    let start = Instant::now();
    for chunk in vals.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|v| format!("('n{}', {}, 1.5)", v, v))
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
    println!("random no-index:  {:?}", start.elapsed());

    // B: random order, WITH index.
    let mut db2 = rustqlite::Database::open_in_memory().unwrap();
    db2.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db2.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
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
    println!("random indexed:   {:?}", start.elapsed());
    // Verify index correctness.
    let total: i64 = db2
        .query("SELECT COUNT(*) FROM t WHERE val >= 0", [])
        .unwrap()[0][0]
        .as_integer();
    println!("  index count check: {} (expect 10000)", total);

    // C: SQLite both.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
    )
    .unwrap();
    let start = Instant::now();
    for chunk in vals.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|v| format!("('n{}', {}, 1.5)", v, v))
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
    println!("sqlite no-index:  {:?}", start.elapsed());
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
    println!("sqlite indexed:   {:?}", start.elapsed());
}
