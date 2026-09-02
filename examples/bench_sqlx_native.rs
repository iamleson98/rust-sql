//! sqlx-level benchmark: rustqlite's NATIVE driver vs sqlx-sqlite (real C
//! SQLite), through the IDENTICAL sqlx API surface (same sqlx-core,
//! same `query()/query_scalar()/fetch_*` calls, same pool options).
//!
//! The only difference is the driver implementation:
//!   * sqlx-sqlite: per-connection worker thread + flume channels +
//!     FFI marshalling into C SQLite.
//!   * rustqlite native: inline execution in the async task, pure Rust.
//!
//! Run: cargo run --release --example bench_sqlx_native --features sqlx

use std::time::Instant;

use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqlitePool, RustqlitePoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

const ROWS: i64 = 5_000;
const OPS: usize = 2_000;

#[tokio::main]
async fn main() {
    println!("sqlx 0.9 driver-vs-driver benchmark (identical API, same pool options)");
    println!("  rows inserted: {ROWS}, ops per scenario: {OPS}\n");

    // -- rustqlite native driver -------------------------------------------
    let rq = RustqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(RustqliteConnectOptions::new()) // private :memory:
        .await
        .unwrap();

    // -- sqlx-sqlite (C SQLite, worker thread + FFI) ------------------------
    let sq = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new() // :memory:
                .create_if_missing(true),
        )
        .await
        .unwrap();

    let ddl = "CREATE TABLE bench (
        id INTEGER PRIMARY KEY,
        a INTEGER NOT NULL,
        b REAL NOT NULL,
        c TEXT NOT NULL
    )";
    sqlx::query(ddl).execute(&rq).await.unwrap();
    sqlx::query(ddl).execute(&sq).await.unwrap();

    macro_rules! seed {
        ($name:expr, $db:expr) => {{
            let t = Instant::now();
            let mut tx = $db.begin().await.unwrap();
            for i in 0..ROWS {
                sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                    .bind(i)
                    .bind(i as f64 * 0.5)
                    .bind(format!("name-{i}"))
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
            println!(
                "  seed ({}): {:.1} ms for {ROWS} rows",
                $name,
                t.elapsed().as_secs_f64() * 1e3
            );
        }};
    }
    seed!("rustqlite", rq);
    seed!("sqlx-sqlite", sq);
    println!();

    struct Row1 {
        rq_ms: f64,
        sq_ms: f64,
    }
    impl Row1 {
        fn ratio(&self) -> f64 {
            self.sq_ms / self.rq_ms
        }
    }
    let mut results: Vec<(&str, Row1)> = Vec::new();

    // ------------------------------------------------------------------ insert
    {
        let t = Instant::now();
        for i in 0..OPS as i64 {
            sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                .bind(i)
                .bind(i as f64)
                .bind("x")
                .execute(&rq)
                .await
                .unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        for i in 0..OPS as i64 {
            sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                .bind(i)
                .bind(i as f64)
                .bind("x")
                .execute(&sq)
                .await
                .unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("INSERT + 3 binds", Row1 { rq_ms, sq_ms }));
    }

    // ------------------------------------------------------------- point lookup
    {
        let t = Instant::now();
        for i in 1..=OPS as i64 {
            let v: String = sqlx::query_scalar("SELECT c FROM bench WHERE id = ?")
                .bind(i)
                .fetch_one(&rq)
                .await
                .unwrap();
            std::hint::black_box(v);
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        for i in 1..=OPS as i64 {
            let v: String = sqlx::query_scalar("SELECT c FROM bench WHERE id = ?")
                .bind(i)
                .fetch_one(&sq)
                .await
                .unwrap();
            std::hint::black_box(v);
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("PK point lookup", Row1 { rq_ms, sq_ms }));
    }

    // -------------------------------------------------------------- update by PK
    {
        let t = Instant::now();
        for i in 1..=OPS as i64 {
            sqlx::query("UPDATE bench SET b = ? WHERE id = ?")
                .bind(i as f64 * 1.25)
                .bind(i)
                .execute(&rq)
                .await
                .unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        for i in 1..=OPS as i64 {
            sqlx::query("UPDATE bench SET b = ? WHERE id = ?")
                .bind(i as f64 * 1.25)
                .bind(i)
                .execute(&sq)
                .await
                .unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("UPDATE by PK", Row1 { rq_ms, sq_ms }));
    }

    // ------------------------------------------------------------ full scan fetch_all
    {
        let t = Instant::now();
        for _ in 0..20 {
            let rows: Vec<(i64, f64, String)> =
                sqlx::query_as("SELECT a, b, c FROM bench WHERE a % 10 = 0")
                    .fetch_all(&rq)
                    .await
                    .unwrap();
            std::hint::black_box(rows.len());
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        for _ in 0..20 {
            let rows: Vec<(i64, f64, String)> =
                sqlx::query_as("SELECT a, b, c FROM bench WHERE a % 10 = 0")
                    .fetch_all(&sq)
                    .await
                    .unwrap();
            std::hint::black_box(rows.len());
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("filtered scan fetch_all", Row1 { rq_ms, sq_ms }));
    }

    // ----------------------------------------------------------------- GROUP BY
    {
        let t = Instant::now();
        for _ in 0..50 {
            let rows: Vec<(i64, i64, f64)> =
                sqlx::query_as("SELECT a / 100, COUNT(*), SUM(b) FROM bench GROUP BY a / 100")
                    .fetch_all(&rq)
                    .await
                    .unwrap();
            std::hint::black_box(rows.len());
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        for _ in 0..50 {
            let rows: Vec<(i64, i64, f64)> =
                sqlx::query_as("SELECT a / 100, COUNT(*), SUM(b) FROM bench GROUP BY a / 100")
                    .fetch_all(&sq)
                    .await
                    .unwrap();
            std::hint::black_box(rows.len());
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("GROUP BY agg fetch_all", Row1 { rq_ms, sq_ms }));
    }

    // ----------------------------------------------------------- transaction batch
    {
        const TXS: usize = 20;
        const PER_TX: i64 = 100;

        let t = Instant::now();
        for _ in 0..TXS {
            let mut tx = rq.begin().await.unwrap();
            for i in 0..PER_TX {
                sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                    .bind(i)
                    .bind(0.0)
                    .bind("tx")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        for _ in 0..TXS {
            let mut tx = sq.begin().await.unwrap();
            for i in 0..PER_TX {
                sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                    .bind(i)
                    .bind(0.0)
                    .bind("tx")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("txn: 100 inserts", Row1 { rq_ms, sq_ms }));
    }

    // ------------------------------------------------------- row-by-row streaming
    {
        let t = Instant::now();
        let mut n = 0usize;
        use futures_util::StreamExt;
        let mut stream = sqlx::query("SELECT id, a, b, c FROM bench").fetch(&rq);
        while let Some(row) = stream.next().await {
            let row = row.unwrap();
            let _id: i64 = row.try_get("id").unwrap();
            n += 1;
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(n);

        let t = Instant::now();
        let mut n = 0usize;
        let mut stream = sqlx::query("SELECT id, a, b, c FROM bench").fetch(&sq);
        while let Some(row) = stream.next().await {
            let row = row.unwrap();
            let _id: i64 = row.try_get("id").unwrap();
            n += 1;
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        std::hint::black_box(n);
        results.push(("stream full table", Row1 { rq_ms, sq_ms }));
    }

    // ---------------------------------------------------------- concurrent lookups
    {
        const TASKS: usize = 8;
        const PER: usize = 500;

        let t = Instant::now();
        let mut handles = Vec::new();
        for w in 0..TASKS {
            let pool = rq.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..PER {
                    let v: Option<i64> = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                        .bind(1 + ((i + w * 7) % OPS) as i64)
                        .fetch_optional(&pool)
                        .await
                        .unwrap()
                        .flatten();
                    std::hint::black_box(v);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        // sqlite: file-backed pool for 8 real connections (in-memory
        // :memory: would give each connection its OWN empty database).
        let tmp = tempfile::tempdir().unwrap();
        let sqf = SqlitePoolOptions::new()
            .max_connections(TASKS as u32)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(tmp.path().join("c.db"))
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query(ddl).execute(&sqf).await.unwrap();
        let mut tx = sqf.begin().await.unwrap();
        for i in 0..ROWS {
            sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                .bind(i)
                .bind(i as f64 * 0.5)
                .bind(format!("name-{i}"))
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let t = Instant::now();
        let mut handles = Vec::new();
        for w in 0..TASKS {
            let pool = sqf.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..PER {
                    let v: Option<i64> = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                        .bind(1 + ((i + w * 7) % OPS) as i64)
                        .fetch_optional(&pool)
                        .await
                        .unwrap()
                        .flatten();
                    std::hint::black_box(v);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("8-task concurrent", Row1 { rq_ms, sq_ms }));
    }

    // --------------------------------------------- multi-connection concurrency
    // The engine supports ONE shared engine behind MANY pool connections
    // (`cache=shared`). SQLite's best in-memory-equivalent concurrency is
    // a file DB in WAL mode with a real connection pool: readers don't
    // block the writer and vice versa. Both sides get 8 connections.

    /// Shared in-memory rustqlite pool with N connections, pre-seeded.
    async fn rq_shared(n: u32, ddl: &'static str, name: &str) -> RustqlitePool {
        let pool = RustqlitePoolOptions::new()
            .max_connections(n)
            .connect_with(RustqliteConnectOptions::shared_memory(name))
            .await
            .unwrap();
        sqlx::query(ddl).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        for i in 0..ROWS {
            sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                .bind(i)
                .bind(i as f64 * 0.5)
                .bind(format!("name-{i}"))
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        pool
    }

    /// SQLite file DB in WAL mode with N connections, pre-seeded.
    async fn sq_wal(n: u32, ddl: &'static str) -> (tempfile::TempDir, sqlx::SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        use sqlx::sqlite::SqliteJournalMode;
        let pool = SqlitePoolOptions::new()
            .max_connections(n)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(tmp.path().join("wal.db"))
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();
        sqlx::query(ddl).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        for i in 0..ROWS {
            sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                .bind(i)
                .bind(i as f64 * 0.5)
                .bind(format!("name-{i}"))
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        (tmp, pool)
    }

    const CONC_TASKS: usize = 8;
    const CONC_PER: usize = 400;

    // -- 8-connection concurrent point lookups (shared engine vs WAL) ------
    {
        let name = "bench-conc-reads";
        let rq = rq_shared(CONC_TASKS as u32, ddl, name).await;
        let (_tmp, sq) = sq_wal(CONC_TASKS as u32, ddl).await;

        let t = Instant::now();
        let mut handles = Vec::new();
        for w in 0..CONC_TASKS {
            let pool = rq.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..CONC_PER {
                    let v: Option<i64> = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                        .bind(1 + ((i + w * 13) % OPS) as i64)
                        .fetch_optional(&pool)
                        .await
                        .unwrap()
                        .flatten();
                    std::hint::black_box(v);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        let mut handles = Vec::new();
        for w in 0..CONC_TASKS {
            let pool = sq.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..CONC_PER {
                    let v: Option<i64> = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                        .bind(1 + ((i + w * 13) % OPS) as i64)
                        .fetch_optional(&pool)
                        .await
                        .unwrap()
                        .flatten();
                    std::hint::black_box(v);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("8-conn concurrent reads", Row1 { rq_ms, sq_ms }));
    }

    // -- 8-connection MIXED read/write 80/20 (the concurrency stress test) -
    {
        let name = "bench-conc-mixed";
        let rq = rq_shared(CONC_TASKS as u32, ddl, name).await;
        let (_tmp, sq) = sq_wal(CONC_TASKS as u32, ddl).await;

        let t = Instant::now();
        let mut handles = Vec::new();
        for w in 0..CONC_TASKS {
            let pool = rq.clone();
            handles.push(tokio::spawn(async move {
                let mut next = 100_000i64 + w as i64 * 10_000;
                for i in 0..CONC_PER {
                    if i % 5 == 4 {
                        // 20% writes: short transaction, insert + update.
                        let mut tx = pool.begin().await.unwrap();
                        next += 1;
                        sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                            .bind(next)
                            .bind(next as f64 * 0.5)
                            .bind("w")
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                        sqlx::query("UPDATE bench SET b = b + 1.0 WHERE id = ?")
                            .bind(next)
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                        tx.commit().await.unwrap();
                    } else {
                        // 80% reads: point lookup + aggregate.
                        let v: i64 = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                            .bind(1 + ((i + w * 13) % OPS) as i64)
                            .fetch_optional(&pool)
                            .await
                            .unwrap()
                            .flatten()
                            .unwrap_or(0);
                        let _: (i64, f64) = sqlx::query_as(
                            "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?",
                        )
                        .bind(v)
                        .bind(v + 50)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                        std::hint::black_box(v);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        let mut handles = Vec::new();
        for w in 0..CONC_TASKS {
            let pool = sq.clone();
            handles.push(tokio::spawn(async move {
                let mut next = 100_000i64 + w as i64 * 10_000;
                for i in 0..CONC_PER {
                    if i % 5 == 4 {
                        let mut tx = pool.begin().await.unwrap();
                        next += 1;
                        sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                            .bind(next)
                            .bind(next as f64 * 0.5)
                            .bind("w")
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                        sqlx::query("UPDATE bench SET b = b + 1.0 WHERE id = ?")
                            .bind(next)
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                        tx.commit().await.unwrap();
                    } else {
                        let v: i64 = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                            .bind(1 + ((i + w * 13) % OPS) as i64)
                            .fetch_optional(&pool)
                            .await
                            .unwrap()
                            .flatten()
                            .unwrap_or(0);
                        let _: (i64, f64) = sqlx::query_as(
                            "SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?",
                        )
                        .bind(v)
                        .bind(v + 50)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                        std::hint::black_box(v);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("8-conn mixed R/W 80/20", Row1 { rq_ms, sq_ms }));
    }

    // -- 1 writer + 7 readers (writers must not starve readers) -----------
    {
        let name = "bench-conc-w7r";
        let rq = rq_shared(CONC_TASKS as u32, ddl, name).await;
        let (_tmp, sq) = sq_wal(CONC_TASKS as u32, ddl).await;

        let t = Instant::now();
        let mut handles = Vec::new();
        for r in 0..7 {
            let pool = rq.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..CONC_PER {
                    let _: Option<i64> =
                        sqlx::query_scalar("SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1")
                            .bind(((i * 37 + r * 11) % 4000) as i64)
                            .bind((((i * 37 + r * 11) % 4000) + 100) as i64)
                            .fetch_optional(&pool)
                            .await
                            .unwrap()
                            .flatten();
                }
            }));
        }
        {
            let pool = rq.clone();
            handles.push(tokio::spawn(async move {
                let mut next = 500_000i64;
                for _ in 0..40 {
                    let mut tx = pool.begin().await.unwrap();
                    for _ in 0..10 {
                        next += 1;
                        sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                            .bind(next)
                            .bind(0.5)
                            .bind("bw")
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                    }
                    tx.commit().await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let rq_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        let mut handles = Vec::new();
        for r in 0..7 {
            let pool = sq.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..CONC_PER {
                    let _: Option<i64> =
                        sqlx::query_scalar("SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1")
                            .bind(((i * 37 + r * 11) % 4000) as i64)
                            .bind((((i * 37 + r * 11) % 4000) + 100) as i64)
                            .fetch_optional(&pool)
                            .await
                            .unwrap()
                            .flatten();
                }
            }));
        }
        {
            let pool = sq.clone();
            handles.push(tokio::spawn(async move {
                let mut next = 500_000i64;
                for _ in 0..40 {
                    let mut tx = pool.begin().await.unwrap();
                    for _ in 0..10 {
                        next += 1;
                        sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                            .bind(next)
                            .bind(0.5)
                            .bind("bw")
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                    }
                    tx.commit().await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let sq_ms = t.elapsed().as_secs_f64() * 1e3;
        results.push(("1 writer + 7 readers", Row1 { rq_ms, sq_ms }));
    }

    // ------------------------------------------------------------------ report
    println!(
        "{:<26} {:>12} {:>12} {:>10}",
        "scenario", "rustqlite", "sqlx-sqlite", "speedup"
    );
    println!("{:-<62}", "");
    let mut wins = 0usize;
    let mut total_rq = 0.0;
    let mut total_sq = 0.0;
    for (name, r) in &results {
        total_rq += r.rq_ms;
        total_sq += r.sq_ms;
        if r.rq_ms <= r.sq_ms {
            wins += 1;
        }
        println!(
            "{:<26} {:>9.1} ms {:>9.1} ms {:>8.2}x",
            name,
            r.rq_ms,
            r.sq_ms,
            r.ratio()
        );
    }
    println!("{:-<62}", "");
    println!(
        "{:<26} {:>9.1} ms {:>9.1} ms {:>8.2}x",
        "TOTAL",
        total_rq,
        total_sq,
        total_sq / total_rq
    );
    println!(
        "\n{} / {} scenarios at parity or faster; {}",
        wins,
        results.len(),
        if wins == results.len() {
            "rustqlite native wins across the board."
        } else {
            "see per-scenario rows."
        }
    );

    rq.close().await;
    sq.close().await;
}
