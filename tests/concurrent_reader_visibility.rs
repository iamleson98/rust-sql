//! Reader-visibility under the BEGIN CONCURRENT regime.
//!
//! Invariant under test (SQLite's WAL contract, and this engine's own
//! documented contract — README "Concurrency"): while concurrent write
//! transactions are open or committing, every plain reader on the SAME
//! engine must observe a fully-committed, consistent table state. Row
//! counts of a table no transaction ever deleted from must never regress.
//!
//! The multi-connection model under test is the engine's supported one:
//! ONE shared engine (`Arc<RwLock<Database>>` — exactly what the sqlx
//! driver's per-path engine registry wraps) with N writer connections
//! (`Database::set_conn_identity`) whose statements interleave per
//! statement, plus a reader that runs under the engine read lock. This is
//! the shape pool-based applications actually run; concurrent
//! INDEPENDENT `Database::open` handles on one native file are outside
//! the engine's contract (the WAL writer lease rejects them with
//! SQLITE_BUSY instead of corrupting the sidecar — see
//! `storage::wal::acquire_wal_lease`).
//!
//! History — the 2026-09-19 pinned divergence, FIXED: during a
//! multi-writer burst, every live-engine reader could observe a
//! consistent PARTIAL table (COUNT(*) = 467 of 10000, missing point
//! reads, integrity ok — a self-consistent subtree). Root cause was a
//! bookkeeping wipe + regression chain, all in the engine-side root
//! maps:
//!
//! 1. A concurrent COMMIT that failed validation (BUSY_SNAPSHOT)
//!    deregisters the transaction BEFORE the error surfaces; the
//!    connection's cleanup `ROLLBACK` then fell through to the PLAIN
//!    rollback machinery, whose epilogue restored `txn_maps_snap` —
//!    `None` for concurrent transactions (their BEGIN intercepts before
//!    the plain machinery) — as `empty_maps()`. Every live root override
//!    the engine had learned since open was wiped.
//! 2. `table_root()` then fell back to the in-memory catalog's
//!    DDL-time root — after the seed's splits, an INTERIOR node; the
//!    descent served its subtree ([1..467]) as the whole table.
//! 3. Separately, `publish_concurrent_maps` swapped the committing
//!    transaction's BEGIN-time overlay in wholesale, regressing root
//!    entries sibling transactions had committed after it began.
//!
//! Fixes: the no-snapshot rollback keeps the current maps; the publish
//! merges only the transaction's DELTA (vs a recorded begin-time BASE)
//! with every root reconciled through the pager's committed-root view
//! (`Pager::committed_root_of`); the concurrent-owner schema-row sync
//! rewrites only the transaction's own root moves. Writer-progress
//! assertions below pin that the burst actually commits (the old
//! single-writer test could pass vacuously with zero committed
//! transactions).

use parking_lot::RwLock;
use rustqlite::{Database, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn tmpdb(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("cwvis-{}.db", name));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    path
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
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

/// One burst: `writers` connections (per-connection identities, statement
/// granularity locking — the driver's access shape) run payment-shaped
/// `BEGIN CONCURRENT` transactions against the SHARED engine while a
/// reader on the same engine continuously checks the table invariant.
/// Returns (reader_rounds, committed_transactions).
fn run_burst(db: &Arc<RwLock<Database>>, writers: usize, txns: u64) -> (u64, usize) {
    let accounts: i64 = 10_000;
    let done = Arc::new(AtomicBool::new(false));

    // Reader on the SHARED engine (the driver's read path: engine read
    // lock per query). Counts must never regress while the regime runs.
    let rdb = db.clone();
    let rdone = done.clone();
    let reader = std::thread::spawn(move || {
        let mut rounds = 0u64;
        while !rdone.load(Ordering::Relaxed) {
            let d = rdb.read();
            let rows = d
                .query("SELECT COUNT(*), MIN(id), MAX(id) FROM accounts", [])
                .unwrap();
            let (n, mn, mx) = (
                rows[0][0].as_integer(),
                rows[0][1].as_integer(),
                rows[0][2].as_integer(),
            );
            drop(d);
            assert_eq!(n, accounts, "reader saw a PARTIAL table (count={})", n);
            assert_eq!((mn, mx), (1, accounts), "reader saw bounds ({},{})", mn, mx);
            rounds += 1;
        }
        rounds
    });

    let committed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for w in 0..writers {
        let wdb = db.clone();
        let wdone = done.clone();
        let wcommitted = committed.clone();
        handles.push(std::thread::spawn(move || {
            Database::set_conn_identity(1_000 + w as u64);
            let mut seed = 0x2545F4914F6CDD1Du64 ^ (w as u64 + 1);
            for t in 0..txns {
                'retry: for _ in 0..80 {
                    if wdone.load(Ordering::Relaxed) {
                        break 'retry;
                    }
                    seed ^= seed >> 12;
                    seed ^= seed << 25;
                    seed ^= seed >> 27;
                    let acct = (seed.wrapping_mul(0x2545F4914F6CDD1D) % accounts as u64) as i64 + 1;
                    let r: rustqlite::Result<()> = (|| {
                        wdb.write().execute("BEGIN CONCURRENT", [])?;
                        wdb.write().execute(
                            "UPDATE accounts SET balance = balance - 1 WHERE id = ?",
                            [Value::Integer(acct)],
                        )?;
                        wdb.write().execute(
                            "INSERT INTO audit (w, account, ts, delta) VALUES (?, ?, ?, -1)",
                            [
                                Value::Integer(w as i64),
                                Value::Integer(acct),
                                Value::Integer(t as i64),
                            ],
                        )?;
                        wdb.write().execute("COMMIT", [])
                    })();
                    match r {
                        Ok(_) => {
                            wcommitted.fetch_add(1, Ordering::Relaxed);
                            break 'retry;
                        }
                        Err(_) => {
                            let _ = wdb.write().execute("ROLLBACK", []);
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
    let rounds = reader.join().expect("reader panicked");
    (rounds, committed.load(Ordering::Relaxed))
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
    cleanup(&path);
}

#[test]
fn reader_full_table_with_single_concurrent_writer() {
    let path = tmpdb("onewriter");
    let db = {
        let mut d = Database::open(&path).unwrap();
        seed(&mut d, 10_000);
        Arc::new(RwLock::new(d))
    };
    let (rounds, committed) = run_burst(&db, 1, 120);
    // Progress pin: the burst must actually commit (the pre-fix test
    // could pass vacuously with every transaction silently failing).
    assert!(committed >= 100, "only {committed}/120 txns committed");
    assert!(rounds > 0);
    let d = db.read();
    assert_eq!(count_accounts(&d), 10_000);
    let audit: i64 = d.query("SELECT COUNT(*) FROM audit", []).unwrap()[0][0].as_integer();
    drop(d);
    assert_eq!(audit, committed as i64, "audit rows must equal commits");
    drop(db);
    // Reconcile on a fresh handle after the drain.
    let db = Database::open(&path).unwrap();
    assert_eq!(count_accounts(&db), 10_000);
    drop(db);
    cleanup(&path);
}

#[test]
fn reader_full_table_with_four_concurrent_writers() {
    let path = tmpdb("fourwriters");
    let db = {
        let mut d = Database::open(&path).unwrap();
        seed(&mut d, 10_000);
        Arc::new(RwLock::new(d))
    };
    let (rounds, committed) = run_burst(&db, 4, 200);
    // Every transaction commits (row-granularity merges resolve page
    // conflicts); a degraded environment may retry but must not fail.
    assert!(
        committed >= 4 * 150,
        "only {committed}/{} txns committed",
        4 * 200
    );
    assert!(rounds > 0);
    let d = db.read();
    assert_eq!(count_accounts(&d), 10_000);
    let audit: i64 = d.query("SELECT COUNT(*) FROM audit", []).unwrap()[0][0].as_integer();
    let ic: String = d.query("PRAGMA integrity_check", []).unwrap()[0][0]
        .as_text()
        .to_string();
    drop(d);
    assert_eq!(audit, committed as i64, "audit rows must equal commits");
    assert_eq!(ic, "ok");
    drop(db);
    // Reconcile on a fresh handle after the drain (durable state).
    let db = Database::open(&path).unwrap();
    assert_eq!(count_accounts(&db), 10_000);
    let audit: i64 = db.query("SELECT COUNT(*) FROM audit", []).unwrap()[0][0].as_integer();
    drop(db);
    assert_eq!(audit, committed as i64);
    cleanup(&path);
}

#[test]
fn wal_mode_persists_across_reopen() {
    // SQLite parity: journal_mode=WAL is persistent — a clean close
    // checkpoints + removes the sidecar, but a fresh open of the file
    // comes up in WAL mode (and BEGIN CONCURRENT works immediately).
    // Before the fix, every fresh handle silently reverted to DELETE
    // mode and BEGIN CONCURRENT failed with "requires journal_mode=WAL".
    let path = tmpdb("walpersist");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO t (v) VALUES ('a')", []).unwrap();
        assert_eq!(
            db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(),
            "wal"
        );
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(),
        "wal",
        "journal_mode=WAL must persist across reopen (SQLite semantics)"
    );
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    // BEGIN CONCURRENT works on the reopened handle without re-issuing
    // the pragma.
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('b')", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    // ...and switching back to DELETE persists too.
    db.execute("PRAGMA journal_mode = DELETE", []).unwrap();
    assert_eq!(
        db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(),
        "delete"
    );
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.query("PRAGMA journal_mode", []).unwrap()[0][0].as_text(),
        "delete",
        "journal_mode=DELETE must persist across reopen"
    );
    drop(db);
    cleanup(&path);
}

#[test]
fn concurrent_second_writer_handle_gets_busy_not_corruption() {
    // Two INDEPENDENT Database handles on one native file: the WAL
    // writer lease must reject the second handle's writes with a busy
    // error (never corrupt the sidecar). The first handle's data stays
    // intact and the second handle's reads keep working.
    let path = tmpdb("lease");
    {
        let mut a = Database::open(&path).unwrap();
        a.execute("PRAGMA journal_mode = WAL", []).unwrap();
        a.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        a.execute("INSERT INTO t (v) VALUES ('one')", []).unwrap();
        let mut b = Database::open(&path).unwrap();
        // b reads fine (WAL frames are plain positioned reads).
        let rows = b.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(rows[0][0].as_integer(), 1);
        // b's write is rejected while a holds the write lease.
        let r = b.execute("INSERT INTO t (v) VALUES ('two')", []);
        let msg = r.err().unwrap().to_string();
        assert!(
            msg.contains("SQLITE_BUSY") && msg.contains("lease"),
            "expected lease busy error, got: {msg}"
        );
        // a keeps writing and reading.
        a.execute("INSERT INTO t (v) VALUES ('three')", []).unwrap();
        let rows = a.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(rows[0][0].as_integer(), 2);
        drop(a);
    }
    // After the owner retires, the next handle takes the lease cleanly.
    let mut c = Database::open(&path).unwrap();
    let rows = c.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    c.execute("INSERT INTO t (v) VALUES ('four')", []).unwrap();
    drop(c);
    let db = Database::open(&path).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 3);
    drop(db);
    cleanup(&path);
}

#[test]
fn wal_writer_lease_releases_through_symlinked_dir() {
    // The lease-key stability pin for the CI mac+win failures: the key
    // is derived from the canonicalized PARENT directory, never the
    // sidecar file itself. Pager::drop removes the sidecar BEFORE its
    // final lease release — keying on the file made canonicalize fail
    // post-removal and fall back to the RAW path, which differs from
    // the acquisition key whenever the database directory is reached
    // through a symlink (macOS `/var` -> `/private/var`) or a device
    // path (Windows `\\?\` prefix): the release missed the entry, the
    // lease leaked, and every later handle on that path got SQLITE_BUSY
    // forever. Linux CI is immune (`/tmp` is real), so this test
    // reproduces the divergent-raw-vs-canonical shape explicitly via a
    // symlinked directory — before the fix it fails with the lease busy
    // error on the reopened handle's write.
    let real = std::env::temp_dir().join(format!("cwvis-leasereal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&real);
    std::fs::create_dir_all(&real).unwrap();
    #[cfg(unix)]
    let link = {
        let link = std::env::temp_dir().join(format!("cwvis-leaselink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        link
    };
    #[cfg(unix)]
    let via = link;
    #[cfg(not(unix))]
    let via = real.clone(); // symlink rights vary on Windows; the \\?\
                            // prefix keeps raw != canonical there anyway
    let path = via.join("lease-sym.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO t (v) VALUES ('one')", []).unwrap();
        // The raw and canonical forms differ here by construction —
        // prove the lease was taken at all through the real directory.
        assert!(real.join("lease-sym.db-wal").exists());
    }
    // Reopen through the SAME divergent path: the dropped owner's lease
    // must be gone — this write was the BUSY failure before the fix.
    let mut db = Database::open(&path).unwrap();
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('two')", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(real.join("lease-sym.db-wal"));
    let _ = std::fs::remove_dir_all(&real);
    #[cfg(unix)]
    let _ = std::fs::remove_file(
        std::env::temp_dir().join(format!("cwvis-leaselink-{}", std::process::id())),
    );
}
