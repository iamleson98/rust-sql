//! Probe: 1 writer + 7 readers interference analysis.
//! Measures readers-only, writer-only, and combined to find where the
//! throughput collapse comes from.

use std::time::Instant;

use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqlitePool, RustqlitePoolOptions};

const READERS: usize = 7;
const PER: usize = 400;
const TXNS: usize = 40;

async fn setup(name: &str, conns: u32) -> RustqlitePool {
    let pool = RustqlitePoolOptions::new()
        .max_connections(conns)
        .connect_with(RustqliteConnectOptions::shared_memory(name))
        .await
        .unwrap();
    sqlx::query("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    for i in 0..5000i64 {
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

async fn readers(pool: &RustqlitePool) {
    let mut handles = Vec::new();
    for r in 0..READERS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER {
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
    for h in handles {
        h.await.unwrap();
    }
}

async fn writer(pool: &RustqlitePool) {
    let mut next = 500_000i64;
    for _ in 0..TXNS {
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
}

#[tokio::main]
async fn main() {
    println!(
        "cores: {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    // (a) readers only
    let pool = setup("probe-w7r-a", 8).await;
    let t = Instant::now();
    readers(&pool).await;
    println!(
        "readers-only   : {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    drop(pool);

    // (b) writer only
    let pool = setup("probe-w7r-b", 8).await;
    let t = Instant::now();
    writer(&pool).await;
    println!(
        "writer-only    : {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    drop(pool);

    // (c) combined
    let pool = setup("probe-w7r-c", 8).await;
    let t = Instant::now();
    let w = {
        let pool = pool.clone();
        tokio::spawn(async move { writer(&pool).await })
    };
    readers(&pool).await;
    w.await.unwrap();
    println!(
        "combined       : {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    // (d) point-lookup readers + writer (same shape but indexed reads)
    let pool = setup("probe-w7r-d", 8).await;
    let t = Instant::now();
    let w = {
        let pool = pool.clone();
        tokio::spawn(async move { writer(&pool).await })
    };
    let mut handles = Vec::new();
    for r in 0..READERS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER {
                let _: Option<i64> = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                    .bind(1 + ((i * 37 + r * 11) % 4000) as i64)
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                    .flatten();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    w.await.unwrap();
    println!(
        "combined (idx) : {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    // (e) single reader query latency (serial, for scale)
    let pool = setup("probe-w7r-e", 1).await;
    let t = Instant::now();
    for i in 0..500 {
        let _: Option<i64> =
            sqlx::query_scalar("SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1")
                .bind(((i * 37) % 4000) as i64)
                .bind((((i * 37) % 4000) + 100) as i64)
                .fetch_optional(&pool)
                .await
                .unwrap()
                .flatten();
    }
    println!(
        "serial latency : {:>8.1} µs/query",
        t.elapsed().as_secs_f64() * 1e6 / 500.0
    );
}
