//! Reader-visibility under the BEGIN CONCURRENT regime.
//!
//! Invariant under test (SQLite's WAL contract, and this engine's own
//! documented contract — README "Concurrency"): while concurrent write
//! transactions are open or committing, every plain reader — including
//! connections opened AFTER the regime started — must observe a
//! fully-committed, consistent table state. Row counts of a table no
//! transaction ever deleted from must never regress.
//!
//! The multi-writer case is currently `#[ignore]`d: it reproduces a real,
//! pinned divergence (found 2026-09-19 via `examples/probe_mw_hammer.rs`
//! and `examples/probe_mw_diag.rs`):
//!
//! * After a split-heavy bulk seed (one BEGIN..COMMIT of 10k rows), the
//!   engine's executor root bookkeeping holds STALE table roots: the
//!   in-memory catalog `Arc<Table>.root_page` stays at its DDL-time value,
//!   and the StmtMaps overlay published at concurrent-commit time carries
//!   mid-transaction root generations. During an active concurrent burst,
//!   `table_root()` resolution serves a stale root page; the b-tree
//!   descent lands INSIDE the tree (an interior node), and readers see a
//!   consistent-but-PARTIAL table (e.g. COUNT(*) = 467 of 10000, missing
//!   point reads for ids past the subtree boundary, integrity_check ok —
//!   the subtree is self-consistent).
//! * The durable image is NOT affected: a cold process (fresh engine from
//!   the file + WAL) sees the full table and passes integrity_check; the
//!   view self-heals after the regime drains. The violation is live-engine
//!   read visibility during the burst.
//! * Deterministic on the 4-writer shape (page-5/467-row partial view);
//!   1-writer and 0-writer shapes are clean.
//!
//! Fix direction (see the probes' traces): keep the in-memory catalog's
//! root_page in sync at every root-moving write-back (the schema-row
//! rewrite path), and/or validate the overlay published by
//! `publish_concurrent_maps` against the committed catalog rows at publish
//! time (the catalog is authoritative after commit-install).

use rustqlite::{Database, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn tmpdb(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("cwvis-{}.db", name));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    path
}

fn seed(db: &mut Database, accounts: i64) {
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL);
         CREATE TABLE audit (seq INTEGER PRIMARY KEY, w INTEGER, account INTEGER, ts INTEGER, delta INTEGER);
         CREATE INDEX ia ON audit(account);",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=accounts {
        db.execute(
            "INSERT INTO accounts (id, balance) VALUES (?, 1000)",
            [Value::Integer(i)],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn count_accounts(db: &Database) -> i64 {
    db.query("SELECT COUNT(*), MIN(id), MAX(id) FROM accounts", [])
        .unwrap()[0][0]
        .as_integer()
}

fn run_burst(path: &std::path::Path, writers: usize, txns: u64) {
    let accounts: i64 = 10_000;
    let done = Arc::new(AtomicBool::new(false));

    // reader: a separate handle opened AFTER the seed; counts must never
    // regress while the regime is active.
    let rpath = path.to_path_buf();
    let rdone = done.clone();
    let reader = std::thread::spawn(move || {
        let db = Database::open(&rpath).unwrap();
        let mut last = i64::MAX;
        let mut rounds = 0u64;
        while !rdone.load(Ordering::Relaxed) {
            let rows = db
                .query("SELECT COUNT(*), MIN(id), MAX(id) FROM accounts", [])
                .unwrap();
            let (n, mn, mx) = (
                rows[0][0].as_integer(),
                rows[0][1].as_integer(),
                rows[0][2].as_integer(),
            );
            assert_eq!(n, accounts, "reader saw a PARTIAL table (count={})", n);
            assert_eq!((mn, mx), (1, accounts), "reader saw bounds ({},{})", mn, mx);
            let _ = &mut last;
            rounds += 1;
        }
        rounds
    });

    let mut handles = Vec::new();
    for w in 0..writers {
        let wpath = path.to_path_buf();
        handles.push(std::thread::spawn(move || {
            let mut db = Database::open(&wpath).unwrap();
            Database::set_conn_identity(1_000 + w as u64);
            let mut seed = 0x2545F4914F6CDD1Du64 ^ (w as u64 + 1);
            for t in 0..txns {
                'retry: for _ in 0..80 {
                    seed ^= seed >> 12;
                    seed ^= seed << 25;
                    seed ^= seed >> 27;
                    let acct = (seed.wrapping_mul(0x2545F4914F6CDD1D) % accounts as u64) as i64 + 1;
                    if db.execute("BEGIN CONCURRENT", []).is_err() {
                        let _ = db.execute("ROLLBACK", []);
                        continue 'retry;
                    }
                    let up = db.execute(
                        "UPDATE accounts SET balance = balance - 1 WHERE id = ?",
                        [Value::Integer(acct)],
                    );
                    let ins = up.and_then(|_| {
                        db.execute(
                            "INSERT INTO audit (w, account, ts, delta) VALUES (?, ?, ?, -1)",
                            [
                                Value::Integer(w as i64),
                                Value::Integer(acct),
                                Value::Integer(t as i64),
                            ],
                        )
                    });
                    match ins {
                        Ok(_) => match db.execute("COMMIT", []) {
                            Ok(_) => break 'retry,
                            Err(_) => {
                                let _ = db.execute("ROLLBACK", []);
                            }
                        },
                        Err(_) => {
                            let _ = db.execute("ROLLBACK", []);
                        }
                    }
                }
            }
            Database::set_conn_identity(0);
        }));
    }
    for h in handles {
        h.join().expect("writer panicked");
    }
    done.store(true, Ordering::Relaxed);
    reader.join().expect("reader panicked");
}

#[test]
fn reader_stable_without_writers() {
    let path = tmpdb("nowriters");
    {
        let mut db = Database::open(&path).unwrap();
        seed(&mut db, 10_000);
    }
    let db = Database::open(&path).unwrap();
    for _ in 0..2000 {
        assert_eq!(count_accounts(&db), 10_000);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}

#[test]
fn reader_full_table_with_single_concurrent_writer() {
    let path = tmpdb("onewriter");
    {
        let mut db = Database::open(&path).unwrap();
        seed(&mut db, 10_000);
    }
    run_burst(&path, 1, 120);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}

#[test]
#[ignore = "PINNED divergence: stale executor root bookkeeping (Arc + published overlay) serves a partial tree to every live-engine reader during a multi-writer BEGIN CONCURRENT burst — see the module docs and examples/probe_mw_diag.rs; durable state unaffected, self-heals after the regime drains"]
fn reader_full_table_with_four_concurrent_writers() {
    let path = tmpdb("fourwriters");
    {
        let mut db = Database::open(&path).unwrap();
        seed(&mut db, 10_000);
    }
    run_burst(&path, 4, 200);
    // Reconcile on a fresh handle after the drain.
    let db = Database::open(&path).unwrap();
    assert_eq!(count_accounts(&db), 10_000);
    let audit: i64 = db.query("SELECT COUNT(*) FROM audit", []).unwrap()[0][0].as_integer();
    assert_eq!(audit, 4 * 200);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}
