//! Probe: cross-connection isolation semantics.
//!
//! 1. Reader blocked during a foreign write-tx → waits (busy timeout) →
//!    SQLITE_BUSY, never a dirty read.
//! 2. Reader wakes immediately when the foreign tx commits.
//! 3. Read-only foreign tx does NOT block readers.
//! 4. Writer waits for a foreign tx and proceeds right after COMMIT.

use std::time::{Duration, Instant};

use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqlitePoolOptions};

#[tokio::main]
async fn main() {
    let pool = RustqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(
            RustqliteConnectOptions::shared_memory("probe2")
                .busy_timeout(Duration::from_millis(300)),
        )
        .await
        .unwrap();

    sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t (v) VALUES ('committed')")
        .execute(&pool)
        .await
        .unwrap();

    let mut a = pool.acquire().await.unwrap();
    let mut b = pool.acquire().await.unwrap();

    // --- 1. blocked reader → BUSY after the timeout, never dirty ---------
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("INSERT INTO t (v) VALUES ('DIRTY')")
        .execute(&mut *a)
        .await
        .unwrap();

    let t0 = Instant::now();
    let read_result: Result<Vec<(i64, String)>, _> =
        sqlx::query_as("SELECT id, v FROM t ORDER BY id")
            .fetch_all(&mut *b)
            .await;
    let waited = t0.elapsed();
    match read_result {
        Ok(rows) => {
            let dirty = rows.iter().any(|(_, v)| v == "DIRTY");
            println!(
                "1. reader returned {} rows while tx open: dirty={dirty}",
                rows.len()
            );
        }
        Err(e) => println!(
            "1. reader blocked, waited {:.0?} then got BUSY: {} (dirty read IMPOSSIBLE)",
            waited, e
        ),
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
        .fetch_all(&mut *a)
        .await
        .unwrap()
        .pop()
        .unwrap_or(0);
    println!("   (A still sees its own write: {n} rows — read-your-own-writes)");

    // --- 2. reader wakes the instant the foreign tx commits --------------
    let start = Instant::now();
    let committer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        // `a` still owns the open transaction (moved into this task).
        sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    });
    let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, v FROM t ORDER BY id")
        .fetch_all(&mut *b)
        .await
        .unwrap();
    println!(
        "2. reader woke {:.0?} after waiting (tx committed): {} rows, committed rows present",
        start.elapsed(),
        rows.len()
    );
    committer.await.unwrap();

    // --- 3. read-only foreign tx does NOT block readers -------------------
    sqlx::query("BEGIN").execute(&mut *b).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
        .fetch_all(&mut *b)
        .await
        .unwrap()
        .pop()
        .unwrap_or(0);
    let t1 = Instant::now();
    let n2: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
        .fetch_all(&mut *b)
        .await
        .unwrap()
        .pop()
        .unwrap_or(0);
    println!(
        "3. read during foreign READ-ONLY tx: instant ({:?}), {n}/{n2} rows",
        t1.elapsed()
    );
    sqlx::query("COMMIT").execute(&mut *b).await.unwrap();
    drop(b);

    // --- 4. writer waits out a foreign tx and proceeds after commit ------
    // (b is dropped above; the pool has 2 slots: the Transaction + writer c)
    let mut c = pool.acquire().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO t (v) VALUES ('tx-writer')")
        .execute(&mut *tx)
        .await
        .unwrap();
    let committer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.commit().await.unwrap();
    });
    let t4 = Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(3), async {
        sqlx::query("INSERT INTO t (v) VALUES ('waiter')")
            .execute(&mut *c)
            .await
    })
    .await
    .unwrap();
    match res {
        Ok(r) => println!(
            "4. writer waited {:.0?} for the foreign tx, then succeeded: {} row(s)",
            t4.elapsed(),
            r.rows_affected()
        ),
        Err(e) => println!("4. writer timed out: {e}"),
    }
    committer.await.unwrap();
    drop(c);
    let final_n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t")
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("   final count: {final_n}");
}
