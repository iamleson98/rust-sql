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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

// ============================================================
// Setup helpers
// ============================================================

const N_ROWS: i64 = 10_000;

fn setup_rusqlite(n: i64) -> rusqlite::Connection {
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
            params![format!("name{}", i), i * 2],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn
}

fn setup_rustqlite(n: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
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
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    conn.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    conn.execute("BEGIN", []).unwrap();
    for i in 1..=left {
        conn.execute("INSERT INTO a (x) VALUES (?1)", params![i])
            .unwrap();
    }
    for i in 1..=right {
        conn.execute(
            "INSERT INTO b (a_id, y) VALUES (?1, ?2)",
            params![(i % left) + 1, i],
        )
        .unwrap();
    }
    conn.execute("COMMIT", []).unwrap();
    conn
}

fn setup_rustqlite_join(left: i64, right: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
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
                db.execute(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                    [],
                )
                .unwrap();
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
                db.execute(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                    [],
                )
                .unwrap();
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
                conn.execute(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                    [],
                )
                .unwrap();
                conn
            },
            |conn| {
                for i in 1..=1000 {
                    conn.execute(
                        "INSERT INTO t (name, val) VALUES (?1, ?2)",
                        params![format!("name{}", i), i],
                    )
                    .unwrap();
                }
            },
        )
    });

    // rusqlite: transactional insert
    group.bench_function("rusqlite_transaction", |b| {
        b.iter_with_setup(
            || {
                let conn = rusqlite::Connection::open_in_memory().unwrap();
                conn.execute(
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
                    [],
                )
                .unwrap();
                conn
            },
            |conn| {
                conn.execute("BEGIN", []).unwrap();
                for i in 1..=1000 {
                    conn.execute(
                        "INSERT INTO t (name, val) VALUES (?1, ?2)",
                        params![format!("name{}", i), i],
                    )
                    .unwrap();
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
            let rows = db
                .query("SELECT name, val FROM t WHERE id = 500", [])
                .unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite(N_ROWS);
    group.bench_function("rusqlite_prepared", |b| {
        b.iter(|| {
            let mut stmt = conn
                .prepare("SELECT name, val FROM t WHERE id = ?1")
                .unwrap();
            let mut rows = stmt.query(params![black_box(500)]).unwrap();
            // Fair-work parity: read the projected columns — rustqlite's
            // materializing `query()` decodes them.
            while let Some(row) = rows.next().unwrap() {
                let _name: String = row.get(0).unwrap();
                let _val: i64 = row.get(1).unwrap();
            }
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
            let rows = db
                .query("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100", [])
                .unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite(N_ROWS);
    group.bench_function("rusqlite_100_rows", |b| {
        b.iter(|| {
            let mut stmt = conn
                .prepare("SELECT name, val FROM t WHERE id BETWEEN 1 AND 100")
                .unwrap();
            let mut rows = stmt.query([]).unwrap();
            // Fair-work parity: read the projected columns (see
            // rusqlite_prepared).
            while let Some(row) = rows.next().unwrap() {
                let _name: String = row.get(0).unwrap();
                let _val: i64 = row.get(1).unwrap();
            }
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
            let rows = db
                .query(
                    "SELECT a.id, a.x, b.id, b.y FROM a INNER JOIN b ON a.id = b.a_id LIMIT 1000",
                    [],
                )
                .unwrap();
            let _ = black_box(rows);
        })
    });

    let conn = setup_rusqlite_join(1000, 1000);
    group.bench_function("rusqlite_inner_join", |b| {
        b.iter(|| {
            let mut stmt = conn
                .prepare(
                    "SELECT a.id, a.x, b.id, b.y FROM a INNER JOIN b ON a.id = b.a_id LIMIT 1000",
                )
                .unwrap();
            let mut rows = stmt.query([]).unwrap();
            // Fair-work parity: rustqlite's `query()` materializes all
            // 1000 projected rows (4 values each). Draining `rows.next()`
            // without reading columns measures only SQLite's VDBE walk.
            let mut acc: i64 = 0;
            while let Some(row) = rows.next().unwrap() {
                acc = acc.wrapping_add(row.get::<_, i64>(0).unwrap());
                acc = acc.wrapping_add(row.get::<_, i64>(1).unwrap());
                acc = acc.wrapping_add(row.get::<_, i64>(2).unwrap());
                acc = acc.wrapping_add(row.get::<_, i64>(3).unwrap());
            }
            black_box(acc);
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
    // FAIRNESS (2026-10 audit): both engines get their best in-process
    // multi-reader shape on file-backed WAL databases with identical data:
    //   rustqlite — one shared `Arc<Database>`; each thread prepares its own
    //     `&self` statement and reads in PARALLEL (the MRMW engine core).
    //   SQLite — 8 REAL connections on one WAL database (busy_timeout,
    //     per-connection prepared statements): SQLite's best multi-reader
    //     shape (WAL readers run concurrently). The old
    //     `Arc<Mutex<Connection>>` shape measured a wrapper lock, not the
    //     engine, and is retired.
    let mut group = c.benchmark_group("concurrent_throughput");
    group.throughput(Throughput::Elements(1));
    group.sample_size(10);

    let tmp = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let rq_path = tmp.join(format!("crit-rq8-{uniq}.db"));
    let sq_path = tmp.join(format!("crit-sq8-{uniq}.db"));
    let rq_path = rq_path.to_string_lossy().into_owned();
    let sq_path = sq_path.to_string_lossy().into_owned();

    // Build the two identical WAL databases.
    let mut rq_db = Database::open(&rq_path).unwrap();
    rq_db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    rq_db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    rq_db
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
    rq_db.execute("BEGIN", []).unwrap();
    for i in 1..=N_ROWS {
        rq_db
            .execute(
                "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
                [
                    Value::Text(format!("name{}", i).into()),
                    Value::Integer(i),
                    Value::Real(i as f64 * 1.5),
                ],
            )
            .unwrap();
    }
    rq_db.execute("COMMIT", []).unwrap();
    // ANSWER EQUALITY before timing.
    {
        let sq_setup = rusqlite::Connection::open(&sq_path).unwrap();
        sq_setup
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .unwrap();
        sq_setup
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
                [],
            )
            .unwrap();
        sq_setup.execute("BEGIN", []).unwrap();
        for i in 1..=N_ROWS {
            sq_setup
                .execute(
                    "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
                    params![format!("name{}", i), i, i as f64 * 1.5],
                )
                .unwrap();
        }
        sq_setup.execute("COMMIT", []).unwrap();
        let sq: (i64, i64) = sq_setup
            .query_row("SELECT COUNT(*), SUM(val) FROM t", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let rq = rq_db.query("SELECT COUNT(*), SUM(val) FROM t", []).unwrap();
        assert_eq!(
            rq[0][0].as_integer(),
            sq.0,
            "criterion reader row count mismatch"
        );
        assert_eq!(rq[0][1].as_integer(), sq.1, "criterion reader SUM mismatch");
    }

    // rust-sql: 8 readers concurrent, 500 queries each (shared Arc<Database>).
    let db = Arc::new(rq_db);
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
                        let mut stmt = db.prepare("SELECT name, val FROM t WHERE id = ?").unwrap();
                        for i in 0..500usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            stmt.bind(1, Value::Integer(id)).unwrap();
                            while let rustqlite::StepResult::Row = stmt.step().unwrap() {
                                let _ = stmt.column_text(0);
                                let _ = stmt.column_int(1);
                            }
                            stmt.reset();
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
    drop(db);

    // rusqlite: 8 REAL connections on one WAL database, 500 queries each.
    group.bench_function("rusqlite_8_readers_wal", |b| {
        b.iter_custom(|iters| {
            let total_queries = Arc::new(AtomicUsize::new(0));
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();
                for _ in 0..8 {
                    let path = sq_path.clone();
                    let total = Arc::clone(&total_queries);
                    handles.push(thread::spawn(move || {
                        let conn = rusqlite::Connection::open(&path).unwrap();
                        conn.busy_timeout(std::time::Duration::from_secs(30))
                            .unwrap();
                        let mut stmt = conn
                            .prepare("SELECT name, val FROM t WHERE id = ?1")
                            .unwrap();
                        for i in 0..500usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            let mut rows = stmt.query(params![id]).unwrap();
                            while let Some(row) = rows.next().unwrap() {
                                let _ = row.get::<_, String>(0).unwrap();
                                let _ = row.get::<_, i64>(1).unwrap();
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

    // Cleanup both WAL databases and their sidecars.
    for path in [&rq_path, &sq_path] {
        let _ = std::fs::remove_file(path);
        let p = std::path::Path::new(path);
        let stem = p.file_name().unwrap().to_string_lossy().into_owned();
        if let Some(dir) = p.parent() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(&stem) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
    }

    group.finish();
}

// ============================================================
// 7. Mixed read/write concurrency (the killer test)
// ============================================================
// FAIRNESS (2026-10 audit): both engines get their best in-process shape
// on file-backed WAL databases with identical data — the engine's shared
// `Arc<Database>` (parallel prepared-statement readers + a concurrent-
// regime writer) against SQLite's 5 real connections on one WAL database
// (busy_timeout, per-connection prepared statements). The old
// `Arc<Mutex<Connection>>` shape serialized all five SQLite threads on a
// wrapper lock; it measured the harness, not the engine. The writer uses
// INSERT OR REPLACE on BOTH sides so every timed iteration performs real
// work (the old harness's plain INSERTs all failed with PK conflicts
// after the first iteration, on both engines equally — swallowed errors).
fn bench_mixed_rw(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_rw_concurrency");
    group.throughput(Throughput::Elements(1));
    group.sample_size(10);

    let tmp = std::env::temp_dir();
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let rq_path = tmp.join(format!("crit-rmw-{uniq}.db"));
    let sq_path = tmp.join(format!("crit-smw-{uniq}.db"));
    let rq_path = rq_path.to_string_lossy().into_owned();
    let sq_path = sq_path.to_string_lossy().into_owned();

    // Engine side: one shared WAL database.
    let mut rq_db = Database::open(&rq_path).unwrap();
    rq_db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    rq_db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    rq_db
        .execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
    rq_db.execute("BEGIN", []).unwrap();
    for i in 1..=N_ROWS {
        rq_db
            .execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [Value::Text(format!("name{}", i).into()), Value::Integer(i)],
            )
            .unwrap();
    }
    rq_db.execute("COMMIT", []).unwrap();

    // SQLite side: the same database shape, built once.
    {
        let sq = rusqlite::Connection::open(&sq_path).unwrap();
        sq.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .unwrap();
        sq.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        sq.execute("BEGIN", []).unwrap();
        for i in 1..=N_ROWS {
            sq.execute(
                "INSERT INTO t (name, val) VALUES (?1, ?2)",
                params![format!("name{}", i), i],
            )
            .unwrap();
        }
        sq.execute("COMMIT", []).unwrap();
        // ANSWER EQUALITY before timing.
        let s: (i64, i64) = sq
            .query_row("SELECT COUNT(*), SUM(val) FROM t", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let r = rq_db.query("SELECT COUNT(*), SUM(val) FROM t", []).unwrap();
        assert_eq!(r[0][0].as_integer(), s.0, "mixed_rw count mismatch");
        assert_eq!(r[0][1].as_integer(), s.1, "mixed_rw SUM mismatch");
    }

    // rust-sql: 4 parallel readers + 1 writer on ONE shared engine.
    let db = Arc::new(rq_db);
    group.bench_function("rustqlite_4r_1w", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();
                // Writer: 200 upserts through a prepared &self statement.
                {
                    let db = Arc::clone(&db);
                    handles.push(thread::spawn(move || {
                        let mut stmt = db
                            .prepare("INSERT OR REPLACE INTO t (id, name, val) VALUES (?, ?, ?)")
                            .unwrap();
                        for i in 0..200 {
                            let id = N_ROWS + 1 + i;
                            stmt.bind(1, Value::Integer(id)).unwrap();
                            stmt.bind(2, Value::Text(format!("new{i}").into())).unwrap();
                            stmt.bind(3, Value::Integer(i)).unwrap();
                            stmt.step().unwrap();
                            stmt.reset();
                        }
                    }));
                }
                // 4 readers: parallel prepared statements.
                for _ in 0..4 {
                    let db = Arc::clone(&db);
                    handles.push(thread::spawn(move || {
                        let mut stmt = db.prepare("SELECT name, val FROM t WHERE id = ?").unwrap();
                        for i in 0..250usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            stmt.bind(1, Value::Integer(id)).unwrap();
                            while let rustqlite::StepResult::Row = stmt.step().unwrap() {
                                let _ = stmt.column_text(0);
                                let _ = stmt.column_int(1);
                            }
                            stmt.reset();
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
    drop(db);

    // rusqlite: 5 real connections (1 writer + 4 readers) on one WAL file.
    group.bench_function("rusqlite_4r_1w_wal", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut handles = Vec::new();
                // Writer connection.
                {
                    let path = sq_path.clone();
                    handles.push(thread::spawn(move || {
                        let conn = rusqlite::Connection::open(&path).unwrap();
                        conn.busy_timeout(std::time::Duration::from_secs(30))
                            .unwrap();
                        let mut stmt = conn
                            .prepare("INSERT OR REPLACE INTO t (id, name, val) VALUES (?1, ?2, ?3)")
                            .unwrap();
                        for i in 0..200 {
                            let id = N_ROWS + 1 + i;
                            stmt.execute(params![id, format!("new{i}"), i]).unwrap();
                        }
                    }));
                }
                // 4 reader connections.
                for _ in 0..4 {
                    let path = sq_path.clone();
                    handles.push(thread::spawn(move || {
                        let conn = rusqlite::Connection::open(&path).unwrap();
                        conn.busy_timeout(std::time::Duration::from_secs(30))
                            .unwrap();
                        let mut stmt = conn
                            .prepare("SELECT name, val FROM t WHERE id = ?1")
                            .unwrap();
                        for i in 0..250usize {
                            let id = (i % N_ROWS as usize) as i64 + 1;
                            let mut rows = stmt.query(params![id]).unwrap();
                            while let Some(row) = rows.next().unwrap() {
                                let _ = row.get::<_, String>(0).unwrap();
                                let _ = row.get::<_, i64>(1).unwrap();
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

    // Cleanup both WAL databases and their sidecars.
    for path in [&rq_path, &sq_path] {
        let _ = std::fs::remove_file(path);
        let p = std::path::Path::new(path);
        let stem = p.file_name().unwrap().to_string_lossy().into_owned();
        if let Some(dir) = p.parent() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(&stem) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
    }

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
