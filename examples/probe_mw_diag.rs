//! Diagnostic companion to probe_mw_hammer: isolates the reader-visibility
//! divergence under concurrent writes. One writer hammers a DIFFERENT
//! table (accounts row count must be invariant throughout); the reader
//! checks it and dumps the state at the first divergence.
//! build: cargo build --release --features sqlx --example probe_mw_diag
use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqliteConnection};
use sqlx::{Connection, Executor};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

static WRITER_COMMITS: AtomicU64 = AtomicU64::new(0);

fn main() {
    let file = "/tmp/mwdiag.db";
    let accounts: i64 = 10_000;
    let _ = std::fs::remove_file(file);
    let _ = std::fs::remove_file(format!("{}-wal", file));
    let opts = RustqliteConnectOptions::filename(file).create_if_missing(true);

    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut c = RustqliteConnection::open(&opts).unwrap();
            c.execute("PRAGMA journal_mode = WAL").await.unwrap();
            c.execute(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL);
                 CREATE TABLE audit (seq INTEGER PRIMARY KEY, w INTEGER, account INTEGER, ts INTEGER, delta INTEGER);
                 CREATE INDEX ia ON audit(account);",
            )
            .await
            .unwrap();
            c.execute("BEGIN").await.unwrap();
            for i in 1..=accounts {
                sqlx::query("INSERT INTO accounts (id, balance) VALUES (?, 1000)")
                    .bind(i)
                    .execute(&mut c)
                    .await
                    .unwrap();
            }
            c.execute("COMMIT").await.unwrap();
            c.close().await.unwrap();
        });
    }
    println!("setup done: 10000 accounts committed");

    // BISECT: fresh connection view BEFORE any writer arms
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fresh = RustqliteConnection::open(&opts).unwrap();
            let mut fresh = fresh;
            let row: (i64, Option<i64>, Option<i64>) = sqlx::query_as(
                "SELECT COUNT(*), MIN(id), MAX(id) FROM accounts",
            )
            .fetch_one(&mut fresh)
            .await
            .unwrap();
            let rp: (i64, i64) = sqlx::query_as("SELECT rootpage, page_count FROM (SELECT rootpage, 1 AS page_count FROM sqlite_master WHERE name = 'accounts')")
                .fetch_one(&mut fresh).await.unwrap();
            println!("PRE-BURST fresh connection: count={} min={:?} max={:?} accounts_rootpage={}", row.0, row.1, row.2, rp.0);
            let rp2: i64 = sqlx::query_scalar("SELECT rootpage FROM sqlite_master WHERE name = 'audit'")
                .fetch_one(&mut fresh).await.unwrap();
            println!("PRE-BURST audit rootpage={}", rp2);
            let _ = fresh.close().await;
        });
    }

    let done = Arc::new(AtomicBool::new(false));

    // writers: BEGIN CONCURRENT update+audit transactions (the hammer shape)
    let nwriters: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let mut wconns: Vec<_> = (0..nwriters)
        .map(|_| RustqliteConnection::open(&opts).unwrap())
        .collect();
    let mut whandles = Vec::new();
    for (w, wc) in wconns.drain(..).enumerate() {
        let wdone = done.clone();
        whandles.push(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut c = wc;
                let mut n = 0i64;
                let mut id = 1i64;
                loop {
                    if wdone.load(Ordering::Relaxed) && n > 50 {
                        break;
                    }
                    id = id % 10_000 + 1;
                    if c.execute("BEGIN CONCURRENT").await.is_err() {
                        let _ = c.execute("ROLLBACK").await;
                        continue;
                    }
                    let r = sqlx::query("UPDATE accounts SET balance = balance - 1 WHERE id = ?")
                        .bind(id)
                        .execute(&mut c)
                        .await;
                    let r = r.map(|_| ());
                    match r {
                        Ok(_) => {
                            let ins = sqlx::query(
                                "INSERT INTO audit (w, account, ts, delta) VALUES (?, ?, ?, -1)",
                            )
                            .bind(w as i64)
                            .bind(id)
                            .bind(n)
                            .execute(&mut c)
                            .await;
                            match ins {
                                Ok(_) => {
                                    if c.execute("COMMIT").await.is_ok() {
                                        n += 1;
                                        WRITER_COMMITS.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        let _ = c.execute("ROLLBACK").await;
                                    }
                                }
                                Err(_) => {
                                    let _ = c.execute("ROLLBACK").await;
                                }
                            }
                        }
                        Err(_) => {
                            let _ = c.execute("ROLLBACK").await;
                        }
                    }
                }
                let _ = c.close().await;
            });
        }));
    }

    // reader: accounts count must be 10000 at every committed instant
    let rconn = RustqliteConnection::open(&opts).unwrap();
    let reader_done = done.clone();
    let reader = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut c = rconn;
            let mut rounds = 0u64;
            while rounds < 200000 {
                let row: (i64, Option<i64>, Option<i64>) =
                    sqlx::query_as("SELECT COUNT(*), MIN(id), MAX(id) FROM accounts")
                        .fetch_one(&mut c)
                        .await
                        .unwrap();
                let (n, mn, mx) = row;
                if n != accounts || mn != Some(1) || mx != Some(accounts) {
                    println!(
                        "DIVERGENCE round={} count={} min={:?} max={:?} writer_commits={}",
                        rounds,
                        n,
                        mn,
                        mx,
                        WRITER_COMMITS.load(Ordering::Relaxed)
                    );
                    // immediate re-query on the SAME connection
                    let row2: (i64, Option<i64>, Option<i64>) =
                        sqlx::query_as("SELECT COUNT(*), MIN(id), MAX(id) FROM accounts")
                            .fetch_one(&mut c)
                            .await
                            .unwrap();
                    println!(
                        "  immediate re-query: count={} min={:?} max={:?}",
                        row2.0, row2.1, row2.2
                    );
                    // and a FRESH connection's view
                    let fresh = RustqliteConnection::open(&RustqliteConnectOptions::filename(
                        "/tmp/mwdiag.db",
                    ))
                    .unwrap();
                    let mut fresh = fresh;
                    let row3: (i64, Option<i64>, Option<i64>) =
                        sqlx::query_as("SELECT COUNT(*), MIN(id), MAX(id) FROM accounts")
                            .fetch_one(&mut fresh)
                            .await
                            .unwrap();
                    println!(
                        "  fresh connection : count={} min={:?} max={:?}",
                        row3.0, row3.1, row3.2
                    );
                    let _ = fresh.close().await;
                    // ---- discriminator battery ----
                    // 1. point reads: does the tree actually hold row 500/1000?
                    for probe_id in [500i64, 1000, 468, 467] {
                        let hit: Option<i64> =
                            sqlx::query_scalar("SELECT balance FROM accounts WHERE id = ?")
                                .bind(probe_id)
                                .fetch_optional(&mut c)
                                .await
                                .unwrap();
                        println!("  point read id={}: {:?}", probe_id, hit);
                    }
                    // 2. full walk (no COUNT memo): force row-by-row scan
                    let walked: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT id FROM accounts)")
                            .fetch_one(&mut c)
                            .await
                            .unwrap();
                    println!("  subquery walk count: {}", walked);
                    // 3. forced decode walk
                    let summed: i64 = sqlx::query_scalar("SELECT COUNT(balance) FROM accounts")
                        .fetch_one(&mut c)
                        .await
                        .unwrap();
                    println!("  COUNT(balance)     : {}", summed);
                    // 4. integrity from the reader's view
                    let ic: String = sqlx::query_scalar("PRAGMA integrity_check")
                        .fetch_one(&mut c)
                        .await
                        .unwrap();
                    println!("  integrity_check    : {}", ic);
                    // rootpage check: catalog vs content
                    let rp: i64 = sqlx::query_scalar(
                        "SELECT rootpage FROM sqlite_master WHERE name = 'accounts'",
                    )
                    .fetch_one(&mut c)
                    .await
                    .unwrap();
                    println!("  DIVERGED accounts rootpage={} (pre-burst was above)", rp);
                    let rp2: i64 = sqlx::query_scalar(
                        "SELECT rootpage FROM sqlite_master WHERE name = 'audit'",
                    )
                    .fetch_one(&mut c)
                    .await
                    .unwrap();
                    println!("  DIVERGED audit rootpage={}", rp2);
                    // 5. file sizes
                    for f in ["/tmp/mwdiag.db", "/tmp/mwdiag.db-wal"] {
                        if let Ok(m) = std::fs::metadata(f) {
                            println!("  file {} : {} bytes", f, m.len());
                        }
                    }
                    return (rounds, n);
                }
                rounds += 1;
            }
            reader_done.store(true, Ordering::Relaxed);
            (rounds, accounts)
        })
    });

    // stop the writer after 20s no matter what
    std::thread::sleep(std::time::Duration::from_secs(20));
    done.store(true, Ordering::Relaxed);
    for h in whandles {
        let _ = h.join();
    }
    let res = reader.join().unwrap();
    println!("reader finished: {:?}", res);
}
