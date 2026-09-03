//! Reproduce the CI-losing bench_compare rows in isolation so they can be
//! profiled/iterated locally under CPU pinning (CI-like conditions).
//!
//! Rows: Range scan (10/100/1000/5000) + UPDATE range (val > 5000).
//! Usage: taskset -c 0 cargo run --release --example probe_ci_rows

use std::time::{Duration, Instant};

use rusqlite::params;
use rustqlite::{Database, Value};

const MEDIUM: usize = 10_000;

fn best_of<const N: usize>(mut f: impl FnMut() -> Duration) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..N {
        let d = f();
        if d < best {
            best = d;
        }
    }
    best
}

// ---------- rustqlite side ----------

fn rq_open() -> Database {
    Database::open_in_memory().unwrap()
}

fn rq_create(db: &mut Database) {
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
}

fn rq_insert_single_in_txn(db: &mut Database, n: usize) {
    db.execute("BEGIN", []).unwrap();
    let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
    for i in 1..=n as i64 {
        db.execute(
            sql,
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn rq_range_scan(db: &mut Database, range: usize) -> Duration {
    let sql = "SELECT name, val, score FROM t WHERE id BETWEEN ? AND ?";
    let _ = db
        .query(sql, [Value::Integer(1), Value::Integer(2)])
        .unwrap();
    best_of::<7>(|| {
        let start = Instant::now();
        let _ = db
            .query(
                sql,
                [
                    Value::Integer(1000),
                    Value::Integer(1000 + range as i64 - 1),
                ],
            )
            .unwrap();
        start.elapsed()
    })
}

fn rq_update_range(db: &mut Database) -> Duration {
    for _ in 0..3 {
        db.execute("UPDATE t SET score = score + 1.0 WHERE val > 5000", [])
            .unwrap();
    }
    best_of::<5>(|| {
        let start = Instant::now();
        db.execute("UPDATE t SET score = score + 1.0 WHERE val > 5000", [])
            .unwrap();
        start.elapsed()
    })
}

// ---------- sqlite side ----------

fn sq_open() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

fn sq_create(conn: &rusqlite::Connection) {
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
}

fn sq_insert_single_in_txn(conn: &rusqlite::Connection, n: usize) {
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=n as i64 {
        conn.execute(
            "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
            params![format!("name{}", i), i, i as f64],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn sq_range_scan(conn: &rusqlite::Connection, range: usize) -> Duration {
    let mut stmt = conn
        .prepare("SELECT name, val, score FROM t WHERE id BETWEEN ?1 AND ?2")
        .unwrap();
    best_of::<7>(|| {
        let start = Instant::now();
        let mut rows = stmt.query(params![1000, 1000 + range as i64 - 1]).unwrap();
        while rows.next().unwrap().is_some() {}
        start.elapsed()
    })
}

/// Same drain, but the callback DECODES the 3 selected columns — the work
/// rustqlite's materialized `query()` API performs per row. Without this,
/// the harness compares decode+alloc work against a zero-decode drain.
fn sq_range_scan_fair(conn: &rusqlite::Connection, range: usize) -> Duration {
    let mut stmt = conn
        .prepare("SELECT name, val, score FROM t WHERE id BETWEEN ?1 AND ?2")
        .unwrap();
    best_of::<7>(|| {
        let start = Instant::now();
        let mut rows = stmt.query(params![1000, 1000 + range as i64 - 1]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let _name: String = row.get(0).unwrap();
            let _val: i64 = row.get(1).unwrap();
            let _score: f64 = row.get(2).unwrap();
        }
        start.elapsed()
    })
}

fn sq_update_range(conn: &rusqlite::Connection) -> Duration {
    for _ in 0..3 {
        conn.execute("UPDATE t SET score = score + 1.0 WHERE val > 5000", [])
            .unwrap();
    }
    best_of::<5>(|| {
        let start = Instant::now();
        conn.execute("UPDATE t SET score = score + 1.0 WHERE val > 5000", [])
            .unwrap();
        start.elapsed()
    })
}

fn fmt_ratio(d_r: Duration, d_s: Duration) -> String {
    let ratio = d_s.as_secs_f64() / d_r.as_secs_f64();
    format!(
        "{:>8.2}x  rq={:>9.2?}  sq={:>9.2?}",
        if ratio >= 1.0 { ratio } else { -ratio },
        d_r,
        d_s
    )
}

fn main() {
    let mut db_r = rq_open();
    rq_create(&mut db_r);
    rq_insert_single_in_txn(&mut db_r, MEDIUM);
    let conn_s = sq_open();
    sq_create(&conn_s);
    sq_insert_single_in_txn(&conn_s, MEDIUM);

    println!("== Range scans (materialized rows vs zero-decode drain) ==");
    for range in [10usize, 100, 1000, 5000] {
        let d_r = rq_range_scan(&mut db_r, range);
        let d_s = sq_range_scan(&conn_s, range);
        let d_f = sq_range_scan_fair(&conn_s, range);
        println!("  range {:>5}:  raw   {}", range, fmt_ratio(d_r, d_s));
        println!("               fair   {}", fmt_ratio(d_r, d_f));
    }

    // Phase 1: UPDATE range WITHOUT index (Filter{Scan} path).
    println!("== UPDATE range (NO index — full-scan+filter path) ==");
    {
        let d_r = rq_update_range(&mut db_r);
        let d_s = sq_update_range(&conn_s);
        println!("              {}", fmt_ratio(d_r, d_s));
    }

    // Phase 2: create idx_val (matches bench_compare Section 3 exactly),
    // then UPDATE range — the planner sees the index now.
    db_r.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    conn_s
        .execute("CREATE INDEX idx_val ON t(val)", [])
        .unwrap();
    println!("== UPDATE range (WITH idx_val — planner's choice) ==");
    {
        let d_r = rq_update_range(&mut db_r);
        let d_s = sq_update_range(&conn_s);
        println!("              {}", fmt_ratio(d_r, d_s));
    }

    // Phase 3: what plan does rustqlite actually pick? EXPLAIN QUERY PLAN.
    let plan = db_r
        .query(
            "EXPLAIN QUERY PLAN UPDATE t SET score = score + 1.0 WHERE val > 5000",
            [],
        )
        .unwrap();
    for row in &plan {
        let mut vals = String::new();
        for v in row {
            vals.push_str(&format!("{:?} ", v));
        }
        println!("  RQ PLAN: {}", vals);
    }

    // Phase 4: attribution. Same predicate, three shapes:
    //   (a) pure read via the index (scan + row fetch + decode)
    //   (b) UPDATE via rowid range (no index — pure table walk + patch)
    //   (c) the real thing (index-selected UPDATE)
    // (a) vs (c) isolates the write-apply cost; (b) vs (c) shows whether
    // the index detour pays for itself.
    {
        let sel_sql = "SELECT name, val, score FROM t WHERE val > 5000";
        let d_r = best_of::<5>(|| {
            let start = Instant::now();
            let _ = db_r.query(sel_sql, []).unwrap();
            start.elapsed()
        });
        let d_s = best_of::<5>(|| {
            let start = Instant::now();
            let _ = conn_s
                .prepare(sel_sql)
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .count();
            start.elapsed()
        });
        println!("  SELECT val>5000 (index):  rq={:?}  sq={:?}", d_r, d_s);
    }
    {
        let upd_rowid = "UPDATE t SET score = score + 1.0 WHERE id > 5000";
        for _ in 0..3 {
            db_r.execute(upd_rowid, []).unwrap();
        }
        let d_r = best_of::<5>(|| {
            let start = Instant::now();
            db_r.execute(upd_rowid, []).unwrap();
            start.elapsed()
        });
        for _ in 0..3 {
            conn_s.execute(upd_rowid, []).unwrap();
        }
        let d_s = best_of::<5>(|| {
            let start = Instant::now();
            conn_s.execute(upd_rowid, []).unwrap();
            start.elapsed()
        });
        println!("  UPDATE id>5000 (rowid):  rq={:?}  sq={:?}", d_r, d_s);
    }

    // Open cost check (the :memory: temp-file tax).
    println!("== open_in_memory cost ==");
    let start = Instant::now();
    for _ in 0..1000 {
        let _db = Database::open_in_memory().unwrap();
    }
    println!("  rustqlite: {:?}/open", start.elapsed() / 1000);
    let start = Instant::now();
    for _ in 0..1000 {
        let _c = rusqlite::Connection::open_in_memory().unwrap();
    }
    println!("  sqlite:    {:?}/open", start.elapsed() / 1000);
}
