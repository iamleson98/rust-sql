//! Layer-by-layer cost breakdown of the remaining benchmark gaps:
//!   1. point lookup by rowid (engine loses ~8% in bench_compare)
//!   2. GROUP BY 100 buckets (~4%)
//! Run: cargo run --release --example probe_gaps2

use rustqlite::storage::btree::Btree;
use rustqlite::types::Value;
use rustqlite::Database;

fn ns(d: std::time::Duration, n: usize) -> f64 {
    d.as_secs_f64() * 1e9 / n as f64
}

fn best(mut f: impl FnMut() -> std::time::Duration) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let d = f();
        if d < best { best = d; }
    }
    best
}

fn main() {
    // ---------- build the same table as bench_compare (10k rows) ----------
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    {
        // fast bulk insert
        let mut vals = String::new();
        for i in 1..=10000i64 {
            vals.push_str(&format!(
                "({i}, 'user{:04}', {v}, {s}),\n",
                v = (i * 37) % 10000,
                s = (i % 1000) as f64 + 0.5
            ));
        }
        vals.pop();
        vals.pop();
        let sql = format!("INSERT INTO t (id, name, val, score) VALUES {}", vals);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    db.flush().unwrap();

    let n_rows: i64 = db
        .query("SELECT COUNT(*) FROM t", [])
        .unwrap()
        .first()
        .and_then(|row| row.first())
        .map(|v| v.as_integer())
        .unwrap_or(0);
    println!("rows: {}", n_rows);

    const OPS: usize = 100_000;
    let sql = "SELECT name, val, score FROM t WHERE id = ?";

    // warm caches
    for i in 1..=100i64 {
        let _ = db.query(sql, [Value::Integer(i)]).unwrap();
    }

    // ---- L0: full public query loop (bench shape, scaled up) ----
    let d = best(|| {
        let start = std::time::Instant::now();
        for i in 1..=OPS as i64 {
            let target = (i % 1000) + 1;
            let _ = db.query(sql, [Value::Integer(target)]).unwrap();
        }
        start.elapsed()
    });
    println!("L0 full query()            {:>8.1} ns/op", ns(d, OPS));

    // ---- L2: raw Btree lookup with the same projection decode ----
    let table = db.catalog_ref().get_table("t").unwrap();
    let root = table.root_page;
    let ncols = table.n_columns();
    let rowid_alias = table.rowid_alias;
    let project: Vec<usize> = vec![1, 2, 3]; // name, val, score
    let d = best(|| {
        let start = std::time::Instant::now();
        for i in 1..=OPS as i64 {
            let target = (i % 1000) + 1;
            let mut bt = Btree::new(db.pager(), root, false);
            let _ = bt
                .lookup_table_with(target, |payload| {
                    let mut out = Vec::with_capacity(3);
                    rustqlite::storage::row_codec::decode_row_selective(
                        payload, ncols, &project, target, rowid_alias, &mut out,
                    )
                    .map(|_| out)
                })
                .unwrap();
        }
        start.elapsed()
    });
    println!("L2 btree lookup + decode   {:>8.1} ns/op", ns(d, OPS));

    // ---- L3: raw Btree lookup, NO decode ----
    let d = best(|| {
        let start = std::time::Instant::now();
        for i in 1..=OPS as i64 {
            let target = (i % 1000) + 1;
            let mut bt = Btree::new(db.pager(), root, false);
            let _ = bt.lookup_table_with(target, |_| Ok(0usize)).unwrap();
        }
        start.elapsed()
    });
    println!("L3 btree lookup, no decode {:>8.1} ns/op", ns(d, OPS));

    // ---- L4: Btree::new alone ----
    let d = best(|| {
        let start = std::time::Instant::now();
        for _ in 0..OPS {
            let _ = Btree::new(db.pager(), root, false);
        }
        start.elapsed()
    });
    println!("L4 Btree::new only         {:>8.1} ns/op", ns(d, OPS));

    // ---- L5: alloc the result shape ----
    let d = best(|| {
        let start = std::time::Instant::now();
        for i in 1..=OPS as i64 {
            let _ = vec![vec![
                Value::Text(format!("user{:04}", i % 1000).into()),
                Value::Integer(i % 10000),
                Value::Real((i % 1000) as f64),
            ]];
        }
        start.elapsed()
    });
    println!("L5 alloc row materialize   {:>8.1} ns/op", ns(d, OPS));

    // ---- L6: cache lookup only (query a rowid that needs no engine work):
    // impossible via public API; approximate the memo path by re-querying
    // with an always-missing rowid (same path length, decode skipped).
    let d = best(|| {
        let start = std::time::Instant::now();
        for i in 1..=OPS as i64 {
            let target = 10_000_000 + (i % 1000);
            let _ = db.query(sql, [Value::Integer(target)]).unwrap();
        }
        start.elapsed()
    });
    println!("L6 query() miss (no row)   {:>8.1} ns/op", ns(d, OPS));

    // ---------- GROUP BY layers ----------
    println!("\n--- GROUP BY workloads (per call, 10k rows) ---");
    let gsql = "SELECT val / 100, COUNT(*), SUM(val), AVG(val) FROM t GROUP BY val / 100";
    for _ in 0..3 {
        let _ = db.query(gsql, []).unwrap();
    }
    let d = best(|| {
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let _ = db.query(gsql, []).unwrap();
        }
        start.elapsed()
    });
    println!(
        "GROUP BY (100 buckets)     {:>8.1} µs/call",
        d.as_secs_f64() * 1e6 / 20.0
    );

    let d = best(|| {
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let _ = db.query("SELECT COUNT(*), SUM(val), AVG(val) FROM t", []).unwrap();
        }
        start.elapsed()
    });
    println!(
        "plain aggregate            {:>8.1} µs/call",
        d.as_secs_f64() * 1e6 / 20.0
    );

    let d = best(|| {
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        }
        start.elapsed()
    });
    println!(
        "COUNT(*) bare              {:>8.1} µs/call",
        d.as_secs_f64() * 1e6 / 20.0
    );
}
