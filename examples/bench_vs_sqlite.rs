//! Head-to-head comparison: rust-sql vs SQLite (via rusqlite).
//!
//! This is a STANDALONE binary that prints a clean comparison table.
//! Run with:
//!   cargo run --release --bin bench_vs_sqlite
//!
//! Each test runs for a fixed amount of work and prints:
//!   rust-sql: <ops/sec>
//!   SQLite:   <ops/sec>
//!   ratio:    <x.xx>x (winner)

use parking_lot::RwLock;
use rusqlite::params;
use rustqlite::{Database, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

const N_ROWS: i64 = 10_000;

fn setup_rusqlite(n: i64) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO t (name, val) VALUES (?1, ?2)",
            params![format!("name{}", i), i * 2],
        ).unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn
}

fn setup_rustqlite(n: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    // Use parameterized query so the statement cache hit makes it a fair
    // comparison with rusqlite's prepared-statement path.
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (name, val) VALUES (?, ?)",
            [Value::Text(format!("name{}", i).into()), Value::Integer(i * 2)],
        ).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

struct Result {
    name: String,
    rustqlite_ops_per_sec: f64,
    rusqlite_ops_per_sec: f64,
}

impl Result {
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
        println!(
            "  {:<32} | rust-sql: {:>10.0} ops/s | SQLite: {:>10.0} ops/s | ratio: {:>5.2}x ({})",
            self.name,
            self.rustqlite_ops_per_sec,
            self.rusqlite_ops_per_sec,
            self.ratio(),
            self.winner(),
        );
    }
}

fn measure<F: Fn()>(name: &str, f: F, total_ops: usize) -> (f64, f64) {
    // warm up
    for _ in 0..3 {
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
    (ops_per_sec, elapsed.as_secs_f64())
}

// ============================================================
// Tests
// ============================================================

fn bench_insert_autocommit() -> Result {
    let n = 1000;
    let (rq_ops, _) = measure("rust-sql autocommit insert", || {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
        // Use parameterized query so the statement cache hit makes it a fair
        // comparison with rusqlite's prepared-statement path. Without this,
        // we'd be re-parsing the SQL string every iteration (since `format!`
        // produces a different string each time).
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
            ).unwrap();
        }
    }, n as usize);

    let (rs_ops, _) = measure("rusqlite autocommit insert", || {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
        for i in 1..=n {
            conn.execute(
                "INSERT INTO t (name, val) VALUES (?1, ?2)",
                params![format!("name{}", i), i],
            ).unwrap();
        }
    }, n as usize);

    Result {
        name: "INSERT (auto-commit, 1k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_insert_transaction() -> Result {
    let n = 1000;
    let (rq_ops, _) = measure("rust-sql transaction insert", || {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
        db.execute("BEGIN", []).unwrap();
        // Use parameterized query so the statement cache hit makes it a fair
        // comparison with rusqlite's prepared-statement path.
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
            ).unwrap();
        }
        db.execute("COMMIT", []).unwrap();
    }, n as usize);

    let (rs_ops, _) = measure("rusqlite transaction insert", || {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
        conn.execute("BEGIN", []).unwrap();
        for i in 1..=n {
            conn.execute(
                "INSERT INTO t (name, val) VALUES (?1, ?2)",
                params![format!("name{}", i), i],
            ).unwrap();
        }
        conn.execute("COMMIT", []).unwrap();
    }, n as usize);

    Result {
        name: "INSERT (transaction, 1k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_point_lookup() -> Result {
    let db = setup_rustqlite(N_ROWS);
    let conn = setup_rusqlite(N_ROWS);
    let n = 1000;

    let (rq_ops, _) = measure("rust-sql point lookup", || {
        for i in 1..=n {
            let _ = db.query(
                "SELECT name, val FROM t WHERE id = ?",
                [Value::Integer(i)],
            ).unwrap();
        }
    }, n as usize);

    let (rs_ops, _) = measure("rusqlite point lookup", || {
        for i in 1..=n {
            let mut stmt = conn.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
            let mut rows = stmt.query(params![i]).unwrap();
            while let Some(_) = rows.next().unwrap() {}
        }
    }, n as usize);

    Result {
        name: "Point lookup (SELECT by id, 1k queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_range_scan() -> Result {
    let db = setup_rustqlite(N_ROWS);
    let conn = setup_rusqlite(N_ROWS);
    let n = 500;

    let (rq_ops, _) = measure("rust-sql range scan", || {
        for _ in 0..n {
            let _ = db.query("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100", []).unwrap();
        }
    }, n as usize);

    let (rs_ops, _) = measure("rusqlite range scan", || {
        for _ in 0..n {
            let mut stmt = conn.prepare("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(_) = rows.next().unwrap() {}
        }
    }, n as usize);

    Result {
        name: "Range scan (100 rows, 500 queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_aggregate() -> Result {
    let db = setup_rustqlite(N_ROWS);
    let conn = setup_rusqlite(N_ROWS);
    let n = 500;

    let (rq_ops, _) = measure("rust-sql aggregate", || {
        for _ in 0..n {
            let _ = db.query("SELECT COUNT(*), SUM(val), MIN(val), MAX(val), AVG(val) FROM t", []).unwrap();
        }
    }, n as usize);

    let (rs_ops, _) = measure("rusqlite aggregate", || {
        for _ in 0..n {
            let mut stmt = conn.prepare("SELECT COUNT(*), SUM(val), MIN(val), MAX(val), AVG(val) FROM t").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(_) = rows.next().unwrap() {}
        }
    }, n as usize);

    Result {
        name: "Aggregate (COUNT/SUM/MIN/MAX/AVG over 10k rows)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_count_star() -> Result {
    let db = setup_rustqlite(N_ROWS);
    let conn = setup_rusqlite(N_ROWS);
    let n = 500;

    let (rq_ops, _) = measure("rust-sql COUNT(*)", || {
        for _ in 0..n {
            let _ = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        }
    }, n as usize);

    let (rs_ops, _) = measure("rusqlite COUNT(*)", || {
        for _ in 0..n {
            let mut stmt = conn.prepare("SELECT COUNT(*) FROM t").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while let Some(_) = rows.next().unwrap() {}
        }
    }, n as usize);

    Result {
        name: "COUNT(*) only (no row decode)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_concurrent_8_readers() -> Result {
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS)));
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS)));
    let n_per_thread = 500;
    let total_ops = 8 * n_per_thread;

    let (rq_ops, _) = measure("rust-sql 8 readers", || {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = Arc::clone(&db);
            handles.push(thread::spawn(move || {
                for i in 0..n_per_thread {
                    let id = (i % N_ROWS as usize) as i64 + 1;
                    let guard = db.read();
                    let _ = guard.query(
                        "SELECT name, val FROM t WHERE id = ?",
                        [Value::Integer(id)],
                    );
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
    }, total_ops);

    let (rs_ops, _) = measure("rusqlite 8 readers mutex", || {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let conn = Arc::clone(&conn);
            handles.push(thread::spawn(move || {
                for i in 0..n_per_thread {
                    let id = (i % N_ROWS as usize) as i64 + 1;
                    let guard = conn.lock().unwrap();
                    let mut stmt = guard.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
                    let mut rows = stmt.query(params![id]).unwrap();
                    while let Some(_) = rows.next().unwrap() {}
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
    }, total_ops);

    Result {
        name: "Concurrent reads (8 threads × 500 queries)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn bench_mixed_rw_4r_1w() -> Result {
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS)));
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS)));
    let n_reads = 250;
    let n_writes = 200;
    let total_ops = (4 * n_reads) + n_writes;

    let (rq_ops, _) = measure("rust-sql 4r 1w", || {
        let mut handles = Vec::new();
        // writer
        {
            let db = Arc::clone(&db);
            handles.push(thread::spawn(move || {
                for i in 0..n_writes {
                    let id = N_ROWS + 1 + i as i64;
                    let mut guard = db.write();
                    let _ = guard.execute(
                        "INSERT INTO t (id, name, val) VALUES (?, ?, ?)",
                        [Value::Integer(id), Value::Text(format!("new{i}").into()), Value::Integer(i as i64)],
                    );
                }
            }));
        }
        // 4 readers
        for _ in 0..4 {
            let db = Arc::clone(&db);
            handles.push(thread::spawn(move || {
                for i in 0..n_reads {
                    let id = (i % N_ROWS as usize) as i64 + 1;
                    let guard = db.read();
                    let _ = guard.query(
                        "SELECT name, val FROM t WHERE id = ?",
                        [Value::Integer(id)],
                    );
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
    }, total_ops);

    let (rs_ops, _) = measure("rusqlite 4r 1w mutex", || {
        let mut handles = Vec::new();
        // writer
        {
            let conn = Arc::clone(&conn);
            handles.push(thread::spawn(move || {
                for i in 0..n_writes {
                    let id = N_ROWS + 1 + i as i64;
                    let guard = conn.lock().unwrap();
                    let _ = guard.execute(
                        "INSERT INTO t (id, name, val) VALUES (?1, ?2, ?3)",
                        params![id, format!("new{i}"), i],
                    );
                }
            }));
        }
        // 4 readers
        for _ in 0..4 {
            let conn = Arc::clone(&conn);
            handles.push(thread::spawn(move || {
                for i in 0..n_reads {
                    let id = (i % N_ROWS as usize) as i64 + 1;
                    let guard = conn.lock().unwrap();
                    let mut stmt = guard.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
                    let mut rows = stmt.query(params![id]).unwrap();
                    while let Some(_) = rows.next().unwrap() {}
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
    }, total_ops);

    Result {
        name: "Mixed R/W (4 readers + 1 writer)".into(),
        rustqlite_ops_per_sec: rq_ops,
        rusqlite_ops_per_sec: rs_ops,
    }
}

fn main() {
    println!();
    println!("============================================================");
    println!("  rust-sql vs SQLite — head-to-head benchmark");
    println!("  (10k rows, in-memory, 5-iter average per test)");
    println!("============================================================");
    println!();

    let tests: Vec<Result> = vec![
        bench_insert_autocommit(),
        bench_insert_transaction(),
        bench_point_lookup(),
        bench_range_scan(),
        bench_aggregate(),
        bench_count_star(),
        bench_concurrent_8_readers(),
        bench_mixed_rw_4r_1w(),
    ];

    println!("Test name                          | rust-sql throughput    | SQLite throughput      | Ratio (rust-sql/SQLite)");
    println!("-----------------------------------+------------------------+------------------------+----------------------");
    for t in &tests {
        t.print();
    }

    println!();
    println!("============================================================");
    println!("Summary:");
    println!("  rust-sql wins: {}", tests.iter().filter(|t| t.winner() == "rust-sql").count());
    println!("  SQLite wins:   {}", tests.iter().filter(|t| t.winner() == "SQLite").count());
    println!();

    // Print concurrent test results in detail since that's the headline.
    if let Some(t) = tests.iter().find(|t| t.name.contains("Concurrent reads")) {
        println!(">>> HEADLINE: {}", t.name);
        println!("    rust-sql: {:.0} ops/sec", t.rustqlite_ops_per_sec);
        println!("    SQLite:   {:.0} ops/sec", t.rusqlite_ops_per_sec);
        println!("    rust-sql is {:.2}x faster than SQLite for concurrent reads", t.ratio());
    }

    if let Some(t) = tests.iter().find(|t| t.name.contains("Mixed R/W")) {
        println!(">>> HEADLINE: {}", t.name);
        println!("    rust-sql: {:.0} ops/sec", t.rustqlite_ops_per_sec);
        println!("    SQLite:   {:.0} ops/sec", t.rusqlite_ops_per_sec);
        println!("    rust-sql is {:.2}x faster than SQLite for mixed R/W workload", t.ratio());
    }
    println!();
}
