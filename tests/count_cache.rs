//! Correctness tests for the epoch-keyed `SELECT COUNT(*)` memoization
//! (see `Database::write_epoch` / `table_count_cache`).
//!
//! The cache is only sound if EVERY row-count-changing path bumps the
//! write epoch: DML via `execute`, DML via prepared statements, DDL
//! (re-created tables), transaction ROLLBACK, and ROLLBACK TO SAVEPOINT.

use rustqlite::{Database, Value};

fn count(db: &Database) -> i64 {
    db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer()
}

fn count_col(db: &Database) -> i64 {
    // COUNT(col) takes the general (non-memoized) path — sanity cross-check.
    db.query("SELECT COUNT(val) FROM t", []).unwrap()[0][0].as_integer()
}

fn fresh(n_rows: i64) -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    for i in 1..=n_rows {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i * 10)])
            .unwrap();
    }
    db
}

#[test]
fn count_matches_after_each_insert() {
    let mut db = fresh(0);
    assert_eq!(count(&db), 0);
    for i in 1..=100 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
        assert_eq!(count(&db), i, "count after insert #{}", i);
    }
    // Cross-check against the non-memoized COUNT(col) path.
    assert_eq!(count_col(&db), 100);
}

#[test]
fn count_after_delete_and_update() {
    let mut db = fresh(50);
    assert_eq!(count(&db), 50);
    db.execute("DELETE FROM t WHERE id <= 20", []).unwrap();
    assert_eq!(count(&db), 30, "count after DELETE");
    // UPDATE keeps the count.
    db.execute("UPDATE t SET val = 99", []).unwrap();
    assert_eq!(count(&db), 30, "count after UPDATE");
    db.execute("DELETE FROM t", []).unwrap();
    assert_eq!(count(&db), 0, "count after DELETE all");
}

#[test]
fn count_sees_uncommitted_rows_in_transaction() {
    let mut db = fresh(10);
    assert_eq!(count(&db), 10);
    db.execute("BEGIN", []).unwrap();
    for i in 0..15 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    assert_eq!(count(&db), 25, "own uncommitted inserts visible");
    db.execute("ROLLBACK", []).unwrap();
    assert_eq!(count(&db), 10, "count reverted after ROLLBACK");
    db.execute("BEGIN", []).unwrap();
    for i in 0..5 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    assert_eq!(count(&db), 15, "count after COMMIT");
}

#[test]
fn count_after_savepoint_rollback() {
    let mut db = fresh(10);
    assert_eq!(count(&db), 10);
    db.execute("SAVEPOINT sp1", []).unwrap();
    for i in 0..7 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    assert_eq!(count(&db), 17, "rows inside savepoint visible");
    db.execute("ROLLBACK TO sp1", []).unwrap();
    assert_eq!(count(&db), 10, "count reverted after ROLLBACK TO SAVEPOINT");
    db.execute("RELEASE sp1", []).unwrap();
    assert_eq!(count(&db), 10);
}

#[test]
fn count_after_prepared_statement_dml() {
    let db = fresh(20);
    assert_eq!(count(&db), 20);
    // Warm the memoized answer first.
    assert_eq!(count(&db), 20);

    // INSERT via a prepared statement (bypasses Database::execute).
    let mut ins = db.prepare("INSERT INTO t (val) VALUES (?)").unwrap();
    for i in 0..10 {
        ins.bind(1, Value::Integer(i)).unwrap();
        let _ = ins.step().unwrap();
        ins.reset();
    }
    drop(ins);
    assert_eq!(count(&db), 30, "prepared-statement INSERTs visible");

    // DELETE via a prepared statement.
    let mut del = db.prepare("DELETE FROM t WHERE id <= 15").unwrap();
    let _ = del.step().unwrap();
    drop(del);
    assert_eq!(count(&db), 15, "prepared-statement DELETE visible");

    // Cross-check the general path.
    assert_eq!(count_col(&db), 15);
}

#[test]
fn count_after_drop_and_recreate() {
    let mut db = fresh(30);
    assert_eq!(count(&db), 30);
    db.execute("DROP TABLE t", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    assert_eq!(count(&db), 0, "re-created table counts from zero");
    db.execute("INSERT INTO t (val) VALUES (1)", []).unwrap();
    assert_eq!(count(&db), 1);
}

#[test]
fn count_multiple_tables_invalidate_independently() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    for i in 0..10 {
        db.execute("INSERT INTO a (x) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    let ca = |db: &Database| db.query("SELECT COUNT(*) FROM a", []).unwrap()[0][0].as_integer();
    let cb = |db: &Database| db.query("SELECT COUNT(*) FROM b", []).unwrap()[0][0].as_integer();
    assert_eq!(ca(&db), 10);
    assert_eq!(cb(&db), 0);
    // Writing to b must not corrupt a's memoized count (a re-walk is fine,
    // a WRONG hit is not — either way the answers must be exact).
    db.execute("INSERT INTO b (x) VALUES (1)", []).unwrap();
    assert_eq!(ca(&db), 10);
    assert_eq!(cb(&db), 1);
    db.execute("INSERT INTO a (x) VALUES (99)", []).unwrap();
    assert_eq!(ca(&db), 11);
    assert_eq!(cb(&db), 1);
}

#[test]
fn count_where_takes_general_path() {
    let mut db = fresh(40);
    // WHERE forces the executor path (count_rows_range), not the memoized
    // arm — results must always be exact regardless of memoization.
    let w = |db: &Database, lo: i64, hi: i64| {
        db.query(
            "SELECT COUNT(*) FROM t WHERE id BETWEEN ? AND ?",
            [Value::Integer(lo), Value::Integer(hi)],
        )
        .unwrap()[0][0]
            .as_integer()
    };
    assert_eq!(w(&db, 1, 10), 10);
    assert_eq!(w(&db, 5, 15), 11);
    assert_eq!(w(&db, 100, 200), 0);
    db.execute("INSERT INTO t (val) VALUES (7)", []).unwrap();
    assert_eq!(w(&db, 1, 41), 41);
    assert_eq!(count(&db), 41);
}

#[test]
fn count_across_reopen_and_file_db() {
    let path = std::env::temp_dir().join("count_cache_file.db");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    for i in 0..25 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    assert_eq!(count(&db), 25);
    db.flush().unwrap();
    drop(db);

    let db2 = Database::open(&path).unwrap();
    assert_eq!(count(&db2), 25, "fresh open counts from the file");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn count_distinct_and_grouped_not_memoized_wrongly() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    for i in 1..=30 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    let d = db.query("SELECT COUNT(DISTINCT val) FROM t", []).unwrap()[0][0].as_integer();
    assert_eq!(d, 30);
    let g: Vec<Vec<Value>> = db
        .query(
            "SELECT val % 10 AS bucket, COUNT(*) FROM t GROUP BY bucket",
            [],
        )
        .unwrap();
    assert_eq!(g.len(), 10, "30 consecutive values spread over 10 buckets");
    let total: i64 = g.iter().map(|r| r[1].as_integer()).sum();
    assert_eq!(total, 30);
    db.execute("INSERT INTO t (val) VALUES (5)", []).unwrap();
    let d2 = db.query("SELECT COUNT(DISTINCT val) FROM t", []).unwrap()[0][0].as_integer();
    assert_eq!(d2, 30, "duplicate value does not change DISTINCT count");
    assert_eq!(count(&db), 31);
}
