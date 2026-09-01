//! SQLite-style prepared-statement API tests: prepare / bind / step /
//! reset, streaming drivers, parameter discovery, DML through statements.

use rustqlite::types::Value;
use rustqlite::{Database, StepResult};

fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER, y TEXT)", [])
        .unwrap();
    for i in 0..100 {
        db.execute(
            "INSERT INTO t (x, y) VALUES (?, ?)",
            vec![Value::Integer(i * 10), Value::Text(format!("row-{}", i).into())],
        )
        .unwrap();
    }
    db
}

#[test]
fn prepare_step_basic() {
    let db = setup();
    let mut stmt = db.prepare("SELECT x, y FROM t WHERE id = ?").unwrap();
    stmt.bind(1, Value::Integer(5)).unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_int(0), 40); // rowid 5 → i=4 → x=40
    assert_eq!(stmt.column_text(1).unwrap(), "row-4");
    assert_eq!(stmt.step().unwrap(), StepResult::Done);
    // Stepping past DONE stays DONE.
    assert_eq!(stmt.step().unwrap(), StepResult::Done);
}

#[test]
fn prepare_rebind_reset() {
    let db = setup();
    let mut stmt = db.prepare("SELECT y FROM t WHERE id = ?").unwrap();
    for rid in [1i64, 50, 99] {
        stmt.reset();
        stmt.bind(1, Value::Integer(rid)).unwrap();
        assert_eq!(stmt.step().unwrap(), StepResult::Row);
        assert_eq!(stmt.column_text(0).unwrap(), format!("row-{}", rid - 1));
        assert_eq!(stmt.step().unwrap(), StepResult::Done);
    }
}

#[test]
fn prepare_streaming_scan_large() {
    let db = setup();
    // 100-row full scan streamed in batches of 64 — two batches.
    let mut stmt = db.prepare("SELECT id, x FROM t").unwrap();
    let mut count = 0;
    let mut sum = 0i64;
    while stmt.step().unwrap() == StepResult::Row {
        count += 1;
        sum += stmt.column_int(1);
    }
    assert_eq!(count, 100);
    assert_eq!(sum, (0..100).map(|i| i * 10).sum::<i64>());
    assert_eq!(stmt.column_count(), 2);
    assert_eq!(stmt.column_name(0), Some("id"));
}

#[test]
fn prepare_streaming_range_with_limit() {
    let db = setup();
    let mut stmt = db
        .prepare("SELECT id FROM t WHERE id BETWEEN ? AND ? LIMIT 10 OFFSET 5")
        .unwrap();
    stmt.bind(1, Value::Integer(0)).unwrap();
    stmt.bind(2, Value::Integer(1000)).unwrap();
    let mut ids = Vec::new();
    while stmt.step().unwrap() == StepResult::Row {
        ids.push(stmt.column_int(0));
    }
    assert_eq!(ids, (6..=15).collect::<Vec<i64>>());
}

#[test]
fn prepare_streaming_with_filter() {
    let db = setup();
    let mut stmt = db.prepare("SELECT id FROM t WHERE x > ?").unwrap();
    stmt.bind(1, Value::Integer(500)).unwrap();
    let mut count = 0;
    while stmt.step().unwrap() == StepResult::Row {
        count += 1;
    }
    assert_eq!(count, 49); // x = 510..990 → ids 51..99
}

#[test]
fn prepare_projection_and_column_names() {
    let db = setup();
    let mut stmt = db
        .prepare("SELECT y AS label, x * 2 AS dbl FROM t WHERE id < 3 ORDER BY id")
        .unwrap();
    let mut rows = Vec::new();
    while stmt.step().unwrap() == StepResult::Row {
        rows.push((
            stmt.column_text(0).unwrap(),
            stmt.column_int(1),
        ));
    }
    assert_eq!(rows.len(), 2); // ids 1, 2 (id < 3)
    assert_eq!(rows[0].0, "row-0");
    assert_eq!(rows[0].1, 0);
    assert_eq!(rows[1].1, 20); // id=2 → x=10 → 20
    let _ = stmt.column_name(0);
}

#[test]
fn prepare_named_parameters() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b TEXT)", []).unwrap();
    let mut stmt = db
        .prepare("INSERT INTO t (a, b) VALUES (:a, :b)")
        .unwrap();
    assert_eq!(stmt.parameter_count(), 0); // named, not positional
    assert!(stmt.parameter_names().iter().any(|n| n.ends_with('a')));
    assert!(stmt.parameter_names().iter().any(|n| n.ends_with('b')));
    stmt.bind_named(":a", Value::Integer(1)).unwrap();
    stmt.bind_named("b", Value::Text("hello".into())).unwrap(); // bare name works
    stmt.raw_execute().unwrap();
    let rows = db.query("SELECT a, b FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    assert_eq!(rows[0][1].as_text(), "hello");
}

#[test]
fn prepare_bind_validation() {
    let db = setup();
    let mut stmt = db.prepare("SELECT x FROM t WHERE id = ?").unwrap();
    assert_eq!(stmt.parameter_count(), 1);
    assert!(stmt.bind(0, Value::Integer(1)).is_err()); // 0 invalid
    assert!(stmt.bind(2, Value::Integer(1)).is_err()); // out of range
    assert!(stmt.bind_named(":nope", Value::Null).is_err());
}

#[test]
fn prepare_insert_update_delete() {
    let db = setup();
    // INSERT via statement + rebind loop (the OLTP pattern).
    {
        let mut stmt = db
            .prepare("INSERT INTO t (x, y) VALUES (?, ?)")
            .unwrap();
        for i in 100..200 {
            stmt.reset();
            stmt.bind(1, Value::Integer(i)).unwrap();
            stmt.bind(2, Value::Text(format!("new-{}", i).into())).unwrap();
            stmt.raw_execute().unwrap();
        }
    }
    let rows = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 200);
    // UPDATE via statement.
    {
        let mut stmt = db
            .prepare("UPDATE t SET x = x + 1 WHERE id > ?")
            .unwrap();
        stmt.bind(1, Value::Integer(190)).unwrap();
        stmt.raw_execute().unwrap();
    }
    let rows = db.query("SELECT x FROM t WHERE id = 200", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 200); // x=199 (i=199) + 1
    // DELETE via statement.
    {
        let mut stmt = db.prepare("DELETE FROM t WHERE id > ?").unwrap();
        stmt.bind(1, Value::Integer(150)).unwrap();
        stmt.raw_execute().unwrap();
    }
    let rows = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 150); // 200 - (ids 151..200)
}

#[test]
fn prepare_rejects_ddl_and_txn() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (x)", []).unwrap();
    for sql in [
        "BEGIN",
        "COMMIT",
        "CREATE TABLE u (y)",
        "DROP TABLE t",
    ] {
        let err = match db.prepare(sql) {
            Err(e) => e,
            Ok(_) => panic!("{} should be rejected by prepare()", sql),
        };
        assert!(
            err.to_string().contains("Database::execute"),
            "{} should be rejected, got {}",
            sql,
            err
        );
    }
}

#[test]
fn prepare_aggregate_materializes() {
    let db = setup();
    let mut stmt = db.prepare("SELECT count(*), sum(x), max(y) FROM t").unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_int(0), 100);
    assert_eq!(stmt.column_int(1), 49500); // sum(i*10, i=0..99) = 10*4950
    assert_eq!(stmt.step().unwrap(), StepResult::Done);
}

#[test]
fn prepare_query_all_and_finalize() {
    let db = setup();
    let mut stmt = db.prepare("SELECT id FROM t WHERE id < 10 ORDER BY id DESC").unwrap();
    let rows = stmt.query_all().unwrap();
    assert_eq!(rows.len(), 9);
    // query_all exhausted the statement.
    assert_eq!(stmt.step().unwrap(), StepResult::Done);
    stmt.finalize().unwrap();
}

#[test]
fn prepare_case_and_types() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n)", [])
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES (1, 2.5, 'txt', x'00ff', NULL)",
        [],
    )
    .unwrap();
    let mut stmt = db.prepare("SELECT i, r, s, b, n FROM t").unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_int(0), 1);
    assert!((stmt.column_real(1) - 2.5).abs() < 1e-12);
    assert_eq!(stmt.column_text(2).unwrap(), "txt");
    assert_eq!(stmt.column_blob(3).unwrap(), vec![0x00, 0xff]);
    assert!(stmt.column_value(4).unwrap().is_null());
}

#[test]
fn concurrent_readers_through_statements() {
    // The engine's MRMW parallel-read win, exercised through prepared
    // statements: N threads share Arc<Database>, each steps its own scan.
    let db = std::sync::Arc::new(setup());
    let mut handles = Vec::new();
    for t in 0..4 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            let mut stmt = db.prepare("SELECT sum(x) FROM t WHERE id > ?").unwrap();
            stmt.bind(1, Value::Integer(t * 10)).unwrap();
            stmt.query_all().unwrap()[0][0].as_integer()
        }));
    }
    let sums: Vec<i64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // t=0: ids 1..100 → all 100 x-values.
    assert_eq!(sums[0], (0..100).map(|i| i * 10).sum::<i64>());
    // Each thread drops 10 more rows (ids <= t*10).
    for t in 1..4 {
        let full: i64 = (0..100).map(|i| i * 10).sum();
        let dropped: i64 = (0..t * 10).map(|i| i * 10).sum();
        assert_eq!(sums[t as usize], full - dropped);
    }
}
