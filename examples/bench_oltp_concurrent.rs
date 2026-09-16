//! Benchmark: a realistic N-connection OLTP burst, quantifying the
//! group-commit fsync amortization and the optimistic-concurrency win
//! over the single-writer model.
//!
//! Each transaction is the classic OLTP payment shape:
//!
//! ```sql
//! UPDATE accounts SET balance = balance - ? WHERE id = ?   -- point write
//! INSERT INTO history (account_id, delta) VALUES (?, ?)    -- append
//! SELECT balance FROM accounts WHERE id = ?                 -- point read
//! ```
//!
//! with a think-time gap between statements — the pool-shaped workload
//! where optimistic concurrency wins (the write gate is never held across
//! think time). Connections run in lock-step rounds (barrier per
//! transaction): every round N transactions commit together, the
//! high-durability fsync-burst shape.
//!
//! Modes (`PRAGMA synchronous=FULL` unless noted):
//!   * `plain-full`      — plain `BEGIN` (single writer; the engine write
//!     gate serializes whole transactions; 1 fsync per commit by
//!     construction).
//!   * `concurrent-full` — `BEGIN CONCURRENT` (optimistic; disjoint-row
//!     writers MERGE; the group-commit leader fsyncs once per group).
//!   * `plain-normal` / `concurrent-normal` — the synchronous=NORMAL
//!     baselines (durability at checkpoints, not commits).
//!
//! Run:
//! ```text
//! cargo run --release --example bench_oltp_concurrent --features sqlx \
//!     [-- conns txns_per_conn scale_rows]
//! ```

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqliteConnection};
use sqlx::{Connection, Executor, Row};

/// Think-time between a transaction's statements — the gap a real
/// application spends between the statements of one unit of work. This is
/// exactly the shape single-writer engines pay for (the write gate is
/// held across the whole gap) and optimistic concurrency does not.
const THINK: Duration = Duration::from_micros(300);

fn think() {
    std::thread::sleep(THINK);
}

/// Tiny deterministic PRNG (xorshift64*) — reproducible account picks.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    PlainNormal,
    ConcurrentNormal,
    PlainFull,
    ConcurrentFull,
}

impl Mode {
    fn name(&self) -> &'static str {
        match self {
            Mode::PlainNormal => "plain-normal",
            Mode::ConcurrentNormal => "concurrent-normal",
            Mode::PlainFull => "plain-full",
            Mode::ConcurrentFull => "concurrent-full",
        }
    }
    fn concurrent(&self) -> bool {
        matches!(self, Mode::ConcurrentFull | Mode::ConcurrentNormal)
    }
    fn normal_sync(&self) -> bool {
        matches!(self, Mode::PlainNormal | Mode::ConcurrentNormal)
    }
}

/// The payment-transaction body: debit + audit-append + verify read,
/// with think-time gaps between the statements. The history append uses
/// an EXPLICIT unique rowid (the realistic audit-log shape — writer-side
/// unique ids, not engine-side auto rowids): implicit rowids would make
/// every overlapping transaction claim the SAME next rowid, a pure
/// collision cascade that serializes commits and hides the group-commit
/// fsync amortization (it remains the documented worst case for OCC).
async fn run_one_txn(
    conn: &mut RustqliteConnection,
    account: i64,
    delta: i64,
    history_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE accounts SET balance = balance - ? WHERE id = ?")
        .bind(delta)
        .bind(account)
        .execute(&mut *conn)
        .await?;
    think();
    sqlx::query("INSERT INTO history (id, account_id, delta) VALUES (?, ?, ?)")
        .bind(history_id)
        .bind(account)
        .bind(delta)
        .execute(&mut *conn)
        .await?;
    think();
    let _balance: i64 = sqlx::query("SELECT balance FROM accounts WHERE id = ?")
        .bind(account)
        .fetch_one(&mut *conn)
        .await?
        .get(0);
    think();
    Ok(())
}

/// One worker connection's whole run: `rounds` transactions in lock-step
/// with the other connections (barrier per round = the burst shape).
/// Retries a 517-conflicted transaction from its beginning (the realistic
/// client behavior). Returns (successful_commits, conflict_retries).
fn run_worker_burst(
    conn: RustqliteConnection,
    rounds: usize,
    scale: u64,
    seed: u64,
    mode: Mode,
    barrier: Arc<Barrier>,
) -> (usize, usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let (mut commits, mut conflicts) = (0usize, 0usize);
    rt.block_on(async {
        let mut conn = conn;
        let begin_stmt = if mode.concurrent() {
            "BEGIN CONCURRENT"
        } else {
            "BEGIN"
        };
        'rounds: for round in 0..rounds {
            let account = rng.below(scale) as i64;
            let delta = (rng.below(200) as i64) - 100;
            // Unique audit id: (worker slot, round) — disjoint across
            // connections, so history appends never collide.
            let history_id = (seed as i64) * 1_000_000 + round as i64;
            for _attempt in 0..8 {
                conn.execute(begin_stmt).await.unwrap();
                let r = run_one_txn(&mut conn, account, delta, history_id).await;
                match r {
                    Ok(()) => match conn.execute("COMMIT").await {
                        Ok(_) => {
                            commits += 1;
                            barrier.wait();
                            continue 'rounds;
                        }
                        // SQLITE_BUSY_SNAPSHOT (517): the engine rolled the
                        // whole transaction back — re-run it.
                        Err(e) if e.to_string().to_lowercase().contains("snapshot") => {
                            conflicts += 1
                        }
                        Err(e) => panic!("COMMIT failed: {e}"),
                    },
                    // A statement refused with BUSY (plain mode queuing):
                    // end the txn and retry it.
                    Err(e) if e.to_string().contains("locked") => {
                        let _ = conn.execute("ROLLBACK").await;
                    }
                    Err(e) => panic!("txn statement failed: {e}"),
                }
            }
            barrier.wait();
        }
        conn.close().await.unwrap();
    });
    (commits, conflicts)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let conns: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(8);
    let rounds: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(200);
    let scale: u64 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(10_000);

    let path = std::env::temp_dir().join(format!("bench-oltp-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    let path_in = path.clone();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let opts = RustqliteConnectOptions::filename(&path_in)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(60));
        let mut setup = RustqliteConnection::open(&opts).unwrap();
        setup.execute("PRAGMA journal_mode = WAL").await.unwrap();
        setup
            .execute(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL, \
                 name TEXT NOT NULL)",
            )
            .await
            .unwrap();
        setup
            .execute("CREATE INDEX idx_accounts_name ON accounts(name)")
            .await
            .unwrap();
        setup
            .execute(
                "CREATE TABLE history (id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL, \
                 delta INTEGER NOT NULL)",
            )
            .await
            .unwrap();
        // Seed the accounts table (one bulk autocommit batch). The SQL is
        // dynamic (10k rows of VALUES), so leak it to the 'static lifetime
        // the Executor bound wants — this binary runs once.
        let mut seed = String::from("INSERT INTO accounts (id, balance, name) VALUES ");
        for i in 0..scale {
            if i > 0 {
                seed.push(',');
            }
            seed.push_str(&format!("({}, 10000, 'acct-{}')", i, i));
        }
        let seed: &'static str = Box::leak(seed.into_boxed_str());
        setup.execute(seed).await.unwrap();

        println!(
            "OLTP burst benchmark: {conns} connections x {rounds} txns (barrier lock-step), \
             {scale} accounts, {THINK:?} think-time between statements"
        );
        println!("txn: UPDATE accounts + INSERT history + SELECT balance\n");

        let modes = [
            Mode::PlainNormal,
            Mode::ConcurrentNormal,
            Mode::PlainFull,
            Mode::ConcurrentFull,
        ];
        println!(
            "{:<18} {:>9} {:>8} {:>8} {:>8} {:>7} {:>9}",
            "mode", "wall_ms", "tps", "commits", "retries", "fsyncs", "fsync/txn"
        );
        for mode in modes {
            // Reset state between modes (the full-table DELETE is itself
            // part of what the benchmark exercises: it once exposed a
            // corrupted interior after the concurrent regime).
            setup
                .execute("UPDATE accounts SET balance = 10000")
                .await
                .unwrap();
            setup.execute("DELETE FROM history").await.unwrap();
            setup
                .execute(if mode.normal_sync() {
                    "PRAGMA synchronous = NORMAL"
                } else {
                    "PRAGMA synchronous = FULL"
                })
                .await
                .unwrap();

            // Pre-open the burst's connections (the pooled pattern).
            let mut workers: Vec<RustqliteConnection> = (0..conns)
                .map(|_| RustqliteConnection::open(&opts).unwrap())
                .collect();

            let (syncs0, _) = workers[0].pager_handle().group_commit_stats();
            let barrier = Arc::new(Barrier::new(conns));
            let t0 = Instant::now();
            let mut joins = Vec::new();
            for (i, w) in workers.drain(..).enumerate() {
                let barrier = barrier.clone();
                joins.push(std::thread::spawn(move || {
                    run_worker_burst(w, rounds, scale, i as u64 + 1, mode, barrier)
                }));
            }
            let (mut commits, mut retries) = (0usize, 0usize);
            for j in joins {
                let (c, cf) = j.join().expect("worker panicked");
                commits += c;
                retries += cf;
            }
            let wall = t0.elapsed();
            let (syncs, _) = setup.pager_handle().group_commit_stats();
            let group_syncs = syncs - syncs0;
            // Plain mode fsyncs once per commit by construction (inline
            // flush_wal under synchronous=FULL); concurrent mode counts the
            // group-commit leader's ACTUAL fsyncs.
            let fsyncs = if mode.concurrent() {
                group_syncs
            } else {
                commits as u64
            };
            let tps = commits as f64 / wall.as_secs_f64();
            println!(
                "{:<18} {:>9.1} {:>8.0} {:>8} {:>8} {:>7} {:>9.3}",
                mode.name(),
                wall.as_secs_f64() * 1e3,
                tps,
                commits,
                retries,
                fsyncs,
                fsyncs as f64 / commits.max(1) as f64
            );
            // Per-mode integrity: exactly one history row per committed
            // transaction, and the debits reconcile with the balances.
            let h: i64 = sqlx::query("SELECT count(*) FROM history")
                .fetch_one(&mut setup)
                .await
                .unwrap()
                .get(0);
            let sum: i64 = sqlx::query("SELECT ifnull(sum(delta), 0) FROM history")
                .fetch_one(&mut setup)
                .await
                .unwrap()
                .get(0);
            let bal: i64 = sqlx::query("SELECT ifnull(sum(balance), 0) FROM accounts")
                .fetch_one(&mut setup)
                .await
                .unwrap()
                .get(0);
            let expect_bal: i64 = 10_000 * scale as i64 - sum;
            println!(
                "  integrity[{}]: history={} (commits={}), balance={} (expected {}) {}",
                mode.name(),
                h,
                commits,
                bal,
                expect_bal,
                if h == commits as i64 && bal == expect_bal {
                    "OK"
                } else {
                    "LEAK!"
                }
            );
            assert_eq!(
                h,
                commits as i64,
                "{}: history rows must equal commits",
                mode.name()
            );
            assert_eq!(bal, expect_bal, "{}: debits must reconcile", mode.name());
        }

        // Final integrity: every debit landed (balance + history reconcile).
        let n: i64 = sqlx::query("SELECT count(*) FROM accounts")
            .fetch_one(&mut setup)
            .await
            .unwrap()
            .get(0);
        let h: i64 = sqlx::query("SELECT count(*) FROM history")
            .fetch_one(&mut setup)
            .await
            .unwrap()
            .get(0);
        let sum: i64 = sqlx::query("SELECT ifnull(sum(delta), 0) FROM history")
            .fetch_one(&mut setup)
            .await
            .unwrap()
            .get(0);
        let bal: i64 = sqlx::query("SELECT ifnull(sum(balance), 0) FROM accounts")
            .fetch_one(&mut setup)
            .await
            .unwrap()
            .get(0);
        println!(
            "\nintegrity: {n} accounts, {h} history rows, sum(delta)={sum}, \
             expected balance sum={} (actual {bal})",
            n * 10_000 - sum
        );
        assert_eq!(bal, n * 10_000 - sum, "debits must reconcile with balances");
        setup.close().await.unwrap();
    });

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}
