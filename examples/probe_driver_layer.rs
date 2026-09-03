//! Layer attribution probe for the sqlx driver hot path.
//!
//! The CI bench-gate loses "txn: 100 inserts" on Windows (0.69x). Ubuntu
//! numbers attribute ~3 µs/statement to the sqlx DRIVER layer on top of the
//! engine's ~1 µs INSERT (bench_compare: engine 10.19 ms / 10k rows).
//! This probe isolates:
//!   1. parser::parse cost for the exact bench INSERT statement
//!   2. the full sqlx txn loop (same shape as bench_sqlx_native)
//!   3. the engine-only equivalent loop (no sqlx layer)
//!
//! so the win from skipping the per-statement classify parse is measurable.
//!
//! Run: cargo run --release --features sqlx --example probe_driver_layer

use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqlitePoolOptions};
use std::time::Instant;

const SQL: &str = "INSERT INTO bench (a, b, c) VALUES (?, ?, ?)";
const TXS: usize = 20;
const PER_TX: i64 = 100;

fn main() {
    // ---- 1. parser::parse cost ------------------------------------------
    {
        // warmup
        for _ in 0..1000 {
            let _ = rustqlite::sql::parser::parse(SQL);
        }
        let n = 200_000u32;
        let t = Instant::now();
        for _ in 0..n {
            let _ = rustqlite::sql::parser::parse(SQL);
        }
        let ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        println!("parser::parse (INSERT, 3 binds):  {ns:8.1} ns/parse");
    }

    // ---- 2. full sqlx txn loop (bench shape) -----------------------------
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    rt.block_on(async {
        let pool = RustqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(RustqliteConnectOptions::new())
            .await
            .unwrap();
        sqlx::query("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();

        // warmup
        {
            let mut tx = pool.begin().await.unwrap();
            for i in 0..100i64 {
                sqlx::query(SQL).bind(i).bind(0.0).bind("tx")
                    .execute(&mut *tx).await.unwrap();
            }
            tx.commit().await.unwrap();
        }

        let t = Instant::now();
        for _ in 0..TXS {
            let mut tx = pool.begin().await.unwrap();
            for i in 0..PER_TX {
                sqlx::query(SQL)
                    .bind(i)
                    .bind(0.0)
                    .bind("tx")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let per = ms * 1e3 / (TXS as f64 * PER_TX as f64);
        println!("sqlx  txn loop:  {ms:8.2} ms total  ({per:6.2} µs/insert, {TXS}x{PER_TX})");
        pool.close().await;
    });

    // ---- 3. engine-only equivalent loop ----------------------------------
    {
        use rustqlite::{Database, Value};
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
        // warmup
        db.execute("BEGIN", []).unwrap();
        for i in 0..100i64 {
            db.execute(SQL, rusqlite_params(i)).unwrap();
        }
        db.execute("COMMIT", []).unwrap();

        let t = Instant::now();
        for _ in 0..TXS {
            db.execute("BEGIN", []).unwrap();
            for i in 0..PER_TX {
                db.execute(SQL, rusqlite_params(i)).unwrap();
            }
            db.execute("COMMIT", []).unwrap();
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let per = ms * 1e3 / (TXS as f64 * PER_TX as f64);
        println!("engine txn loop: {ms:8.2} ms total  ({per:6.2} µs/insert)");
        let _ = Value::Integer(0);
    }

    // ---- 4. pure statement-cache lookup (no execution) -------------------
    {
        use rustqlite::Database;
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
        // warm the stmt cache
        let _ = db.prepare(SQL).unwrap();
        let n = 100_000u32;
        let t = Instant::now();
        for _ in 0..n {
            let _ = db.prepare(SQL).unwrap();
        }
        let ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        println!("db.prepare (cache HIT):          {ns:8.1} ns/prepare");
    }
}

fn rusqlite_params(i: i64) -> [rustqlite::Value; 3] {
    [
        rustqlite::Value::Integer(i),
        rustqlite::Value::Real(0.0),
        rustqlite::Value::Text("tx".into()),
    ]
}
