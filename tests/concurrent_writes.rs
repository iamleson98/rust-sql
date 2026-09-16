//! Multi-writer concurrency tests: `BEGIN CONCURRENT` transactions with
//! page-level optimistic conflict detection (see `storage/concurrent.rs`).
//!
//! Engine-level tests drive two "connections" through the connection-
//! identity API (`Database::set_conn_identity`) — the same mechanism the
//! sqlx driver uses — against one `Database`, interleaving their
//! statements exactly like pool connections would. Driver-level tests
//! (sqlx feature) exercise the real pool/connection surface.
//!
//! The core properties under test:
//!   1. N transactions open at once, interleaved statements, each seeing
//!      its own writes and the BEGIN-time committed state of everything
//!      else (snapshot isolation).
//!   2. Disjoint write sets (different tables / regions) commit without
//!      conflict — the throughput win over single-writer WAL.
//!   3. Overlapping write sets conflict: first committer wins, the loser
//!      gets SQLITE_BUSY_SNAPSHOT and is fully rolled back.
//!   4. Readers never block and never observe uncommitted rows.
//!   5. ROLLBACK discards everything (the live page cache was never
//!      touched), recycles the transaction's fresh pages, and leaves a
//!      durable, consistent database.
//!   6. WAL crash recovery replays concurrent commits.

use rustqlite::{Database, Value};

fn tmpdb(name: &str) -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("{}.db", name));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    let db = Database::open(&path).unwrap();
    (db, path)
}

fn cleanup(path: &std::path::PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
}

fn waldb(name: &str) -> (Database, std::path::PathBuf) {
    let (mut db, path) = tmpdb(name);
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    (db, path)
}

fn count(db: &Database, table: &str) -> i64 {
    db.query(&format!("SELECT count(*) FROM {}", table), [])
        .unwrap()[0][0]
        .as_integer()
}

// ---------------------------------------------------------------------------
// basics
// ---------------------------------------------------------------------------

#[test]
fn concurrent_basic_commit_and_read() {
    let (mut db, path) = waldb("cw_basic");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('a')", []).unwrap();
    // Own writes visible inside the transaction.
    assert_eq!(count(&db, "t"), 1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 1);
    // Durable: reopen and read back through WAL recovery.
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 1);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_rollback_discards() {
    let (mut db, path) = waldb("cw_rollback");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES ('seed')", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('gone')", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('gone2')", [])
        .unwrap();
    assert_eq!(count(&db, "t"), 3);
    db.execute("ROLLBACK", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 1);
    // Durable state after the rollback's recycle flush.
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 1);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_requires_wal_mode() {
    let (mut db, path) = tmpdb("cw_nowal");
    // DELETE journal mode (the default for a fresh file DB).
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    let r = db.execute("BEGIN CONCURRENT", []);
    assert!(r.is_err(), "BEGIN CONCURRENT must require WAL mode");
    let msg = r.err().unwrap().to_string();
    assert!(
        msg.contains("WAL"),
        "error should explain the WAL requirement: {}",
        msg
    );
    // A plain BEGIN still works.
    db.execute("BEGIN", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    cleanup(&path);
}

#[test]
fn concurrent_ddl_and_savepoint_rejected() {
    let (mut db, path) = waldb("cw_ddl");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let ddl = db.execute("CREATE TABLE u (a INT)", []);
    assert!(ddl.is_err());
    assert!(ddl.err().unwrap().to_string().contains("CONCURRENT"));
    let sp = db.execute("SAVEPOINT s1", []);
    assert!(sp.is_err());
    let pragma = db.execute("PRAGMA synchronous = FULL", []);
    assert!(pragma.is_err());
    // The transaction is still usable for DML after the rejections.
    db.execute("INSERT INTO t (id) VALUES (1)", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 1);
    cleanup(&path);
}

#[test]
fn concurrent_begin_requires_no_plain_txn() {
    let (mut db, path) = waldb("cw_begin_mix");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    let r = db.execute("BEGIN CONCURRENT", []);
    assert!(r.is_err());
    db.execute("COMMIT", []).unwrap();
    // And the reverse: no plain BEGIN while a concurrent txn is open.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let r2 = db.execute("BEGIN", []);
    assert!(r2.is_err());
    db.execute("ROLLBACK", []).unwrap();
    Database::set_conn_identity(0);
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// isolation + the multi-writer win
// ---------------------------------------------------------------------------

#[test]
fn concurrent_two_writers_disjoint_tables_both_commit() {
    let (mut db, path) = waldb("cw_disjoint");
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    // Both transactions open at once, statements interleaved.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("INSERT INTO a (v) VALUES ('a1')", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("INSERT INTO b (v) VALUES ('b1')", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("INSERT INTO a (v) VALUES ('a2')", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("INSERT INTO b (v) VALUES ('b2')", []).unwrap();
    // Each sees ONLY its own writes (snapshot isolation).
    Database::set_conn_identity(1);
    assert_eq!(count(&db, "a"), 2);
    assert_eq!(count(&db, "b"), 0);
    Database::set_conn_identity(2);
    assert_eq!(count(&db, "b"), 2);
    assert_eq!(count(&db, "a"), 0);
    // Both commit — the single-writer model would have serialized these.
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "a"), 2);
    assert_eq!(count(&db, "b"), 2);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "a"), 2);
    assert_eq!(count(&db2, "b"), 2);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_reader_never_sees_uncommitted() {
    let (mut db, path) = waldb("cw_reader");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES ('seed')", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('uncommitted')", [])
        .unwrap();
    // A reader on ANOTHER identity sees only the committed state —
    // without blocking (the live cache IS the committed state in the
    // concurrent regime).
    Database::set_conn_identity(7);
    assert_eq!(count(&db, "t"), 1);
    Database::set_conn_identity(1);
    // The owner still sees its own write.
    assert_eq!(count(&db, "t"), 2);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(7);
    assert_eq!(count(&db, "t"), 2);
    Database::set_conn_identity(0);
    cleanup(&path);
}

#[test]
fn concurrent_same_hot_page_conflict_first_committer_wins() {
    let (mut db, path) = waldb("cw_conflict");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    // Two transactions over the SAME small table (one hot leaf page).
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("INSERT INTO t (v) VALUES ('first')", [])
        .unwrap();
    Database::set_conn_identity(2);
    db.execute("INSERT INTO t (v) VALUES ('second')", [])
        .unwrap();
    // First committer wins.
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    // The loser conflicts at COMMIT — SQLITE_BUSY_SNAPSHOT semantics —
    // and is fully rolled back.
    Database::set_conn_identity(2);
    let r = db.execute("COMMIT", []);
    assert!(r.is_err(), "the second committer must conflict");
    let msg = r.err().unwrap().to_string();
    assert!(
        msg.contains("SQLITE_BUSY_SNAPSHOT"),
        "error must carry SQLITE_BUSY_SNAPSHOT: {}",
        msg
    );
    // Only the first writer's row survived.
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 1);
    let rows = db.query("SELECT v FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "first");
    // The conflicting connection can immediately start a NEW transaction
    // and retry (the regime was cleanly torn down).
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('retry')", [])
        .unwrap();
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 2);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 2);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_fail_fast_when_page_moved_after_begin() {
    let (mut db, path) = waldb("cw_failfast");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES ('seed')", []).unwrap();
    // W2 begins, THEN W1 commits a write to a page W2 later needs: the
    // fetch fails fast with the snapshot-conflict error.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('w1')", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let r = db.query("SELECT v FROM t", []);
    assert!(
        r.is_err(),
        "fetch after a conflicting commit must fail fast"
    );
    assert!(r
        .err()
        .unwrap()
        .to_string()
        .contains("SQLITE_BUSY_SNAPSHOT"));
    // The failed statement aborted the whole transaction (v1 semantics:
    // no partial statement state in shadows) — a retry works.
    let r2 = db.execute("ROLLBACK", []);
    assert!(r2.is_ok() || r2.err().unwrap().to_string().contains("not open"));
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    let rows = db.query("SELECT v FROM t", []).unwrap();
    assert_eq!(rows.len(), 2);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    cleanup(&path);
}

#[test]
fn concurrent_plain_writes_wait_for_the_regime() {
    let (mut db, path) = waldb("cw_plain_gate");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES ('c')", []).unwrap();
    // A plain autocommit write from another identity is excluded while
    // the regime is active (the live cache must stay committed-clean).
    Database::set_conn_identity(2);
    let r = db.execute("INSERT INTO t (v) VALUES ('plain')", []);
    assert!(r.is_err());
    assert!(r.err().unwrap().to_string().contains("SQLITE_BUSY"));
    // Plain READS still pass.
    assert_eq!(count(&db, "t"), 0);
    // After COMMIT the plain writer proceeds.
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("INSERT INTO t (v) VALUES ('plain')", [])
        .unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 2);
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// tree surgery, allocation recycling, durability
// ---------------------------------------------------------------------------

#[test]
fn concurrent_bulk_insert_splits_and_recycles() {
    let (mut db, path) = waldb("cw_bulk");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    // Enough rows to split the B+tree many times (fresh page
    // allocations inside the transaction).
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    for i in 0..2000i64 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("row-{}", i).into())],
        )
        .unwrap();
    }
    assert_eq!(count(&db, "t"), 2000);
    db.execute("ROLLBACK", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 0);
    // The database must still be healthy: fresh plain inserts, reopen,
    // and a full re-read.
    db.execute("BEGIN", []).unwrap();
    for i in 0..500i64 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("after-{}", i).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    assert_eq!(count(&db, "t"), 500);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 500);
    let rows = db2.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "after-0");
    assert_eq!(rows[499][0].as_text(), "after-499");
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_bulk_insert_commits_durable() {
    let (mut db, path) = waldb("cw_bulk_commit");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    for i in 0..2000i64 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("row-{}", i).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    drop(db);
    // WAL recovery replays the concurrent commit.
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 2000);
    let rows = db2.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "row-0");
    assert_eq!(rows[1999][0].as_text(), "row-1999");
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_update_and_delete() {
    let (mut db, path) = waldb("cw_update_delete");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 0..50i64 {
        db.execute(
            "INSERT INTO t (v) VALUES (?)",
            [Value::Text(format!("v{}", i).into())],
        )
        .unwrap();
    }
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'upd' WHERE id % 2 = 0", [])
        .unwrap();
    db.execute("DELETE FROM t WHERE id > 40", []).unwrap();
    assert_eq!(count(&db, "t"), 40);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 40);
    let rows = db.query("SELECT v FROM t WHERE id = 2", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "upd");
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 40);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_integrity_check_after_mixed_workload() {
    let (mut db, path) = waldb("cw_integrity");
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    // A mix of committing, conflicting, and rolled-back concurrent
    // transactions plus plain autocommit writes in between.
    for round in 0..5i64 {
        Database::set_conn_identity(1);
        db.execute("BEGIN CONCURRENT", []).unwrap();
        db.execute(
            "INSERT INTO a (v) VALUES (?)",
            [Value::Text(format!("a{}", round).into())],
        )
        .unwrap();
        Database::set_conn_identity(2);
        db.execute("BEGIN CONCURRENT", []).unwrap();
        db.execute(
            "INSERT INTO b (v) VALUES (?)",
            [Value::Text(format!("b{}", round).into())],
        )
        .unwrap();
        // A third transaction over table a conflicts with identity 1.
        Database::set_conn_identity(3);
        db.execute("BEGIN CONCURRENT", []).unwrap();
        db.execute(
            "INSERT INTO a (v) VALUES (?)",
            [Value::Text(format!("x{}", round).into())],
        )
        .unwrap();
        Database::set_conn_identity(1);
        db.execute("COMMIT", []).unwrap();
        Database::set_conn_identity(2);
        db.execute("COMMIT", []).unwrap();
        Database::set_conn_identity(3);
        let conflicted = db.execute("COMMIT", []);
        assert!(conflicted.is_err());
        // Plain writer between rounds.
        Database::set_conn_identity(0);
        db.execute("INSERT INTO a (v) VALUES ('plain')", [])
            .unwrap();
        db.execute("INSERT INTO b (v) VALUES ('plain')", [])
            .unwrap();
    }
    assert_eq!(count(&db, "a"), 5 + 5);
    assert_eq!(count(&db, "b"), 5 + 5);
    // Full durability + integrity after reopen.
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "a"), 10);
    assert_eq!(count(&db2, "b"), 10);
    drop(db2);
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// true parallelism: interleaved threads under the outer RwLock
// ---------------------------------------------------------------------------

#[test]
fn concurrent_parallel_writer_threads_disjoint_tables() {
    let (mut db, path) = waldb("cw_threads");
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    drop(db);
    let state = std::sync::Arc::new(parking_lot::RwLock::new({
        let mut db = Database::open(&path).unwrap();
        // WAL mode is probed from the -wal sidecar at reopen; re-arm it
        // explicitly (a clean close may have checkpointed it away).
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db
    }));
    let n = 200i64;
    let handles: Vec<_> = (1..=2i64)
        .map(|w| {
            let state = std::sync::Arc::clone(&state);
            std::thread::spawn(move || {
                let table = if w == 1 { "a" } else { "b" };
                // Each thread IS its own connection identity.
                Database::set_conn_identity(w as u64);
                let mut guard = state.write();
                guard.execute("BEGIN CONCURRENT", []).unwrap();
                for i in 0..n {
                    guard
                        .execute(
                            &format!("INSERT INTO {} (v) VALUES (?)", table),
                            [Value::Text(format!("w{}-{}", w, i).into())],
                        )
                        .unwrap();
                }
                guard.execute("COMMIT", []).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    Database::set_conn_identity(0);
    {
        let db = state.read();
        assert_eq!(count(&db, "a"), n);
        assert_eq!(count(&db, "b"), n);
    }
    drop(state);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "a"), n);
    assert_eq!(count(&db2, "b"), n);
    drop(db2);
    cleanup(&path);
}

// ---------------------------------------------------------------------------
// sqlx driver surface (real connections)
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlx")]
mod driver {
    use rustqlite::sqlx_driver::RustqliteConnectOptions;
    use sqlx::{Connection, Executor, Row};

    fn file_opts(name: &str) -> RustqliteConnectOptions {
        let path = std::env::temp_dir().join(format!("cwdrv-{}.db", name));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
        RustqliteConnectOptions::filename(path).create_if_missing(true)
    }

    fn cleanup(name: &str) {
        let path = std::env::temp_dir().join(format!("cwdrv-{}.db", name));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.to_str().unwrap()));
    }

    #[tokio::test]
    async fn driver_two_connections_concurrent_transactions() {
        let opts = file_opts("two");
        let mut c1 = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        let mut c2 = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        c1.execute("PRAGMA journal_mode = WAL").await.unwrap();
        // Schema outside the concurrent transactions (DDL is rejected
        // inside them — v1 restriction).
        c1.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
        c1.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
        // Both transactions open SIMULTANEOUSLY — impossible under the
        // single-writer model — with interleaved DML on disjoint tables.
        c1.execute("BEGIN CONCURRENT").await.unwrap();
        c2.execute("BEGIN CONCURRENT").await.unwrap();
        for _ in 0..50i64 {
            c1.execute("INSERT INTO a (v) VALUES ('a')").await.unwrap();
            c2.execute("INSERT INTO b (v) VALUES ('b')").await.unwrap();
        }
        // Each connection's reads see only its own writes.
        let (ra, rb): (i64, i64) =
            sqlx::query("SELECT (SELECT count(*) FROM a), (SELECT count(*) FROM b)")
                .fetch_one(&mut c1)
                .await
                .map(|r| (r.get::<i64, _>(0), r.get::<i64, _>(1)))
                .unwrap();
        assert_eq!((ra, rb), (50, 0));
        c1.execute("COMMIT").await.unwrap();
        c2.execute("COMMIT").await.unwrap();
        // Both committed.
        let (ra, rb): (i64, i64) =
            sqlx::query("SELECT (SELECT count(*) FROM a), (SELECT count(*) FROM b)")
                .fetch_one(&mut c1)
                .await
                .map(|r| (r.get::<i64, _>(0), r.get::<i64, _>(1)))
                .unwrap();
        assert_eq!((ra, rb), (50, 50));
        c1.close().await.unwrap();
        c2.close().await.unwrap();
        cleanup("two");
    }

    #[tokio::test]
    async fn driver_conflict_maps_to_busy_snapshot() {
        let opts = file_opts("conflict");
        let mut c1 = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        let mut c2 = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        c1.execute("PRAGMA journal_mode = WAL").await.unwrap();
        c1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
        c1.execute("BEGIN CONCURRENT").await.unwrap();
        c2.execute("BEGIN CONCURRENT").await.unwrap();
        c1.execute("INSERT INTO t (v) VALUES ('first')")
            .await
            .unwrap();
        c2.execute("INSERT INTO t (v) VALUES ('second')")
            .await
            .unwrap();
        c1.execute("COMMIT").await.unwrap();
        let r = c2.execute("COMMIT").await;
        assert!(r.is_err());
        let err = r.err().unwrap();
        let db_err = err
            .as_database_error()
            .expect("conflict must surface as a database error");
        assert_eq!(db_err.code().unwrap().as_ref(), "517"); // SQLITE_BUSY_SNAPSHOT
                                                            // Retry after the conflict succeeds.
        c2.execute("BEGIN CONCURRENT").await.unwrap();
        c2.execute("INSERT INTO t (v) VALUES ('retry')")
            .await
            .unwrap();
        c2.execute("COMMIT").await.unwrap();
        let n: i64 = sqlx::query("SELECT count(*) FROM t")
            .fetch_one(&mut c1)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 2);
        c1.close().await.unwrap();
        c2.close().await.unwrap();
        cleanup("conflict");
    }

    #[tokio::test]
    async fn driver_reader_unblocked_during_concurrent_txn() {
        let opts = file_opts("reader");
        let mut w = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        let mut r = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        w.execute("PRAGMA journal_mode = WAL").await.unwrap();
        w.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
        w.execute("INSERT INTO t (v) VALUES ('seed')")
            .await
            .unwrap();
        w.execute("BEGIN CONCURRENT").await.unwrap();
        w.execute("INSERT INTO t (v) VALUES ('uncommitted')")
            .await
            .unwrap();
        // The reader proceeds WITHOUT waiting and sees committed state.
        let n: i64 = sqlx::query("SELECT count(*) FROM t")
            .fetch_one(&mut r)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 1);
        w.execute("COMMIT").await.unwrap();
        let n2: i64 = sqlx::query("SELECT count(*) FROM t")
            .fetch_one(&mut r)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n2, 2);
        w.close().await.unwrap();
        r.close().await.unwrap();
        cleanup("reader");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_group_commit_amortizes_fsync() {
        let opts = file_opts("group");
        let mut setup = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        setup.execute("PRAGMA journal_mode = WAL").await.unwrap();
        setup
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .await
            .unwrap();
        // High-durability bursts: synchronous=FULL makes every COMMIT's
        // durability point an fsync. Four connections open concurrent
        // transactions on parallel threads, and their COMMITs are
        // barrier-aligned so they arrive together — the group-commit
        // leader must coalesce them into fewer fsyncs than commits.
        setup.execute("PRAGMA synchronous = FULL").await.unwrap();
        setup.close().await.unwrap();
        // Open all four connections up front (the pooled production
        // pattern: connections live before the burst), then run each
        // transaction on its own thread.
        let mut conns: Vec<_> = (0..4)
            .map(|_| rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap())
            .collect();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut joins = Vec::new();
        for (i, c) in conns.drain(..).enumerate() {
            let barrier = barrier.clone();
            joins.push(std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let mut c = c;
                    c.execute("BEGIN CONCURRENT").await.unwrap();
                    sqlx::query("INSERT INTO t (id, v) VALUES (?, 'x')")
                        .bind(i as i64)
                        .execute(&mut c)
                        .await
                        .unwrap();
                    barrier.wait();
                    c.execute("COMMIT").await.unwrap();
                    c.close().await.unwrap();
                });
            }));
        }
        for j in joins {
            j.join().expect("worker thread panicked");
        }
        // All four committed...
        let mut c = rustqlite::sqlx_driver::RustqliteConnection::open(&opts).unwrap();
        let n: i64 = sqlx::query("SELECT count(*) FROM t")
            .fetch_one(&mut c)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 4);
        // ...and the fsyncs were AMORTIZED: fewer group syncs than group
        // commits (the leader covers every sibling that appended in its
        // coalescing window / during its in-flight fsync).
        let (syncs, commits) = c.pager_handle().group_commit_stats();
        assert!(commits >= 4, "stats: {}", commits);
        assert!(
            syncs < commits,
            "expected amortized fsyncs, got syncs={} commits={}",
            syncs,
            commits
        );
        c.close().await.unwrap();
        // Durable: a fresh engine replays the WAL and sees all four rows.
        let path = std::env::temp_dir().join("cwdrv-group.db");
        let db = rustqlite::Database::open(&path).unwrap();
        let n = db.query("SELECT count(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 4);
        drop(db);
        cleanup("group");
    }
}

// ---------------------------------------------------------------------------
// ROW-LEVEL conflict granularity (v2): two writers on DIFFERENT rows of
// the SAME hot leaf page both commit — the second MERGES (replays its row
// journal onto the current committed trees) instead of retrying.
// ---------------------------------------------------------------------------

#[test]
fn concurrent_hot_page_disjoint_rows_merge() {
    let (mut db, path) = waldb("cw_row_merge_insert");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    // Two transactions inserting DIFFERENT rowids of one hot leaf page.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("INSERT INTO t (id, v) VALUES (1, 'a')", [])
        .unwrap();
    Database::set_conn_identity(2);
    db.execute("INSERT INTO t (id, v) VALUES (2, 'b')", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    // Page-granularity would abort here (both dirtied the same leaf);
    // row-granularity MERGES the second writer's journal.
    Database::set_conn_identity(2);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 2);
    let rows = db.query("SELECT id, v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][1].as_text(), "a");
    assert_eq!(rows[1][1].as_text(), "b");
    // Durable + tree-consistent after WAL recovery.
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 2);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_hot_page_disjoint_row_updates_merge() {
    let (mut db, path) = waldb("cw_row_merge_update");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'x'), (2, 'y')", [])
        .unwrap();
    // Both writers UPDATE different rows (same single-leaf table).
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'A' WHERE id = 1", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'B' WHERE id = 2", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("COMMIT", []).unwrap(); // MERGE path
    Database::set_conn_identity(0);
    let rows = db.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "A");
    assert_eq!(rows[1][0].as_text(), "B");
    assert_eq!(count(&db, "t"), 2);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    let rows = db2.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "A");
    assert_eq!(rows[1][0].as_text(), "B");
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_hot_page_same_row_update_conflicts() {
    let (mut db, _path) = waldb("cw_row_conflict_update");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'x'), (2, 'y')", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'A' WHERE id = 1", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'B' WHERE id = 1", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    // SAME row: first-committer-wins at row granularity — retryable
    // SQLITE_BUSY_SNAPSHOT.
    Database::set_conn_identity(2);
    let r = db.execute("COMMIT", []);
    assert!(r.is_err(), "same-row update must conflict");
    assert!(r
        .err()
        .unwrap()
        .to_string()
        .contains("SQLITE_BUSY_SNAPSHOT"));
    Database::set_conn_identity(0);
    let rows = db.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "A");
    assert_eq!(rows[1][0].as_text(), "y");
}

#[test]
fn concurrent_disjoint_deletes_merge() {
    let (mut db, path) = waldb("cw_row_merge_delete");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'x'), (2, 'y')", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("DELETE FROM t WHERE id = 1", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("DELETE FROM t WHERE id = 2", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("COMMIT", []).unwrap(); // MERGE path (delete journal)
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 0);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 0);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_merge_with_index_maintenance() {
    let (mut db, path) = waldb("cw_row_merge_index");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INT)", [])
        .unwrap();
    db.execute("CREATE INDEX ik ON t(k)", []).unwrap();
    db.execute(
        "INSERT INTO t (id, k) VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
        [],
    )
    .unwrap();
    // Both writers update DIFFERENT rows' indexed column: table-leaf ops
    // AND index-leaf ops merge.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET k = 11 WHERE id = 1", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET k = 22 WHERE id = 2", []).unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("COMMIT", []).unwrap(); // MERGE: table + index journals
    Database::set_conn_identity(0);
    // Index-driven lookups must reflect BOTH merges.
    for (k, id) in [(11i64, 1i64), (22, 2), (30, 3), (40, 4)] {
        let rows = db
            .query("SELECT id FROM t WHERE k = ?", [Value::Integer(k)])
            .unwrap();
        assert_eq!(rows.len(), 1, "k={} must find exactly one row", k);
        assert_eq!(rows[0][0].as_integer(), id);
    }
    // The stale keys must be gone from the index.
    assert_eq!(
        db.query("SELECT count(*) FROM t WHERE k = 10", []).unwrap()[0][0].as_integer(),
        0
    );
    drop(db);
    let db2 = Database::open(&path).unwrap();
    let rows = db2
        .query("SELECT id FROM t WHERE k IN (11, 22) ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_merge_with_root_split_rebuilds_tree() {
    let (mut db, path) = waldb("cw_row_merge_split");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'seed')", [])
        .unwrap();
    // Writer 2 bulk-inserts enough rows to SPLIT the (single-leaf) tree
    // and move its root; writer 1 touches the same original leaf first
    // and commits — writer 2 must MERGE: discard its split shadows,
    // replay the journal (re-splitting naturally), and land a correct
    // catalog rootpage.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'touched' WHERE id = 1", [])
        .unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    for i in 2..400i64 {
        db.execute(
            "INSERT INTO t (id, v) VALUES (?, ?)",
            [Value::Integer(i), Value::Text(format!("r{}", i).into())],
        )
        .unwrap();
    }
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("COMMIT", []).unwrap(); // MERGE with re-rooting
    Database::set_conn_identity(0);
    assert_eq!(count(&db, "t"), 399);
    let rows = db.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "touched");
    // Integrity: every row reachable in key order (a broken root or a
    // stale catalog rootpage would misplace or lose rows).
    let rows = db.query("SELECT id FROM t ORDER BY id", []).unwrap();
    let mut expect = 1i64;
    for r in &rows {
        assert_eq!(r[0].as_integer(), expect);
        expect += 1;
    }
    assert_eq!(expect, 400);
    drop(db);
    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2, "t"), 399);
    let rows = db2.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "touched");
    drop(db2);
    cleanup(&path);
}

#[test]
fn concurrent_merge_conflict_still_first_committer_wins_on_row() {
    let (mut db, _path) = waldb("cw_row_merge_vs_conflict");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (1, 'x'), (2, 'y')", [])
        .unwrap();
    // MIXED: writer 2 updates row 2 (mergeable) AND row 1 (conflicting
    // with writer 1's committed update) — the row-level validation must
    // abort the whole transaction, never a partial merge.
    Database::set_conn_identity(1);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'A' WHERE id = 1", []).unwrap();
    Database::set_conn_identity(2);
    db.execute("BEGIN CONCURRENT", []).unwrap();
    db.execute("UPDATE t SET v = 'B' WHERE id = 2", []).unwrap();
    db.execute("UPDATE t SET v = 'B2' WHERE id = 1", [])
        .unwrap();
    Database::set_conn_identity(1);
    db.execute("COMMIT", []).unwrap();
    Database::set_conn_identity(2);
    let r = db.execute("COMMIT", []);
    assert!(r.is_err(), "same-row component must abort the merge");
    assert!(r
        .err()
        .unwrap()
        .to_string()
        .contains("SQLITE_BUSY_SNAPSHOT"));
    Database::set_conn_identity(0);
    let rows = db.query("SELECT v FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "A");
    assert_eq!(rows[1][0].as_text(), "y");
}
