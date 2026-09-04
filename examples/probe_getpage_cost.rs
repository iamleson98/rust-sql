// Time get_page inside vs outside a transaction (savepoint capture cost)
// and the raw cache-hit path.
fn main() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a)", []).unwrap();
    db.execute("INSERT INTO t VALUES (1)", []).unwrap();

    // Grab the pager through the public API? Not exposed — use a probe
    // through behavior instead: time N inserts inside a txn (get_page
    // with savepoint active) vs autocommit (no savepoint).
    let n = 20_000i64;
    for (label, txn) in [("autocommit", false), ("in-txn", true)] {
        let mut d2 = rustqlite::Database::open_in_memory().unwrap();
        d2.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER)", [])
            .unwrap();
        if txn {
            d2.execute("BEGIN", []).unwrap();
        }
        let t = std::time::Instant::now();
        for i in 1..=n {
            d2.execute(
                "INSERT INTO t (b) VALUES (?)",
                [rustqlite::Value::Integer(i)],
            )
            .unwrap();
        }
        if txn {
            d2.execute("COMMIT", []).unwrap();
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("{label}: {us:.3}us/row");
    }
}
