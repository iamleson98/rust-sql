//! Head-to-head benchmark: rust-sql vs SQLite (via rusqlite).
//!
//! This benchmark covers all the dimensions where we want to BEAT SQLite:
//! 1. Single-threaded throughput: insert, point lookup, range scan, join, aggregate.
//! 2. Multi-threaded concurrency: N readers + M writers concurrently.
//! 3. Latency: per-statement cost.
//!
//! Run with:
//!   cargo bench --bench sqlite_comparison --features ...
//!
//! Output is printed to stdout with a final summary table.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use rusqlite::params;
use rustqlite::{Database, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;
use parking_lot::RwLock;

// ============================================================
// Setup helpers
// ============================================================

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
    for i in 1..=n {
        let sql = format!("INSERT INTO t (name, val) VALUES ('name{}', {})", i, i * 2);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn setup_rusqlite_join(left: i64, right: i64) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    conn.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, y INTEGER)", []).unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=left {
        conn.execute("INSERT INTO a (x) VALUES (?1)", params![i]).unwrap();
    }
    for i in 1..=right {
        conn.execute("INSERT INTO b (a_id, y) VALUES (?1, ?2)", params![(i % left) + 1, i]).unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn
}

fn setup_rustqlite_join(left: i64, right: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, y INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=left {
        let sql = format!("INSERT INTO a (x) VALUES ({})", i);
        db.execute(&sql, []).unwrap();
    }
    for i in 1..=right {
        let sql = format!("INSERT INTO b (a_id, y) VALUES ({}, {})", (i % left) + 1, i);
        db.execute(&sql, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    db
}

// ============================================================
// 1. INSERT throughput (auto-commit + transactional)
// ============================================================

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    group.throughput(Throughput::Elements(1));

    // rust-sql: auto-commit insert
    group.bench_function("rustqlite_autocommit", |b| {
        b.iter_with_setup(
            || {
                let mut db = Database::open_in_memory().unwrap();
                db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
                db
            },
            |mut db| {
                for i in 1..=1000 {
                    let sql = format!("INSERT INTO t (name, val) VALUES ('name{}', {})", i, i);
                    db.execute(&sql, []).unwrap();
                }
            },
        )
    });

    // rust-sql: transactional insert
    group.bench_function("rustqlite_transaction", |b| {
        b.iter_with_setup(
            || {
                let mut db = Database::open_in_memory().unwrap();
                db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
                db
            },
            |mut db| {
                db.execute("BEGIN", []).unwrap();
                for i in 1..=1000 {
                    let sql = format!("INSERT INTO t (name, val) VALUES ('name{}', {})", i, i);
                    db.execute(&sql, []).unwrap();
                }
                db.execute("COMMIT", []).unwrap();
            },
        )
    });

    // rusqlite: auto-commit insert
    group.bench_function("rusqlite_autocommit", |b| {
        b.iter_with_setup(
            || {
                let conn = rusqlite::Connection::open_in_memory().unwrap();
                conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
                conn
            },
            |conn| {
                for i in 1..=1000 {
                    conn.execute(
                        "INSERT INTO t (name, val) VALUES (?1, ?2)",
                        params![format!("name{}", i), i],
                    ).unwrap();
                }
            },
        )
    });

    // rusqlite: transactional insert
    group.bench_function("rusqlite_transaction", |b| {
        b.iter_with_setup(
            || {
                let conn = rusqlite::Connection::open_in_memory().unwrap();
                conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
                conn
            },
            |conn| {
                conn.execute("BEGIN", []).unwrap();
                for i in 1..=1000 {
                    conn.execute(
                        "INSERT INTO t (name, val) VALUES (?1, ?2)",
                        params![format!("name{}", i), i],
                    ).unwrap();
                }
                conn.execute("COMMIT", []).unwrap();
            },
        )
    });

    group.finish();
}

// ============================================================
// 2. Point lookup (SELECT by id)
// ============================================================

fn bench_point_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("point_lookup");
    group.throughput(Throughput::Elements(1));

    let db = setup_rustqlite(N_ROWS);
    group.bench_function("rustqlite", |b| {
        b.iter(|| {
            let rows = db.query(
                "SELECT name, val FROM t WHERE id = 500",
                [],
            ).unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite(N_ROWS);
    group.bench_function("rusqlite_prepared", |b| {
        b.iter(|| {
            let mut stmt = conn.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
            let mut rows = stmt.query(params![black_box(500)]).unwrap();
            while rows.next().unwrap().is_some() {}
        })
    });

    group.finish();
}

// ============================================================
// 3. Range scan (SELECT by range of ids)
// ============================================================

fn bench_range_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("range_scan");
    group.throughput(Throughput::Elements(100));

    let db = setup_rustqlite(N_ROWS);
    group.bench_function("rustqlite_100_rows", |b| {
        b.iter(|| {
            let rows = db.query(
                "SELECT name, val FROM t WHERE id BETWEEN 1 AND 100",
                [],
            ).unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite(N_ROWS);
    group.bench_function("rusqlite_100_rows", |b| {
        b.iter(|| {
            let mut stmt = conn.prepare("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while rows.next().unwrap().is_some() {}
        })
    });

    group.finish();
}

// ============================================================
// 4. Aggregate (COUNT/SUM/MIN/MAX)
// ============================================================

fn bench_aggregate(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate");
    group.throughput(Throughput::Elements(1));

    let db = setup_rustqlite(N_ROWS);
    group.bench_function("rustqlite_count_star", |b| {
        b.iter(|| {
            let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite(N_ROWS);
    group.bench_function("rusqlite_count_star", |b| {
        b.iter(|| {
            let mut stmt = conn.prepare("SELECT COUNT(*) FROM t").unwrap();
            let mut rows = stmt.query([]).unwrap();
            while rows.next().unwrap().is_some() {}
        })
    });

    group.finish();
}

// ============================================================
// 5. Hash join / nested-loop join
// ============================================================

fn bench_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("join");
    group.throughput(Throughput::Elements(1000));

    let db = setup_rustqlite_join(1000, 1000);
    group.bench_function("rustqlite_inner_join", |b| {
        b.iter(|| {
            let rows = db.query(
                "SELECT a.id, a.x, b.id, b.y FROM a INNER JOIN b ON a.id = b.a_id LIMIT 1000",
                [],
            ).unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite_join(1000, 1000);
    group.bench_function("rusqlite_inner_join", |b| {
        b.iter(|| {
            let mut stmt = conn.prepare(
                "SELECT a.id, a.x, b.id, b.y FROM a INNER JOIN b ON a.id = b.a_id LIMIT 1000"
            ).unwrap();
            let mut rows = stmt.query([]).unwrap();
            while rows.next().unwrap().is_some() {}
        })
    });

    group.finish();
}

// ============================================================
// 6. CONCURRENT throughput: N readers + M writers — THE BIG ONE
// ============================================================
//
// This is where the interior-mutability refactor on `Pager` + `Database`
// should let us BEAT SQLite, which uses a single-writer mutex for the
// whole connection. We share `Arc<RwLock<Database>>` across threads,
// readers take read locks, writers take write locks. SQLite uses
// `Arc<Mutex<Connection>>` because each rusqlite Connection is `!Sync`
// (the underlying SQLite handle is `Send + Sync` per-connection, but
// the rusqlite wrapper requires a Mutex for shared access).

fn bench_concurrent_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_throughput");
    group.throughput(Throughput::Elements(1));
    group.sample_size(10);

    // rust-sql: 8 readers concurrent, 500 queries each.
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS)));
    group.bench_function("rustqlite_8_readers", |b| {
        b.iter_custom(|iters| {
            let total_queries = Arc::new(AtomicUsize::new(0));
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();
                for _ in 0..8 {
                    let db = Arc::clone(&db);
                    let total = Arc::clone(&total_queries);
                    handles.push(thread::spawn(move || {
                        for i in 0..500usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            let guard = db.read();
                            let _ = guard.query(
                                "SELECT name, val FROM t WHERE id = ?",
                                [Value::Integer(id)],
                            );
                            drop(guard);
                            total.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
            }
            let elapsed = start.elapsed();
            let _total = total_queries.load(Ordering::SeqCst);
            elapsed
        })
    });

    // rusqlite: 8 readers concurrent, 500 queries each.
    // Uses Arc<Mutex<Connection>> because rusqlite::Connection is !Sync.
    // This is what most production Rust apps do today for SQLite concurrency.
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS)));
    group.bench_function("rusqlite_8_readers_mutex", |b| {
        b.iter_custom(|iters| {
            let total_queries = Arc::new(AtomicUsize::new(0));
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();
                for _ in 0..8 {
                    let conn = Arc::clone(&conn);
                    let total = Arc::clone(&total_queries);
                    handles.push(thread::spawn(move || {
                        for i in 0..500usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            {
                                let guard = conn.lock().unwrap();
                                let mut stmt = guard.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
                                let mut rows = stmt.query(params![id]).unwrap();
                                while rows.next().unwrap().is_some() {}
                            }
                            total.fetch_add(1, Ordering::Relaxed);
                        }
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
            }
            let elapsed = start.elapsed();
            let _total = total_queries.load(Ordering::SeqCst);
            elapsed
        })
    });

    group.finish();
}

// ============================================================
// 7. Mixed read/write concurrency (the killer test)
// ============================================================

fn bench_mixed_rw(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_rw_concurrency");
    group.throughput(Throughput::Elements(1));
    group.sample_size(10);

    // rust-sql: 4 readers + 1 writer, fixed work (1000 queries per reader + 200 inserts).
    let db = Arc::new(RwLock::new(setup_rustqlite(N_ROWS)));
    group.bench_function("rustqlite_4r_1w", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();

                // Writer thread: 200 inserts in a distinct id range.
                {
                    let db = Arc::clone(&db);
                    handles.push(thread::spawn(move || {
                        for i in 0..200 {
                            let id = N_ROWS + 1 + i;
                            let mut guard = db.write();
                            let _ = guard.execute(
                                "INSERT INTO t (id, name, val) VALUES (?, ?, ?)",
                                [Value::Integer(id), Value::Text(format!("new{i}").into()), Value::Integer(i)],
                            );
                            drop(guard);
                        }
                    }));
                }

                // 4 reader threads × 250 queries each.
                for _ in 0..4 {
                    let db = Arc::clone(&db);
                    handles.push(thread::spawn(move || {
                        for i in 0..250usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            let guard = db.read();
                            let _ = guard.query(
                                "SELECT name, val FROM t WHERE id = ?",
                                [Value::Integer(id)],
                            );
                            drop(guard);
                        }
                    }));
                }

                for h in handles {
                    h.join().unwrap();
                }
            }
            start.elapsed()
        })
    });

    // rusqlite: 4 readers + 1 writer, same workload, serialized via Mutex.
    let conn = Arc::new(std::sync::Mutex::new(setup_rusqlite(N_ROWS)));
    group.bench_function("rusqlite_4r_1w_mutex", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();

                // Writer thread: 200 inserts.
                {
                    let conn = Arc::clone(&conn);
                    handles.push(thread::spawn(move || {
                        for i in 0..200 {
                            let id = N_ROWS + 1 + i;
                            let guard = conn.lock().unwrap();
                            let _ = guard.execute(
                                "INSERT INTO t (id, name, val) VALUES (?1, ?2, ?3)",
                                params![id, format!("new{i}"), i],
                            );
                            drop(guard);
                        }
                    }));
                }

                // 4 reader threads × 250 queries each.
                for _ in 0..4 {
                    let conn = Arc::clone(&conn);
                    handles.push(thread::spawn(move || {
                        for i in 0..250usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            {
                                let guard = conn.lock().unwrap();
                                let mut stmt = guard.prepare("SELECT name, val FROM t WHERE id = ?1").unwrap();
                                let mut rows = stmt.query(params![id]).unwrap();
                                while rows.next().unwrap().is_some() {}
                            }
                        }
                    }));
                }

                for h in handles {
                    h.join().unwrap();
                }
            }
            start.elapsed()
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_point_lookup,
    bench_range_scan,
    bench_aggregate,
    bench_join,
    bench_concurrent_throughput,
    bench_mixed_rw,
);
criterion_main!(benches);
