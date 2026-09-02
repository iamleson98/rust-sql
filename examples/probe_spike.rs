//! Decompose the single-shot spike: why does the FIRST query after a
//! previous workload cost ~10 µs when steady state is ~1 µs?
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 / 1e3
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("user{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let sql_pt = "SELECT name, val, score FROM t WHERE id = ?";
    let sql_rng = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";

    // warm both statements
    for _ in 0..5 {
        let _ = db
            .query(sql_rng, [Value::Integer(1), Value::Integer(2)])
            .unwrap();
        let _ = db.query(sql_pt, [Value::Integer(5)]).unwrap();
    }

    // Reproduce bench pattern: 1000 point lookups (alloc/free storm),
    // then time individual range queries #1..#5.
    for i in 0..1000 {
        let _ = db.query(sql_pt, [Value::Integer((i % 1000) + 1)]).unwrap();
    }
    println!("--- after 1000 point lookups, individual range queries:");
    for k in 1..=5 {
        let start = Instant::now();
        let _ = db
            .query(sql_rng, [Value::Integer(1000), Value::Integer(1009)])
            .unwrap();
        println!("  range query #{}: {:>8.2} us", k, us(start.elapsed()));
    }

    // Now: storm WITHOUT allocations in the storm (UPDATE? no — just wait
    // 11ms so mimalloc's delayed purge fires on a timer, then query).
    for i in 0..1000 {
        let _ = db.query(sql_pt, [Value::Integer((i % 1000) + 1)]).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    println!("--- after storm + 50ms sleep (mimalloc timer purge window):");
    for k in 1..=3 {
        let start = Instant::now();
        let _ = db
            .query(sql_rng, [Value::Integer(1000), Value::Integer(1009)])
            .unwrap();
        println!("  range query #{}: {:>8.2} us", k, us(start.elapsed()));
    }

    // Control: fresh engine state — same query in a tight loop.
    println!("--- tight loop of 1000 range queries (steady state):");
    let start = Instant::now();
    for _ in 0..1000 {
        let _ = db
            .query(sql_rng, [Value::Integer(1000), Value::Integer(1009)])
            .unwrap();
    }
    println!("  avg: {:>8.2} us", us(start.elapsed()) / 1000.0);

    // Where does the spike live? Repeat storm->single-shot with a plain
    // Vec alloc/free storm instead of queries (isolates mimalloc purge).
    println!("--- pure Vec<u8> alloc/free storm (1000 x 64B), then range query:");
    for _ in 0..3 {
        let mut v: Vec<Vec<u8>> = Vec::new();
        for i in 0..1000 {
            v.push(vec![0u8; (i % 64) + 16]);
        }
        drop(v);
        let start = Instant::now();
        let _ = db
            .query(sql_rng, [Value::Integer(1000), Value::Integer(1009)])
            .unwrap();
        println!(
            "  range query after Vec storm: {:>8.2} us",
            us(start.elapsed())
        );
    }
}
