//! EXPLAIN ANALYZE smoke tests (PostgreSQL-borrowed instrumentation).

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<String>> {
    let rows = db.query(sql, []).expect(sql);
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Text(t) => t.as_str().to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => format!("{}", f),
                    Value::Null => "NULL".to_string(),
                    Value::Blob(_) => "<blob>".to_string(),
                })
                .collect()
        })
        .collect()
}

#[test]
fn analyze_basic_shape() {
    let mut db = db();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", [])
        .unwrap();
    for b in ["x", "y", "z"] {
        db.execute("INSERT INTO t(b) VALUES (?)", [Value::Text(b.into())])
            .unwrap();
    }
    let rows = rows_of(&db, "EXPLAIN ANALYZE SELECT a FROM t WHERE a > 1");
    // Last row = total runtime marker; earlier rows carry node details.
    assert!(!rows.is_empty(), "no analyze rows: {:?}", rows);
    let last = rows.last().unwrap();
    assert_eq!(last[4], "Total runtime", "last row detail: {:?}", last);
    // Some node reports 2 actual rows (a=2, a=3 survived the filter).
    assert!(
        rows.iter().any(|r| r[2] == "2"),
        "no node with actual rows=2: {:?}",
        rows
    );
    // elapsed_ms is a non-negative real.
    for r in &rows {
        let ms: f64 = r[3].parse().unwrap_or(-1.0);
        assert!(ms >= 0.0, "negative elapsed_ms in {:?}", r);
    }
}

#[test]
fn analyze_executes_inner_statement() {
    let mut db = db();
    db.execute("CREATE TABLE t(a INTEGER)", []).unwrap();
    for a in 1..=5 {
        db.execute("INSERT INTO t VALUES (?)", [Value::Integer(a)])
            .unwrap();
    }
    // The analysis REPLACES the result rows (PG semantics) — but the inner
    // SELECT really ran: actual row counts reflect the data. COUNT(*) plans
    // as Aggregate (1 output row); the WHERE-only variant surfaces the
    // filtered row count on the scan/projection node.
    let rows = rows_of(&db, "EXPLAIN ANALYZE SELECT COUNT(*) FROM t WHERE a >= 2");
    let last = rows.last().unwrap();
    assert_eq!(last[4], "Total runtime");
    assert!(
        rows.iter().any(|r| r[4] == "AGGREGATE" && r[2] == "1"),
        "no AGGREGATE node with 1 row: {:?}",
        rows
    );
    let rows = rows_of(&db, "EXPLAIN ANALYZE SELECT a FROM t WHERE a >= 2");
    assert!(
        rows.iter().any(|r| r[2] == "4"),
        "no node with 4 actual rows: {:?}",
        rows
    );
}

#[test]
fn analyze_with_params() {
    let mut db = db();
    db.execute("CREATE TABLE t(a INTEGER)", []).unwrap();
    for a in 1..=3 {
        db.execute("INSERT INTO t VALUES (?)", [Value::Integer(a)])
            .unwrap();
    }
    let rows = db
        .query(
            "EXPLAIN ANALYZE SELECT a FROM t WHERE a >= ?",
            [Value::Integer(1)],
        )
        .unwrap();
    let details: Vec<String> = rows.iter().map(|r| r[4].as_text().to_string()).collect();
    assert!(details.iter().any(|d| d.contains("Total runtime")));
    assert!(
        rows.iter().any(|r| matches!(r[2], Value::Integer(3))),
        "no node with 3 actual rows: {:?}",
        rows
    );
}

#[test]
fn analyze_rejects_dml() {
    let mut db = db();
    db.execute("CREATE TABLE t(a INTEGER)", []).unwrap();
    let err = db
        .query("EXPLAIN ANALYZE INSERT INTO t VALUES (1)", [])
        .unwrap_err();
    assert!(err.to_string().contains("EXPLAIN ANALYZE"), "err: {}", err);
}

#[test]
fn static_explain_unchanged() {
    let mut db = db();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", [])
        .unwrap();
    db.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    // EXPLAIN QUERY PLAN: static, 4-column shape, never executes.
    let rows = db
        .query("EXPLAIN QUERY PLAN SELECT a FROM t WHERE b = 'x'", [])
        .unwrap();
    assert!(!rows.is_empty());
    assert_eq!(rows[0].len(), 4);
    let detail = rows[0][3].as_text();
    assert!(
        detail.contains("SCAN t") || detail.contains("SEARCH t"),
        "detail: {}",
        detail
    );
    // Plain EXPLAIN behaves the same as QUERY PLAN (SQLite parses both).
    let rows2 = db
        .query("EXPLAIN SELECT a FROM t WHERE b = 'x'", [])
        .unwrap();
    assert_eq!(rows2.len(), rows.len());
}

#[test]
fn analyze_with_cte() {
    let mut db = db();
    db.execute("CREATE TABLE t(a INTEGER)", []).unwrap();
    for a in 1..=4 {
        db.execute("INSERT INTO t VALUES (?)", [Value::Integer(a)])
            .unwrap();
    }
    let rows = rows_of(
        &db,
        "EXPLAIN ANALYZE WITH big AS (SELECT a FROM t WHERE a > 1) SELECT COUNT(*) FROM big",
    );
    let last = rows.last().unwrap();
    assert_eq!(last[4], "Total runtime");
    assert!(
        rows.iter().any(|r| r[2] == "3"),
        "no node with 3 actual rows (CTE body): {:?}",
        rows
    );
}
