//! Probe: adversarial multi-writer hammer + integrity reconciliation.
//!
//! N writer threads, each driving its OWN sqlx-driver connection
//! (`RustqliteConnection` — the same connection identity machinery a real
//! pool uses) run `BEGIN CONCURRENT` payment-shaped transactions (debit +
//! audit append) against one file; M reader connections continuously check
//! snapshot invariants; at the end a FRESH engine handle runs
//! integrity_check, reconciles every counter, `VACUUM INTO`s an export,
//! and REAL SQLite (rusqlite) re-opens the export and verifies the same
//! invariants.
//!
//! usage: probe_mw_hammer [file] [writers] [readers] [txns_per_writer]
//! build: cargo build --release --features sqlx --example probe_mw_hammer
use rustqlite::sqlx_driver::{RustqliteConnectOptions, RustqliteConnection};
use rustqlite::Database;
use sqlx::{Connection, Executor};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

static COMMITTED: AtomicU64 = AtomicU64::new(0);
static CONFLICTS: AtomicU64 = AtomicU64::new(0);
static STMT_ERRS: AtomicU64 = AtomicU64::new(0);
static READ_ROUNDS: AtomicU64 = AtomicU64::new(0);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let file = args.get(1).cloned().unwrap_or("/tmp/mw_hammer.db".into());
    let writers: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    let readers: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let txns: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(200);
    let accounts: i64 = 10_000;
    let seed_balance: i64 = 1_000;

    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(format!("{}-wal", file));
    let _ = std::fs::remove_file("mw_export_check.db");

    let opts = RustqliteConnectOptions::filename(&file).create_if_missing(true);

    // ---- setup (driver connection; WAL armed BEFORE the burst) ----
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut c = RustqliteConnection::open(&opts).unwrap();
            c.execute("PRAGMA journal_mode = WAL").await.unwrap();
            c.execute(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL, note TEXT);
                 CREATE TABLE audit (seq INTEGER PRIMARY KEY, w INTEGER, account INTEGER, ts INTEGER, delta INTEGER);
                 CREATE INDEX ia ON audit(account);",
            )
            .await
            .unwrap();
            c.execute("BEGIN").await.unwrap();
            for i in 1..=accounts {
                sqlx::query("INSERT INTO accounts (id, balance, note) VALUES (?, ?, ?)")
                    .bind(i)
                    .bind(seed_balance)
                    .bind(format!("acct-{}", i))
                    .execute(&mut c)
                    .await
                    .unwrap();
            }
            c.execute("COMMIT").await.unwrap();
            c.close().await.unwrap();
        });
    }

    // ---- open all connections up front (the pooled pattern), then burst ----
    let mut writer_conns: Vec<_> = (0..writers)
        .map(|_| RustqliteConnection::open(&opts).unwrap())
        .collect();
    let mut reader_conns: Vec<_> = (0..readers)
        .map(|_| RustqliteConnection::open(&opts).unwrap())
        .collect();

    let done = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();

    // ---- readers: snapshot invariants, never blocking ----
    let mut reader_handles = Vec::new();
    for c in reader_conns.drain(..) {
        let done = done.clone();
        reader_handles.push(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut c = c;
                let mut last_sum = i64::MAX;
                while !done.load(Ordering::Relaxed) {
                    let row: (i64, i64, i64, i64) = sqlx::query_as(
                        "SELECT COUNT(*), SUM(balance), MIN(balance), MAX(balance) FROM accounts",
                    )
                    .fetch_one(&mut c)
                    .await
                    .unwrap();
                    let (n, s, mn, mx) = row;
                    // invariants: full table always visible, sum only ever
                    // decreases by committed debits, balances stay in range.
                    assert_eq!(n, accounts, "reader saw partial table");
                    assert!(s <= last_sum, "reader saw sum go UP ({} > {})", s, last_sum);
                    assert!(
                        mn >= seed_balance - (txns as i64 + 10) * writers as i64,
                        "balance underflow: {}",
                        mn
                    );
                    assert!(mx <= seed_balance, "balance overflow: {}", mx);
                    last_sum = last_sum.min(s);
                    READ_ROUNDS.fetch_add(1, Ordering::Relaxed);
                }
                c.close().await.unwrap();
            });
        }));
    }

    // ---- writers: concurrent payment transactions ----
    let mut writer_handles = Vec::new();
    for (w, c) in writer_conns.drain(..).enumerate() {
        writer_handles.push(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut c = c;
                let mut committed = 0u64;
                let mut conflicts = 0u64;
                let mut stmt_errs = 0u64;
                let mut seed = 0x2545F4914F6CDD1Du64 ^ (w as u64 + 1);
                'txn: for t in 0..txns {
                    for _attempt in 0..80 {
                        // xorshift64*
                        seed ^= seed >> 12;
                        seed ^= seed << 25;
                        seed ^= seed >> 27;
                        let acct = (seed.wrapping_mul(0x2545F4914F6CDD1D) % accounts as u64) as i64 + 1;
                        if c.execute("BEGIN CONCURRENT").await.is_err() {
                            stmt_errs += 1;
                            let _ = c.execute("ROLLBACK").await;
                            continue;
                        }
                        let up = sqlx::query("UPDATE accounts SET balance = balance - 1 WHERE id = ?")
                            .bind(acct)
                            .execute(&mut c)
                            .await;
                        match up {
                            Ok(_) => {
                                let ins =
                                    sqlx::query("INSERT INTO audit (w, account, ts, delta) VALUES (?, ?, ?, -1)")
                                        .bind(w as i64)
                                        .bind(acct)
                                        .bind(t as i64)
                                        .execute(&mut c)
                                        .await;
                                match ins {
                                    Ok(_) => match c.execute("COMMIT").await {
                                        Ok(_) => {
                                            committed += 1;
                                            continue 'txn;
                                        }
                                        Err(_) => {
                                            conflicts += 1;
                                            let _ = c.execute("ROLLBACK").await;
                                        }
                                    },
                                    Err(_) => {
                                        stmt_errs += 1;
                                        let _ = c.execute("ROLLBACK").await;
                                    }
                                }
                            }
                            Err(_) => {
                                stmt_errs += 1;
                                let _ = c.execute("ROLLBACK").await;
                            }
                        }
                    }
                }
                COMMITTED.fetch_add(committed, Ordering::Relaxed);
                CONFLICTS.fetch_add(conflicts, Ordering::Relaxed);
                STMT_ERRS.fetch_add(stmt_errs, Ordering::Relaxed);
                c.close().await.unwrap();
            });
        }));
    }

    for h in writer_handles {
        h.join().expect("writer panicked");
    }
    done.store(true, Ordering::Relaxed);
    for h in reader_handles {
        h.join().expect("reader panicked");
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let committed = COMMITTED.load(Ordering::Relaxed);
    let conflicts = CONFLICTS.load(Ordering::Relaxed);
    let stmt_errs = STMT_ERRS.load(Ordering::Relaxed);

    // ---- reconciliation on a FRESH engine handle ----
    let expected_debits = committed as i64; // every committed txn debits exactly 1
    {
        let mut db = Database::open(&file).unwrap();
        let ic: String = db.query("PRAGMA integrity_check", []).unwrap()[0][0].as_text();
        assert_eq!(ic, "ok", "integrity_check failed after hammer");
        let n: i64 = db.query("SELECT COUNT(*) FROM accounts", []).unwrap()[0][0].as_integer();
        let s: i64 = db.query("SELECT SUM(balance) FROM accounts", []).unwrap()[0][0].as_integer();
        let audit: i64 = db.query("SELECT COUNT(*) FROM audit", []).unwrap()[0][0].as_integer();
        assert_eq!(n, accounts, "account count drifted");
        assert_eq!(
            s,
            accounts * seed_balance - expected_debits,
            "balance reconciliation failed: got {}",
            s
        );
        assert_eq!(
            audit, expected_debits,
            "audit reconciliation failed: got {}",
            audit
        );
        // reopen durability: export and hand to real SQLite
        db.execute("VACUUM INTO 'mw_export_check.db'", []).unwrap();
    }

    // ---- real SQLite verifies the export ----
    {
        let conn = rusqlite::Connection::open("mw_export_check.db").unwrap();
        let ic: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ic, "ok", "SQLite rejects the export");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        let s: i64 = conn
            .query_row("SELECT SUM(balance) FROM accounts", [], |r| r.get(0))
            .unwrap();
        let a: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, accounts);
        assert_eq!(s, accounts * seed_balance - expected_debits);
        assert_eq!(a, expected_debits);
        let (w, acct, ts): (i64, i64, i64) = conn
            .query_row(
                "SELECT w, account, ts FROM audit ORDER BY seq LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(
            w >= 0 && acct >= 1 && acct <= accounts && ts >= 0,
            "bad audit row"
        );
    }
    let _ = std::fs::remove_file("mw_export_check.db");

    println!(
        "HAMMER OK  writers={} readers={} txns/writer={} | committed={} conflicts_retried={} stmt_errs={} read_rounds={} | {:.0} txns/s | integrity ok | SQLite export verified",
        writers,
        readers,
        txns,
        committed,
        conflicts,
        stmt_errs,
        READ_ROUNDS.load(Ordering::Relaxed),
        committed as f64 / elapsed
    );
    // Every transaction must eventually commit (retry loop is generous).
    assert_eq!(committed, writers as u64 * txns, "lost transactions!");
}
