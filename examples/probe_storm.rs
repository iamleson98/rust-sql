//! Isolate the "first read after a write storm" penalty.
//! The bench_compare single-shot numbers for indexed point lookups and
//! 3-table joins are dominated by a ~40-175us cost that hits the FIRST
//! read query after a COMMIT — this probe decomposes it.
use rustqlite::types::Value;
use rustqlite::Database;
use std::time::Instant;

fn us(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e6 + d.as_nanos() as f64 * 1e-3
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // Warm everything: statement cached, pages resident.
    let sql = "SELECT id, name, score FROM t WHERE val = ?";
    for i in 1..=1000i64 {
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
    for _ in 0..50 {
        let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    }
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    println!(
        "warm query (no storm):        {:>7.1} us",
        us(start.elapsed())
    );

    // --- Storm 1: small write txn, then the SAME cached query ---
    db.execute("BEGIN", []).unwrap();
    for i in 1001..=1100i64 {
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
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    println!(
        "after 100-row COMMIT:         {:>7.1} us",
        us(start.elapsed())
    );

    // --- Storm 2: BIG write txn (like bench setup), same cached query ---
    db.execute("BEGIN", []).unwrap();
    for i in 2000..=52000i64 {
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
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    println!(
        "after 50k-row COMMIT:         {:>7.1} us",
        us(start.elapsed())
    );
    // second query right after
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(501)]).unwrap();
    println!(
        "  2nd query after storm:      {:>7.1} us",
        us(start.elapsed())
    );
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(502)]).unwrap();
    println!(
        "  3rd query after storm:      {:>7.1} us",
        us(start.elapsed())
    );

    // --- Is it the allocator? Allocate/free a bunch first, THEN query. ---
    db.execute("BEGIN", []).unwrap();
    for i in 60000..=110000i64 {
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
    // churn the allocator with unrelated allocs first
    let mut sink: Vec<Vec<u8>> = Vec::new();
    for i in 0..1000 {
        sink.push(vec![0u8; i % 128]);
    }
    drop(sink);
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    println!(
        "after 50k COMMIT + alloc churn:{:>6.1} us",
        us(start.elapsed())
    );

    // --- FRESH statement after storm: parse+plan cost ---
    let v: String = format!("SELECT id, name, score FROM t WHERE val = ? /* {} */", 42);
    let start = Instant::now();
    let _ = db.query(&v, [Value::Integer(500)]).unwrap();
    println!(
        "fresh stmt after storm:       {:>7.1} us",
        us(start.elapsed())
    );

    // --- page-touch cost: full scan right after a storm (all pages) ---
    let start = Instant::now();
    let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    println!(
        "COUNT(*) after storm:         {:>7.1} us",
        us(start.elapsed())
    );

    // Steady state reference
    let n = 2000;
    let start = Instant::now();
    for i in 0..n {
        let _ = db.query(sql, [Value::Integer((i % 1000) + 1)]).unwrap();
    }
    println!(
        "steady idx-point:             {:>7.1} ns/op",
        us(start.elapsed()) / n as f64 * 1000.0
    );

    // --- CREATE INDEX storm + first query (bench section 3 pattern) ---
    db.execute("DROP INDEX idx_val", []).unwrap();
    let start = Instant::now();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    println!(
        "CREATE INDEX (110k rows):     {:>7.1} us",
        us(start.elapsed())
    );
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    println!(
        "  1st query after CREATE INDEX:{:>6.1} us",
        us(start.elapsed())
    );
    let start = Instant::now();
    let _ = db.query(sql, [Value::Integer(500)]).unwrap();
    println!(
        "  2nd query after CREATE INDEX:{:>6.1} us",
        us(start.elapsed())
    );
}
