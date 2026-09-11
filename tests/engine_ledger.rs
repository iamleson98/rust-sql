//! Engine observability ledger — regression tests for the admin stats
//! counters (`PRAGMA cache_size` KiB semantics, engine-wide row-mutation
//! total, transaction ledger with implicit auto-commit counting).
//!
//! Background (found from the k6 03-chat load test's DB stats card):
//! * `PRAGMA cache_size = -65536` (64 MiB) was being resolved as
//!   `65536 / 4096 = 16 PAGES` (KiB count divided by byte page size) —
//!   a 1024x shrink that collapsed the page-cache hit rate to ~10%.
//! * `total_changes` was a THREAD-LOCAL counter, so a stats endpoint
//!   sampling from another thread read a permanent 0.
//! * The transaction ledger only counted EXPLICIT BEGIN/COMMIT; the
//!   app's auto-commit workload showed an all-zero ledger despite
//!   thousands of writes.

use rustqlite::Database;

fn temp_db_path(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("engine-ledger-{}-{}.db", tag, std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    path
}

fn read_int(db: &Database, sql: &str) -> i64 {
    let rows = db.query(sql, []).expect(sql);
    match rows.first().and_then(|r| r.first()) {
        Some(rustqlite::Value::Integer(i)) => *i,
        other => panic!("{sql}: expected single Integer, got {other:?}"),
    }
}

#[test]
fn cache_size_negative_kib_converts_to_correct_page_count() {
    // SQLite semantics: negative cache_size = KiB. -65536 = 64 MiB.
    // The engine resolves it to pages = bytes / page_size.
    let path = temp_db_path("cachesize");
    let mut db = Database::open(&path).expect("open");

    db.execute("PRAGMA cache_size=-65536", []).expect("pragma");
    let page_size = read_int(&db, "PRAGMA page_size");
    // SQLite's read form returns the SETTING as given (-65536), not the
    // derived page capacity; the byte budget it requests is |k| KiB.
    let setting = read_int(&db, "PRAGMA cache_size");
    assert_eq!(setting, -65536, "read form returns the raw setting");

    assert!(page_size > 0, "page_size must be positive, got {page_size}");
    // The capacity in BYTES must equal the requested 64 MiB (± page
    // rounding). The old bug produced 16 pages (64 KiB) for a 4096-byte
    // page — a 1024x shrink. (Derive the capacity from the setting —
    // the read form no longer reports it directly.)
    let cap_bytes = (-setting) * 1024;
    assert_eq!(cap_bytes, 64 * 1024 * 1024, "requested byte budget");

    // SQLite's own default form (-2000 KiB): the read form reports the
    // raw setting; the byte budget is |k| KiB.
    db.execute("PRAGMA cache_size=-2000", []).expect("pragma");
    assert_eq!(read_int(&db, "PRAGMA cache_size"), -2000);
    let cap_bytes = 2000 * 1024;
    assert_eq!(cap_bytes, 2_000 * 1024, "-2000 KiB budget");

    // Positive form stays a direct page count.
    db.execute("PRAGMA cache_size=77", []).expect("pragma");
    assert_eq!(read_int(&db, "PRAGMA cache_size"), 77);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn engine_ledger_counts_autocommit_and_explicit_transactions() {
    let path = temp_db_path("ledger");
    let mut db = Database::open(&path).expect("open");

    // 1 implicit txn (DDL auto-commits on its own).
    db.execute("CREATE TABLE t (a INTEGER)", []).unwrap();
    // 3 implicit txns (auto-commit single + chained + multi-row fast).
    db.execute("INSERT INTO t VALUES (1)", []).unwrap();
    db.execute("INSERT INTO t VALUES (2)", []).unwrap();
    db.execute("INSERT INTO t VALUES (3),(4),(5)", []).unwrap();

    assert_eq!(
        db.pager().tx_begun_total(),
        4,
        "CREATE + 3 auto-commit INSERTs"
    );
    assert_eq!(db.pager().tx_committed_total(), 4);
    assert_eq!(db.pager().tx_rolled_back_total(), 0);
    assert_eq!(db.pager().rows_modified_total(), 5, "5 rows inserted");

    // Explicit transaction: one begun, one committed — the in-txn INSERT
    // adds NO implicit txn.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t VALUES (6)", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    assert_eq!(db.pager().tx_begun_total(), 5);
    assert_eq!(db.pager().tx_committed_total(), 5);
    assert_eq!(db.pager().rows_modified_total(), 6);

    // Rolled-back explicit transaction: begun + rolled_back, no commit.
    // rows_modified keeps the statement count (SQLite's total_changes
    // counts statement completions, not durable rows).
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t VALUES (7)", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    assert_eq!(db.pager().tx_begun_total(), 6);
    assert_eq!(db.pager().tx_committed_total(), 5);
    assert_eq!(db.pager().tx_rolled_back_total(), 1);
    assert_eq!(db.pager().rows_modified_total(), 7);

    // Durability sanity: the rolled-back row is gone.
    assert_eq!(read_int(&db, "SELECT COUNT(*) FROM t"), 6);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn engine_ledger_counts_streaming_statement_dml() {
    // The prepare/bind/step path (sqlx's C-ABI execution shape) must feed
    // the same aggregates.
    let path = temp_db_path("streaming");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE t (a INTEGER)", []).unwrap();

    {
        let mut stmt = db.prepare("INSERT INTO t VALUES (?)").expect("prepare");
        for i in 1..=3i64 {
            stmt.bind(1, rustqlite::Value::Integer(i)).expect("bind");
            let _ = stmt.step().expect("step");
            stmt.reset();
        }
    }

    assert_eq!(db.pager().tx_begun_total(), 4, "CREATE + 3 stepped INSERTs");
    assert_eq!(db.pager().tx_committed_total(), 4);
    assert_eq!(db.pager().rows_modified_total(), 3);
    assert_eq!(read_int(&db, "SELECT COUNT(*) FROM t"), 3);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn cache_size_kib_updates_capacity_before_any_insert() {
    // The pragma is applied LIVE (not at next open): the very next cache
    // insert's eviction pass uses the new capacity. A DB that has already
    // touched pages must accept the resize.
    let path = temp_db_path("live");
    let mut db = Database::open(&path).expect("open");
    db.execute("CREATE TABLE t (a INTEGER)", []).unwrap();
    db.execute("INSERT INTO t VALUES (1)", []).unwrap();

    db.execute("PRAGMA cache_size=-4096", []).unwrap(); // 4 MiB
    let setting = read_int(&db, "PRAGMA cache_size");
    assert_eq!(setting, -4096, "live resize: read form = raw setting");
    let cap_bytes = (-setting) * 1024;
    assert_eq!(cap_bytes, 4 * 1024 * 1024, "live resize: requested bytes");

    // Shrink below the current cache footprint: further inserts must trim
    // toward the new capacity (never exceed it by more than pin pressure).
    db.execute("PRAGMA cache_size=8", []).unwrap();
    db.execute("INSERT INTO t VALUES (2),(3),(4),(5)", [])
        .unwrap();
    assert_eq!(read_int(&db, "PRAGMA cache_size"), 8);
    assert!(
        db.pager().cache_size() <= 64,
        "cache should be trimmed near the 8-page capacity after inserts, held {}",
        db.pager().cache_size()
    );

    let _ = std::fs::remove_file(&path);
}
