//! Scan-cost isolation probe: btree-walk vs per-column decode vs full row
//! materialization, using public-API query shapes over a 10k-row table.
//! Run pinned and unpinned to expose scheduler/hardware sensitivity.

use rustqlite::Database;

fn time_n(db: &Database, sql: &str, n: usize) -> f64 {
    // warm
    let _ = db.query(sql, []).unwrap();
    let t = std::time::Instant::now();
    for _ in 0..n {
        let _ = db.query(sql, []).unwrap();
    }
    t.elapsed().as_nanos() as f64 / n as f64 / 1000.0 // us per query
}

fn main() {
    let n = 10_000;
    let rounds = 200;
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    let mut i = 1;
    while i <= n {
        let end = (i + 99).min(n);
        let values: String = (i..=end)
            .map(|j| format!("('name{}', {}, {})", j, j, j))
            .collect::<Vec<_>>()
            .join(",");
        db.execute(
            &format!("INSERT INTO t (name, val, score) VALUES {values}"),
            [],
        )
        .unwrap();
        i = end + 1;
    }
    db.execute("COMMIT", []).unwrap();

    let rows_per_query = n as f64;
    let shapes = [
        (
            "rowid-alias only (walk+filter)  ",
            "SELECT id FROM t WHERE id > 0",
        ),
        ("SUM(val) (walk+decode1+agg)     ", "SELECT SUM(val) FROM t"),
        (
            "SUM(val,score,id) (decode3)     ",
            "SELECT SUM(val), SUM(score), SUM(id) FROM t",
        ),
        ("SELECT * (decode4+materialize)  ", "SELECT * FROM t"),
        (
            "SELECT * WHERE val>5000 (filter)",
            "SELECT * FROM t WHERE val > 5000",
        ),
        (
            "SUM+COUNT+AVG+WHERE (agg+filter)",
            "SELECT SUM(val), COUNT(*), AVG(score) FROM t WHERE val > 5000",
        ),
        (
            "5-aggs (bench shape)            ",
            "SELECT COUNT(*), SUM(val), MIN(val), MAX(val), AVG(val) FROM t",
        ),
    ];
    for (label, sql) in shapes {
        let us = time_n(&db, sql, rounds);
        println!(
            "{label} {:>8.1} us/query  = {:>7.1} ns/row",
            us,
            us * 1000.0 / rows_per_query
        );
    }
}
