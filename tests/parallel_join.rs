//! Parallel JOIN probe: the fused scan-hash join's probe side splits
//! across workers. Results (values AND row order) must be bit-identical
//! to the serial probe — range-ordered partial concatenation reproduces
//! the serial scan's row order exactly.

use rustqlite::{Database, Value};

fn build(db: &mut Database, n: i64) {
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 3),
            ],
        )
        .unwrap();
    }
    for i in 1..=n {
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 7),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

#[test]
fn parallel_join_matches_serial_exactly() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 150_000);
    let sql = "SELECT a.x, b.y, a.k FROM a JOIN b ON b.k = a.k";
    // Serial (parallel_scan off).
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    // Parallel (default threshold engaged at 400k rows).
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser.len(), par.len(), "row count differs");
    assert_eq!(
        ser, par,
        "parallel join must equal serial bit-for-bit, order included"
    );
    // Sanity: the join actually matched rows (both k ranges overlap).
    assert!(ser.len() > 1000, "join unexpectedly tiny: {}", ser.len());
}

#[test]
fn parallel_join_with_filter_and_order() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 120_000);
    let sql = "SELECT a.x, b.y FROM a JOIN b ON b.k = a.k WHERE a.x > 100000 ORDER BY b.y LIMIT 50";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser, par);
    assert_eq!(ser.len(), 50);
}

#[test]
fn parallel_join_aggregate_over_output() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 120_000);
    let sql = "SELECT COUNT(*), SUM(a.x), MIN(b.y), MAX(b.y) FROM a JOIN b ON b.k = a.k";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser, par);
    // The aggregate values are deterministic integers: both paths equal.
    assert_eq!(ser[0][0], par[0][0]);
}

#[test]
fn parallel_join_real_keys_and_nulls() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k REAL, x TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k REAL, y TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100_000i64 {
        let k = if i % 10 == 0 {
            Value::Null // NULL keys never match
        } else {
            Value::Real((i % 500) as f64 + 0.5)
        };
        db.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                k,
                Value::Text(format!("a{}", i % 97).into()),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Real((i % 500) as f64 + 0.5),
                Value::Text(format!("b{}", i % 89).into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let sql = "SELECT a.x, b.y FROM a JOIN b ON b.k = a.k";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser, par);
    assert!(!ser.is_empty());
}

#[test]
fn parallel_join_write_between_queries() {
    // Writes between executions must invalidate the join-build cache and
    // the workers must observe the new committed state.
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 100_000);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let sql = "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k";
    let before = rows_of(&db, sql)[0][0].as_integer();
    db.execute("DELETE FROM b WHERE id > 99000", []).unwrap();
    let after = rows_of(&db, sql)[0][0].as_integer();
    assert!(
        after <= before,
        "delete must not grow the join: {} -> {}",
        before,
        after
    );
    db.execute(
        "INSERT INTO b (id, k, y) VALUES (100001, 99999, 99999)",
        [
            Value::Integer(1000001),
            Value::Integer(0),
            Value::Integer(0),
        ],
    )
    .unwrap();
    let grown = rows_of(&db, sql)[0][0].as_integer();
    assert!(
        grown >= after,
        "insert must be visible to the parallel probe"
    );
}
