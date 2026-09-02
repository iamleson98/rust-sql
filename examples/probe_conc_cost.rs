//! Probe: where does multi-connection concurrent throughput go?
//!
//! Compares the same mixed read/write workload under:
//!   1. serial (1 task)
//!   2. 8 tasks, 1-connection pool (multiplexed)
//!   3. 8 tasks, 8-connection pool (true concurrency)
//!   4. reads-only 8 tasks, 8 conns
//!   5. writes-only 8 tasks, 8 conns
//! …plus per-query atomic counters to see lock/gate behavior.

use std::time::Instant;

use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqlitePool, RustqlitePoolOptions};

const TASKS: usize = 8;
const PER: usize = 400;
const ROWS: i64 = 5_000;

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

async fn mixed_op(pool: &RustqlitePool, task: usize, i: usize, counter: &mut usize) {
    if i % 5 == 4 {
        let mut tx = pool.begin().await.unwrap();
        let next = 1_000_000i64 + (task as i64) * 100_000 + i as i64;
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
        *counter += 1;
    } else {
        let v: i64 = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
            .bind(1 + ((i + task * 13) % 2000) as i64)
            .fetch_optional(pool)
            .await
            .unwrap()
            .flatten()
            .unwrap_or(0);
        let _: (i64, f64) = sqlx::query_as("SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?")
            .bind(v)
            .bind(v + 50)
            .fetch_one(pool)
            .await
            .unwrap();
        *counter += 2;
    }
}

#[tokio::main]
async fn main() {
    // 1. serial baseline
    let pool = setup("probe-serial", 1).await;
    let t = Instant::now();
    let mut n = 0usize;
    for i in 0..PER {
        mixed_op(&pool, 0, i, &mut n).await;
    }
    println!("serial 1 conn        : {:>8.1} ms  ({n} queries)", t.elapsed().as_secs_f64() * 1e3);
    drop(pool);

    // 2. 8 tasks, 1 connection
    let pool = setup("probe-mux", 1).await;
    let t = Instant::now();
    let mut handles = Vec::new();
    for w in 0..TASKS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut n = 0usize;
            for i in 0..PER {
                mixed_op(&pool, w, i, &mut n).await;
            }
            n
        }));
    }
    let mut total = 0usize;
    for h in handles {
        total += h.await.unwrap();
    }
    println!("8 tasks 1 conn       : {:>8.1} ms  ({total} queries)", t.elapsed().as_secs_f64() * 1e3);
    drop(pool);

    // 3. 8 tasks, 8 connections
    let pool = setup("probe-multi", TASKS as u32).await;
    let t = Instant::now();
    let mut handles = Vec::new();
    for w in 0..TASKS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut n = 0usize;
            for i in 0..PER {
                mixed_op(&pool, w, i, &mut n).await;
            }
            n
        }));
    }
    let mut total = 0usize;
    for h in handles {
        total += h.await.unwrap();
    }
    println!("8 tasks 8 conns      : {:>8.1} ms  ({total} queries)", t.elapsed().as_secs_f64() * 1e3);
    drop(pool);

    // 4. reads-only, 8 conns
    let pool = setup("probe-reads", TASKS as u32).await;
    let t = Instant::now();
    let mut handles = Vec::new();
    for w in 0..TASKS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER {
                let v: i64 = sqlx::query_scalar("SELECT a FROM bench WHERE id = ?")
                    .bind(1 + ((i + w * 13) % 2000) as i64)
                    .fetch_optional(&pool)
                    .await
                    .unwrap()
                    .flatten()
                    .unwrap_or(0);
                let _: (i64, f64) =
                    sqlx::query_as("SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?")
                        .bind(v)
                        .bind(v + 50)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    println!("8 tasks reads-only   : {:>8.1} ms", t.elapsed().as_secs_f64() * 1e3);
    drop(pool);

    // 5. writes-only, 8 conns
    let pool = setup("probe-writes", TASKS as u32).await;
    let t = Instant::now();
    let mut handles = Vec::new();
    for w in 0..TASKS {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER / 4 {
                let mut tx = pool.begin().await.unwrap();
                let next = 2_000_000i64 + (w as i64) * 100_000 + i as i64;
                sqlx::query("INSERT INTO bench (a, b, c) VALUES (?, ?, ?)")
                    .bind(next)
                    .bind(0.5)
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
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    println!("8 tasks writes-only  : {:>8.1} ms", t.elapsed().as_secs_f64() * 1e3);

    // 6. single aggregate query cost (for scale)
    let pool = setup("probe-agg", 1).await;
    let t = Instant::now();
    for i in 0..2000i64 {
        let _: (i64, f64) =
            sqlx::query_as("SELECT COUNT(*), AVG(b) FROM bench WHERE a BETWEEN ? AND ?")
                .bind((i * 2) % 4000)
                .bind((i * 2) % 4000 + 50)
                .fetch_one(&pool)
                .await
                .unwrap();
    }
    println!("2000 agg queries     : {:>8.1} ms  (serial, 1 conn)", t.elapsed().as_secs_f64() * 1e3);
}
