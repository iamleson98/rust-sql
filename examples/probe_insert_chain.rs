//! INSERT chain probe: correctness of chained single-row literal inserts
//! interleaved with every chain-breaking statement kind, plus a throughput
//! measurement (txn + autocommit) mirroring the criterion insert bench.

use rustqlite::{Database, Value};

fn main() {
    // ---------------- Correctness: chain + breakers ----------------
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", [])
        .unwrap();

    // Three chained inserts.
    for i in 1..=3i64 {
        db.execute(&format!("INSERT INTO t (name, val) VALUES ('n{i}', {i})"), [])
            .unwrap();
    }
    // A SELECT breaks the chain and must observe all rows.
    let rows = db.query("SELECT COUNT(*), SUM(val) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(3));
    assert_eq!(rows[0][1], Value::Integer(6));

    // Chained inserts continue after the read (chain rebuild).
    for i in 4..=5i64 {
        db.execute(&format!("INSERT INTO t (name, val) VALUES ('n{i}', {i})"), [])
            .unwrap();
    }
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(5));

    // DELETE breaks the chain; the next chained insert must re-derive
    // max_rowid (max rowid was 5, deleted; SQLite semantics: next is the
    // scan-derived max + 1, i.e. 4 -> then chain continues 5, 6...).
    db.execute("DELETE FROM t WHERE id = 5", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('again', 99)", []).unwrap();
    let rows = db
        .query("SELECT id, name, val FROM t WHERE name = 'again'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    println!("after delete+insert: id={} (expect 4..6, no collision)", rows[0][0]);

    // ROLLBACK mid-chain.
    db.execute("BEGIN", []).unwrap();
    for i in 100..=150i64 {
        db.execute(&format!("INSERT INTO t (name, val) VALUES ('r{i}', {i})"), [])
            .unwrap();
    }
    let n_before = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
    db.execute("ROLLBACK", []).unwrap();
    let n_after = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
    println!("rollback: {n_before} -> {n_after} (expect back to pre-txn)");
    assert_eq!(n_after, Value::Integer(5));

    // COMMIT persistence of chained inserts (in-memory, just visibility).
    db.execute("BEGIN", []).unwrap();
    for i in 200..=250i64 {
        db.execute(&format!("INSERT INTO t (name, val) VALUES ('c{i}', {i})"), [])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
    assert_eq!(n, Value::Integer(56));

    // Trigger table: chain must NOT engage (trigger must fire).
    db.execute("CREATE TABLE log (msg TEXT)", []).unwrap();
    db.execute("CREATE TABLE t2 (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t2 BEGIN INSERT INTO log VALUES ('fired'); END",
        [],
    )
    .unwrap();
    for i in 0..3 {
        db.execute(&format!("INSERT INTO t2 (x) VALUES ({i})"), []).unwrap();
    }
    let fired = db.query("SELECT COUNT(*) FROM log", []).unwrap()[0][0].clone();
    assert_eq!(fired, Value::Integer(3), "trigger must fire 3x via cold path");
    let x_sum = db.query("SELECT SUM(x) FROM t2", []).unwrap()[0][0].clone();
    assert_eq!(x_sum, Value::Integer(3));

    // NOT NULL violation mid-chain: error surfaces, state stays correct.
    db.execute("CREATE TABLE t3 (id INTEGER PRIMARY KEY, a TEXT NOT NULL)", [])
        .unwrap();
    db.execute("INSERT INTO t3 (a) VALUES ('ok1')", []).unwrap();
    let err = db
        .execute("INSERT INTO t3 (a) VALUES (NULL)", [])
        .err()
        .expect("NOT NULL must reject");
    println!("NOT NULL error: {err}");
    db.execute("INSERT INTO t3 (a) VALUES ('ok2')", []).unwrap();
    let n = db.query("SELECT COUNT(*) FROM t3", []).unwrap()[0][0].clone();
    assert_eq!(n, Value::Integer(2));

    // Escaped quotes in chained literals.
    db.execute("CREATE TABLE t4 (id INTEGER PRIMARY KEY, s TEXT)", []).unwrap();
    db.execute("INSERT INTO t4 (s) VALUES ('it''s')", []).unwrap();
    db.execute("INSERT INTO t4 (s) VALUES ('plain')", []).unwrap();
    let s = db.query("SELECT s FROM t4 WHERE id = 1", []).unwrap()[0][0].clone();
    assert_eq!(s, Value::Text("it's".into()));
    let s2 = db.query("SELECT s FROM t4 WHERE id = 2", []).unwrap()[0][0].clone();
    assert_eq!(s2, Value::Text("plain".into()));

    // Multi-row VALUES after a single-row chain (shape fallback).
    db.execute(
        "INSERT INTO t4 (s) VALUES ('m1'), ('m2'), ('m3')",
        [],
    )
    .unwrap();
    let n = db.query("SELECT COUNT(*) FROM t4", []).unwrap()[0][0].clone();
    assert_eq!(n, Value::Integer(5));

    // Supplies-all shape (no column list).
    db.execute(
        "CREATE TABLE t5 (id INTEGER PRIMARY KEY, a TEXT, b INTEGER)",
        [],
    )
    .unwrap();
    for i in 0..5 {
        db.execute(&format!("INSERT INTO t5 VALUES (NULL, 'v{i}', {i})"), [])
            .unwrap();
    }
    let n = db.query("SELECT COUNT(*) FROM t5", []).unwrap()[0][0].clone();
    assert_eq!(n, Value::Integer(5));
    let ids = db.query("SELECT MIN(id), MAX(id) FROM t5", []).unwrap();
    assert_eq!(ids[0][0], Value::Integer(1));
    assert_eq!(ids[0][1], Value::Integer(5));

    // ---------------- Throughput ----------------
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", [])
        .unwrap();
    // Warm the chain.
    for i in 0..100 {
        db.execute(&format!("INSERT INTO t (name, val) VALUES ('w{i}', {i})"), [])
            .unwrap();
    }
    let n = 5000i64;
    let sqls: Vec<String> = (1..=n)
        .map(|i| format!("INSERT INTO t (name, val) VALUES ('name{i}', {i})"))
        .collect();
    db.execute("BEGIN", []).unwrap();
    let t0 = std::time::Instant::now();
    for sql in &sqls {
        db.execute(sql, []).unwrap();
    }
    let txn = t0.elapsed();
    db.execute("COMMIT", []).unwrap();
    let cnt = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].clone();
    assert_eq!(cnt, Value::Integer(n + 100));
    println!(
        "txn chained   : {:>9.2?}  ({:.0} ns/insert)",
        txn,
        txn.as_nanos() as f64 / n as f64
    );

    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", [])
        .unwrap();
    let t0 = std::time::Instant::now();
    for i in 1..=n {
        db2.execute(&format!("INSERT INTO t (name, val) VALUES ('name{i}', {i})"), [])
            .unwrap();
    }
    let ac = t0.elapsed();
    println!(
        "autocommit    : {:>9.2?}  ({:.0} ns/insert)",
        ac,
        ac.as_nanos() as f64 / n as f64
    );
    println!("all chain correctness checks passed");
}
