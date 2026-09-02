//! Decompose the indexed point-lookup cost: API fixed overhead (no-op
//! query), rowid point lookup (API + table descent), indexed lookup
//! (API + index descent + table descent). Steady state, 100k ops each.
use rustqlite::types::Value;
use rustqlite::Database;

fn ns(d: std::time::Duration) -> f64 {
    d.as_secs() as f64 * 1e9 + d.as_nanos() as f64
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let ins = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=10000i64 {
        db.execute(
            ins,
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // (C) API fixed cost: cached statement, zero-row result.
    // `SELECT id FROM t WHERE val = -1` — full pipeline, no matching rows,
    // one index descent to a cold-ish leaf, NO table fetch, NO row decode.
    let sql_miss = "SELECT id, name, score FROM t WHERE val = ?";
    // (A) full indexed hit
    let sql_idx = "SELECT id, name, score FROM t WHERE val = ?";
    // (B) rowid point
    let sql_rid = "SELECT id, name, score FROM t WHERE id = ?";

    let warm = 2000;
    for i in 0..warm {
        let _ = db
            .query(sql_idx, [Value::Integer((i % 10000 + 1) as i64 * 2)])
            .unwrap();
        let _ = db
            .query(sql_rid, [Value::Integer((i % 10000 + 1) as i64)])
            .unwrap();
        let _ = db.query(sql_miss, [Value::Integer(-1)]).unwrap();
    }

    let n = 100_000;
    // (A) indexed hit: cycle all 10k vals so hints see realistic rotation.
    let t = std::time::Instant::now();
    for i in 0..n {
        let target = ((i % 10000) as i64 + 1) * 2;
        let _ = db.query(sql_idx, [Value::Integer(target)]).unwrap();
    }
    let a = ns(t.elapsed()) / n as f64;
    println!("indexed hit (A):            {:>7.1} ns/op", a);

    // (B) rowid hit: cycle all 10k ids.
    let t = std::time::Instant::now();
    for i in 0..n {
        let target = (i % 10000) as i64 + 1;
        let _ = db.query(sql_rid, [Value::Integer(target)]).unwrap();
    }
    let b = ns(t.elapsed()) / n as f64;
    println!("rowid hit (B):              {:>7.1} ns/op", b);

    // (C) miss: same pipeline, no row decode/fetch.
    let t = std::time::Instant::now();
    for i in 0..n {
        let _ = db
            .query(sql_miss, [Value::Integer(-1 - (i % 100) as i64)])
            .unwrap();
    }
    let c = ns(t.elapsed()) / n as f64;
    println!("indexed miss (C):           {:>7.1} ns/op", c);

    println!();
    println!("index descent + seek:       {:>7.1} ns (C - fixed)", c);
    println!(
        "rowid descent+decode+row:   {:>7.1} ns (B - fixed est.)",
        b - c
    );
    println!("table fetch on hit:         {:>7.1} ns (A - C)", a - c);
}
