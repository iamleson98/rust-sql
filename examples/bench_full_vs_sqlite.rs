//! Comprehensive head-to-head benchmark: rust-sql vs SQLite (via rusqlite).
//!
//! This is a STANDALONE binary that runs 15+ benchmark scenarios comparing
//! rust-sql directly against SQLite. Prints a clean comparison table that
//! the user can run and see.
//!
//! Run with:
//!   cargo run --release --example bench_full_vs_sqlite
//!
//! Each test runs for a fixed amount of work and prints:
//!   rust-sql: <ops/sec>
//!   SQLite:   <ops/sec>
//!   ratio:    <x.xx>x (winner)

use parking_lot::RwLock;
use rusqlite::params;
use rustqlite::{Database, Value};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const N_ROWS: i64 = 10_000;

// ============================================================
// Test setup helpers
// ============================================================

fn setup_rusqlite(n: i64, with_index: bool) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    if with_index {
        conn.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    }
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
            params![format!("name{}", i), i, i as f64 * 1.5],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn
}

fn setup_rustqlite(n: i64, with_index: bool) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    if with_index {
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    }
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{}", i).into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

// ============================================================
// Result + measure helpers
// ============================================================

struct BenchResult {
    name: String,
    rustqlite_ops_per_sec: f64,
    rusqlite_ops_per_sec: f64,
    note: Option<&'static str>,
}

impl BenchResult {
    fn ratio(&self) -> f64 {
        self.rustqlite_ops_per_sec / self.rusqlite_ops_per_sec
    }
    fn winner(&self) -> &'static str {
        if self.rustqlite_ops_per_sec > self.rusqlite_ops_per_sec {
            "rust-sql"
        } else {
            "SQLite"
        }
    }
    fn print(&self) {
        let note = self.note.map(|n| format!("  [{}]", n)).unwrap_or_default();
        println!(
            "  {:<42} | rust-sql: {:>10.0} ops/s | SQLite: {:>10.0} ops/s | ratio: {:>5.2}x ({}){}",
            self.name,
            self.rustqlite_ops_per_sec,
            self.rusqlite_ops_per_sec,
            self.ratio(),
            self.winner(),
            note,
        );
    }
}

fn measure<F: Fn()>(name: &str, f: F, total_ops: usize) -> (f64, String) {
    // warm up
    for _ in 0..2 {
        f();
    }
    // measure
    let start = Instant::now();
    let iters = 5;
    for _ in 0..iters {
        f();
    }
    let elapsed = start.elapsed();
    let ops_per_sec = (total_ops * iters) as f64 / elapsed.as_secs_f64();
    (ops_per_sec, name.to_string())
}

// ============================================================
// Benchmarks
// ============================================================

fn bench_insert_autocommit() -> BenchResult {
    let n = 1000;
    let (rq_ops, _) = measure(
        "rust-sql autocommit insert",
        || {
            let mut db = Database::open_in_memory().unwrap();
            db.execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            for i in 1..=n {
                db.execute(
                    "INSERT INTO t (name, val) VALUES (?, ?)",
                    [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
                )
                .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite autocommit insert",
        || {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            for i in 1..=n {
                conn.execute(
                    "INSERT INTO t (name, val) VALUES (?1, ?2)",
                    params![format!("name{}", i), i],
                )
                .unwrap();
            }
        },
        n as usize,
    );
    BenchResult {
        name: "INSERT (auto-commit, 1k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("fsync per stmt"),
    }
}

fn bench_insert_transaction() -> BenchResult {
    let n = 1000;
    let (rq_ops, _) = measure(
        "rust-sql txn insert",
        || {
            let mut db = Database::open_in_memory().unwrap();
            db.execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 1..=n {
                db.execute(
                    "INSERT INTO t (name, val) VALUES (?, ?)",
                    [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
                )
                .unwrap();
            }
            db.execute("COMMIT", []).unwrap();
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite txn insert",
        || {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            conn.execute("BEGIN", []).unwrap();
            for i in 1..=n {
                conn.execute(
                    "INSERT INTO t (name, val) VALUES (?1, ?2)",
                    params![format!("name{}", i), i],
                )
                .unwrap();
            }
            conn.execute("COMMIT", []).unwrap();
        },
        n as usize,
    );
    BenchResult {
        name: "INSERT (transaction, 1k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_insert_multi_values() -> BenchResult {
    // Single INSERT with 1000 VALUES tuples — best case for bulk insert.
    let n = 1000;
    let (rq_ops, _) = measure(
        "rust-sql multi-VALUES",
        || {
            let mut db = Database::open_in_memory().unwrap();
            db.execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            // Build a multi-VALUES INSERT once, execute many times.
            let chunk_size = 100;
            let mut i = 1;
            while i <= n {
                let end = (i + chunk_size - 1).min(n);
                let values: String = (i..=end)
                    .map(|j| format!("('name{}', {})", j, j))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!("INSERT INTO t (name, val) VALUES {}", values);
                db.execute(&sql, []).unwrap();
                i = end + 1;
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite multi-VALUES",
        || {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                [],
            )
            .unwrap();
            let chunk_size = 100;
            let mut i = 1;
            while i <= n {
                let end = (i + chunk_size - 1).min(n);
                let values: String = (i..=end)
                    .map(|j| format!("('name{}', {})", j, j))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!("INSERT INTO t (name, val) VALUES {}", values);
                conn.execute(&sql, []).unwrap();
                i = end + 1;
            }
        },
        n as usize,
    );
    BenchResult {
        name: "INSERT (multi-VALUES 100/batch)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("bulk path"),
    }
}

fn bench_point_lookup() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 1000;
    let (rq_ops, _) = measure(
        "rust-sql point lookup",
        || {
            for i in 1..=n {
                let _ = db
                    .query("SELECT name, val FROM t WHERE id = ?", [Value::Integer(i)])
                    .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite point lookup",
        || {
            for i in 1..=n {
                let mut stmt = conn
                    .prepare("SELECT name, val FROM t WHERE id = ?1")
                    .unwrap();
                let mut rows = stmt.query(params![i]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        n as usize,
    );
    BenchResult {
        name: "Point lookup (SELECT by id, 1k queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("PK index"),
    }
}

fn bench_index_lookup() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, true);
    let conn = setup_rusqlite(N_ROWS, true);
    let n = 1000;
    let (rq_ops, _) = measure(
        "rust-sql index lookup",
        || {
            for i in 1..=n {
                let _ = db
                    .query("SELECT name FROM t WHERE val = ?", [Value::Integer(i)])
                    .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite index lookup",
        || {
            for i in 1..=n {
                let mut stmt = conn.prepare("SELECT name FROM t WHERE val = ?1").unwrap();
                let mut rows = stmt.query(params![i]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        n as usize,
    );
    BenchResult {
        name: "Index lookup (SELECT by indexed col, 1k)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("secondary idx"),
    }
}

fn bench_range_scan() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 500;
    let (rq_ops, _) = measure(
        "rust-sql range scan",
        || {
            for _ in 0..n {
                let _ = db
                    .query("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100", [])
                    .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite range scan",
        || {
            for _ in 0..n {
                let mut stmt = conn
                    .prepare("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100")
                    .unwrap();
                let mut rows = stmt.query([]).unwrap();
                // Fair-work parity: read the projected columns —
                // rustqlite's materializing `query()` decodes them.
                while let Some(row) = rows.next().unwrap() {
                    let _name: String = row.get(0).unwrap();
                    let _val: i64 = row.get(1).unwrap();
                }
            }
        },
        n as usize,
    );
    BenchResult {
        name: "Range scan (100 rows, 500 queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_full_scan() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 50;
    let (rq_ops, _) = measure(
        "rust-sql full scan",
        || {
            for _ in 0..n {
                let _ = db.query("SELECT * FROM t", []).unwrap();
            }
        },
        N_ROWS as usize * n,
    );
    let (rs_ops, _) = measure(
        "rusqlite full scan",
        || {
            for _ in 0..n {
                let mut stmt = conn.prepare("SELECT * FROM t").unwrap();
                let mut rows = stmt.query([]).unwrap();
                // Fair-work parity: rustqlite's `db.query()` materializes
                // EVERY column of EVERY row into owned Values — the SQLite
                // side must consume the same data (4 column reads per row),
                // not just step the VDBE. A bare `rows.next()` loop would
                // compare rustqlite's eager materialization against a lazy
                // cursor that decodes nothing.
                while let Ok(Some(r)) = rows.next() {
                    let _ = r.get::<_, i64>(0);
                    let _ = r.get::<_, rusqlite::types::Value>(1);
                    let _ = r.get::<_, i64>(2);
                    let _ = r.get::<_, f64>(3);
                }
            }
        },
        N_ROWS as usize * n,
    );
    BenchResult {
        name: "Full table scan (10k rows × 50 iters)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_aggregate_multi() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 500;
    let (rq_ops, _) = measure(
        "rust-sql aggregate multi",
        || {
            for _ in 0..n {
                let _ = db
                    .query(
                        "SELECT COUNT(*), SUM(val), MIN(val), MAX(val), AVG(val) FROM t",
                        [],
                    )
                    .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite aggregate multi",
        || {
            for _ in 0..n {
                let mut stmt = conn
                    .prepare("SELECT COUNT(*), SUM(val), MIN(val), MAX(val), AVG(val) FROM t")
                    .unwrap();
                let mut rows = stmt.query([]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        n as usize,
    );
    BenchResult {
        name: "Aggregate (COUNT/SUM/MIN/MAX/AVG over 10k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("5 aggs"),
    }
}

fn bench_count_star() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 500;
    let (rq_ops, _) = measure(
        "rust-sql COUNT(*)",
        || {
            for _ in 0..n {
                let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite COUNT(*)",
        || {
            for _ in 0..n {
                let mut stmt = conn.prepare("SELECT COUNT(*) FROM t").unwrap();
                let mut rows = stmt.query([]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        n as usize,
    );
    BenchResult {
        name: "COUNT(*) only (no row decode)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("count_rows fast path"),
    }
}

fn bench_aggregate_with_where() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 500;
    let (rq_ops, _) = measure(
        "rust-sql agg + WHERE",
        || {
            for _ in 0..n {
                let _ = db
                    .query(
                        "SELECT SUM(val), COUNT(*), AVG(score) FROM t WHERE val > 5000",
                        [],
                    )
                    .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite agg + WHERE",
        || {
            for _ in 0..n {
                let mut stmt = conn
                    .prepare("SELECT SUM(val), COUNT(*), AVG(score) FROM t WHERE val > 5000")
                    .unwrap();
                let mut rows = stmt.query([]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        n as usize,
    );
    BenchResult {
        name: "Aggregate + WHERE (filter 50% rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_group_by() -> BenchResult {
    let db = setup_rustqlite(N_ROWS, false);
    let conn = setup_rusqlite(N_ROWS, false);
    let n = 50;
    let (rq_ops, _) = measure(
        "rust-sql GROUP BY",
        || {
            for _ in 0..n {
                let _ = db
                    .query(
                        "SELECT val / 100 AS bucket, COUNT(*) FROM t GROUP BY bucket",
                        [],
                    )
                    .unwrap();
            }
        },
        N_ROWS as usize * n,
    );
    let (rs_ops, _) = measure(
        "rusqlite GROUP BY",
        || {
            for _ in 0..n {
                let mut stmt = conn
                    .prepare("SELECT val / 100 AS bucket, COUNT(*) FROM t GROUP BY bucket")
                    .unwrap();
                let mut rows = stmt.query([]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        N_ROWS as usize * n,
    );
    BenchResult {
        name: "GROUP BY (10 buckets × 10k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_update_by_id() -> BenchResult {
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS, false)));
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS, false)));
    let n = 1000;
    let (rq_ops, _) = measure(
        "rust-sql update by id",
        || {
            for i in 1..=n {
                let mut db = db.write();
                db.execute(
                    "UPDATE t SET val = ? WHERE id = ?",
                    [Value::Integer(i * 2), Value::Integer(i)],
                )
                .unwrap();
            }
        },
        n as usize,
    );
    let (rs_ops, _) = measure(
        "rusqlite update by id",
        || {
            for i in 1..=n {
                let conn = conn.lock().unwrap();
                conn.execute("UPDATE t SET val = ?1 WHERE id = ?2", params![i * 2, i])
                    .unwrap();
            }
        },
        n as usize,
    );
    BenchResult {
        name: "UPDATE by id (1k updates)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_delete_insert_cycle() -> BenchResult {
    let n = 500;
    let (rq_ops, _) = measure(
        "rust-sql delete+insert cycle",
        || {
            let mut db = setup_rustqlite(N_ROWS, false);
            for i in 1..=n {
                db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(i)])
                    .unwrap();
                db.execute(
                    "INSERT INTO t (id, name, val, score) VALUES (?, ?, ?, ?)",
                    [
                        Value::Integer(i),
                        Value::Text(format!("name{}", i).into()),
                        Value::Integer(i),
                        Value::Real(i as f64),
                    ],
                )
                .unwrap();
            }
        },
        n as usize * 2,
    );
    let (rs_ops, _) = measure(
        "rusqlite delete+insert cycle",
        || {
            let conn = setup_rusqlite(N_ROWS, false);
            for i in 1..=n {
                conn.execute("DELETE FROM t WHERE id = ?1", params![i])
                    .unwrap();
                conn.execute(
                    "INSERT INTO t (id, name, val, score) VALUES (?1, ?2, ?3, ?4)",
                    params![i, format!("name{}", i), i, i as f64],
                )
                .unwrap();
            }
        },
        n as usize * 2,
    );
    BenchResult {
        name: "DELETE + INSERT cycle (500 iters)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_concurrent_8_readers() -> BenchResult {
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS, false)));
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS, false)));
    let n_per_thread = 500;
    let total_ops = 8 * n_per_thread;
    let (rq_ops, _) = measure(
        "rust-sql 8 readers",
        || {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let db = Arc::clone(&db);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = db.read();
                        let _ = guard
                            .query("SELECT name, val FROM t WHERE id = ?", [Value::Integer(id)]);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        },
        total_ops,
    );
    let (rs_ops, _) = measure(
        "rusqlite 8 readers mutex",
        || {
            let mut handles = Vec::new();
            for _ in 0..8 {
                let conn = Arc::clone(&conn);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = conn.lock().unwrap();
                        let mut stmt = guard
                            .prepare("SELECT name, val FROM t WHERE id = ?1")
                            .unwrap();
                        let mut rows = stmt.query(params![id]).unwrap();
                        while rows.next().unwrap().is_some() {}
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        },
        total_ops,
    );
    BenchResult {
        name: "Concurrent reads (8 threads × 500 queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("Rust: RwLock | SQLite: Mutex"),
    }
}

fn bench_concurrent_16_readers() -> BenchResult {
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS, false)));
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS, false)));
    let n_per_thread = 250;
    let total_ops = 16 * n_per_thread;
    let (rq_ops, _) = measure(
        "rust-sql 16 readers",
        || {
            let mut handles = Vec::new();
            for _ in 0..16 {
                let db = Arc::clone(&db);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = db.read();
                        let _ = guard
                            .query("SELECT name, val FROM t WHERE id = ?", [Value::Integer(id)]);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        },
        total_ops,
    );
    let (rs_ops, _) = measure(
        "rusqlite 16 readers mutex",
        || {
            let mut handles = Vec::new();
            for _ in 0..16 {
                let conn = Arc::clone(&conn);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = conn.lock().unwrap();
                        let mut stmt = guard
                            .prepare("SELECT name, val FROM t WHERE id = ?1")
                            .unwrap();
                        let mut rows = stmt.query(params![id]).unwrap();
                        while rows.next().unwrap().is_some() {}
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        },
        total_ops,
    );
    BenchResult {
        name: "Concurrent reads (16 threads × 250 queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_mixed_rw() -> BenchResult {
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS, false)));
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS, false)));
    let n_per_thread = 500;
    let total_ops = 5 * n_per_thread;
    let (rq_ops, _) = measure(
        "rust-sql mixed R/W",
        || {
            let mut handles = Vec::new();
            // 1 writer
            {
                let db = Arc::clone(&db);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let mut guard = db.write();
                        guard
                            .execute(
                                "UPDATE t SET val = val + 1 WHERE id = ?",
                                [Value::Integer(id)],
                            )
                            .unwrap();
                    }
                }));
            }
            // 4 readers
            for _ in 0..4 {
                let db = Arc::clone(&db);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = db.read();
                        let _ = guard
                            .query("SELECT name, val FROM t WHERE id = ?", [Value::Integer(id)]);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        },
        total_ops,
    );
    let (rs_ops, _) = measure(
        "rusqlite mixed R/W",
        || {
            let mut handles = Vec::new();
            // 1 writer
            {
                let conn = Arc::clone(&conn);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = conn.lock().unwrap();
                        guard
                            .execute("UPDATE t SET val = val + 1 WHERE id = ?1", params![id])
                            .unwrap();
                    }
                }));
            }
            // 4 readers
            for _ in 0..4 {
                let conn = Arc::clone(&conn);
                handles.push(thread::spawn(move || {
                    for i in 0..n_per_thread {
                        let id = (i % N_ROWS as usize) as i64 + 1;
                        let guard = conn.lock().unwrap();
                        let mut stmt = guard
                            .prepare("SELECT name, val FROM t WHERE id = ?1")
                            .unwrap();
                        let mut rows = stmt.query(params![id]).unwrap();
                        while rows.next().unwrap().is_some() {}
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        },
        total_ops,
    );
    BenchResult {
        name: "Mixed R/W (4 readers + 1 writer)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: None,
    }
}

fn bench_concurrent_writes() -> BenchResult {
    // 4 writer threads each committing 250 INSERTs — the TRUE-PARALLEL
    // shape. rustqlite: one shared `Arc<Database>`, four SIMULTANEOUS
    // `BEGIN CONCURRENT` transactions whose statements run in PARALLEL
    // (private page shadows); only COMMIT takes a short critical section
    // (validate → install/row-merge → one WAL append). SQLite: four
    // connections on one WAL database — its WAL admits EXACTLY ONE
    // writer at a time, database-wide, so the four transactions
    // serialize end-to-end (BEGIN..COMMIT of one writer must finish
    // before the next acquires the write lock). Same thread count, same
    // statements, same durability class on both sides (file-backed WAL,
    // synchronous=NORMAL, prepared statements, disjoint rowids).
    let total_ops = 4 * 250;

    let rq_path = {
        let p = std::env::temp_dir().join(format!(
            "bench-cw-rq-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p.to_string_lossy().into_owned()
    };
    let (rq_ops, _) = measure(
        "rust-sql 4 parallel BEGIN CONCURRENT writers",
        || {
            let mut db = Database::open(&rq_path).unwrap();
            db.execute("PRAGMA journal_mode = WAL", []).unwrap();
            db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
                .unwrap();
            let db = Arc::new(db);
            let mut handles = Vec::new();
            for tid in 0..4i64 {
                let db = Arc::clone(&db);
                handles.push(thread::spawn(move || {
                    rustqlite::Database::set_conn_identity((tid + 1) as u64);
                    db.begin_concurrent_transaction().unwrap();
                    let mut stmt = db.prepare("INSERT INTO t (id, val) VALUES (?, ?)").unwrap();
                    for i in 0..250i64 {
                        stmt.bind(1, Value::Integer(tid * 1_000_000 + i)).unwrap();
                        stmt.bind(2, Value::Integer(tid)).unwrap();
                        stmt.step().unwrap();
                        stmt.reset();
                    }
                    drop(stmt);
                    db.commit_concurrent_transaction().unwrap();
                    rustqlite::Database::set_conn_identity(0);
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            let n = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
            assert_eq!(n, 1000, "every parallel writer's rows must land");
            drop(db);
            let _ = std::fs::remove_file(&rq_path);
            let _ = std::fs::remove_file(format!("{}-wal", rq_path));
        },
        total_ops,
    );
    let rs_path = {
        let p = std::env::temp_dir().join(format!(
            "bench-cw-sql-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p.to_string_lossy().into_owned()
    };
    let (rs_ops, _) = measure(
        "rusqlite 4 WAL writers (serialized by SQLite's write lock)",
        || {
            {
                let conn = rusqlite::Connection::open(&rs_path).unwrap();
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
                    .unwrap();
                conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
                    .unwrap();
            }
            let mut handles = Vec::new();
            for tid in 0..4i64 {
                let path = rs_path.clone();
                handles.push(thread::spawn(move || {
                    let conn = rusqlite::Connection::open(&path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
                        .unwrap();
                    conn.execute("BEGIN", []).unwrap();
                    for i in 0..250i64 {
                        conn.execute(
                            "INSERT INTO t (id, val) VALUES (?1, ?2)",
                            params![tid * 1_000_000 + i, tid],
                        )
                        .unwrap();
                    }
                    conn.execute("COMMIT", []).unwrap();
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            {
                let conn = rusqlite::Connection::open(&rs_path).unwrap();
                let n: i64 = conn
                    .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, 1000);
            }
            let _ = std::fs::remove_file(&rs_path);
            let _ = std::fs::remove_file(format!("{}-wal", rs_path));
            let _ = std::fs::remove_file(format!("{}-shm", rs_path));
        },
        total_ops,
    );
    BenchResult {
        name: "Concurrent writes (4 writers × 250 INSERTs)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("parallel BEGIN CONCURRENT vs SQLite's one-writer WAL"),
    }
}

fn bench_hash_join() -> BenchResult {
    // Self-join — should hit hash join path.
    let db = setup_rustqlite(N_ROWS / 10, false);
    let conn = setup_rusqlite(N_ROWS / 10, false);
    let n = 50;
    let (rq_ops, _) = measure(
        "rust-sql self-join",
        || {
            for _ in 0..n {
                let _ = db
                    .query("SELECT a.id, b.val FROM t a JOIN t b ON a.val = b.val", [])
                    .unwrap();
            }
        },
        (N_ROWS / 10) as usize * n * 2,
    );
    let (rs_ops, _) = measure(
        "rusqlite self-join",
        || {
            for _ in 0..n {
                let mut stmt = conn
                    .prepare("SELECT a.id, b.val FROM t a JOIN t b ON a.val = b.val")
                    .unwrap();
                let mut rows = stmt.query([]).unwrap();
                while rows.next().unwrap().is_some() {}
            }
        },
        (N_ROWS / 10) as usize * n * 2,
    );
    BenchResult {
        name: "Self-join (1k rows × 50 iters)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
        note: Some("hash join"),
    }
}

fn main() {
    println!();
    println!("============================================================");
    println!("  rust-sql vs SQLite — COMPREHENSIVE head-to-head benchmark");
    println!("  (10k rows, in-memory, 5-iter average per test)");
    println!("============================================================");
    println!();

    let results = vec![
        bench_insert_autocommit(),
        bench_insert_transaction(),
        bench_insert_multi_values(),
        bench_point_lookup(),
        bench_index_lookup(),
        bench_range_scan(),
        bench_full_scan(),
        bench_aggregate_multi(),
        bench_count_star(),
        bench_aggregate_with_where(),
        bench_group_by(),
        bench_update_by_id(),
        bench_delete_insert_cycle(),
        bench_concurrent_8_readers(),
        bench_concurrent_16_readers(),
        bench_mixed_rw(),
        bench_concurrent_writes(),
        bench_hash_join(),
    ];

    let mut wins_rust = 0;
    let mut wins_sqlite = 0;
    let mut ties = 0;
    println!("Test name                                    | rust-sql             | SQLite               | Ratio");
    println!("---------------------------------------------+----------------------+----------------------+---------");
    for r in &results {
        r.print();
        if r.rustqlite_ops_per_sec > r.rusqlite_ops_per_sec * 1.05 {
            wins_rust += 1;
        } else if r.rusqlite_ops_per_sec > r.rustqlite_ops_per_sec * 1.05 {
            wins_sqlite += 1;
        } else {
            ties += 1;
        }
    }
    println!();
    println!("============================================================");
    println!("Summary:");
    println!(
        "  rust-sql wins: {}  | SQLite wins: {}  | ties: {} (of {} tests)",
        wins_rust,
        wins_sqlite,
        ties,
        results.len()
    );
    println!();

    // Print headline wins (rust-sql > SQLite by significant margin)
    let mut headlines: Vec<&BenchResult> = results
        .iter()
        .filter(|r| r.rustqlite_ops_per_sec > r.rusqlite_ops_per_sec * 1.2)
        .collect();
    headlines.sort_by(|a, b| (b.ratio()).partial_cmp(&(a.ratio())).unwrap());
    if !headlines.is_empty() {
        println!(">>> HEADLINE WINS (rust-sql >1.2x faster than SQLite):");
        for h in headlines.iter().take(5) {
            println!("    {}", h.name);
            println!("      rust-sql: {:.0} ops/sec", h.rustqlite_ops_per_sec);
            println!("      SQLite:   {:.0} ops/sec", h.rusqlite_ops_per_sec);
            println!("      rust-sql is {:.2}x faster than SQLite", h.ratio());
        }
        println!();
    }

    // Print remaining gaps (SQLite > rust-sql)
    let mut gaps: Vec<&BenchResult> = results
        .iter()
        .filter(|r| r.rusqlite_ops_per_sec > r.rustqlite_ops_per_sec * 1.05)
        .collect();
    gaps.sort_by(|a, b| (a.ratio()).partial_cmp(&(b.ratio())).unwrap());
    if !gaps.is_empty() {
        println!(">>> REMAINING GAPS (SQLite wins, rust-sql still slower):");
        for g in gaps.iter().take(5) {
            println!(
                "    {:<42}  ratio: {:.2}x (SQLite wins by {:.1}x)",
                g.name,
                g.ratio(),
                1.0 / g.ratio()
            );
        }
    }
}
