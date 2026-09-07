//! rowid-inside-EXPRESSIONS (rowid-in-expressions hardening (2026-09)): `rowid` /
//! `_rowid_` / `oid` referenced inside expressions — `rowid*2`,
//! `max(rowid)`, `WHERE rowid % 2 = 0`, `ORDER BY rowid % 3` — on both
//! alias tables (INTEGER PRIMARY KEY: planner rewrite to the alias
//! column) and no-alias tables (trailing hidden rowid slot on every
//! row-producing path). Shadowing (a REAL column named `rowid`),
//! qualification (`t.rowid`, `x.rowid` under an alias), star arity, and
//! both API surfaces (query + prepare/step) are pinned.
use rustqlite::{Database, StepResult, Value};

fn mixed() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, name TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ia ON t(a)", []).unwrap();
    for i in 0..20i64 {
        db.execute(
            "INSERT INTO t (a, name) VALUES (?, 'n')",
            [Value::Integer(i % 10)],
        )
        .unwrap();
    }
    db
}

fn first_col(db: &Database, sql: &str) -> Vec<Value> {
    db.query(sql, [])
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[test]
fn rowid_expr_alias_table_projection() {
    let db = mixed();
    // rows 3,13 have a=2 → rowid*2 = 6,26 (index order).
    assert_eq!(
        first_col(&db, "SELECT rowid*2 FROM t WHERE a = 2"),
        vec![Value::Integer(6), Value::Integer(26)]
    );
    assert_eq!(
        first_col(&db, "SELECT rowid + 0 FROM t WHERE id BETWEEN 2 AND 3"),
        vec![Value::Integer(2), Value::Integer(3)]
    );
    // All spellings.
    for sql in [
        "SELECT oid * 2 FROM t WHERE a = 2",
        "SELECT _rowid_ * 2 FROM t WHERE a = 2",
        "SELECT t.rowid * 2 FROM t WHERE a = 2",
        "SELECT x.rowid * 2 FROM t AS x WHERE x.a = 2",
    ] {
        assert_eq!(
            first_col(&db, sql),
            vec![Value::Integer(6), Value::Integer(26)],
            "{sql}"
        );
    }
    // Functions over rowid.
    assert_eq!(
        first_col(&db, "SELECT abs(rowid - 10) FROM t WHERE rowid = 12"),
        vec![Value::Integer(2)]
    );
}

#[test]
fn rowid_expr_alias_table_predicates_and_order() {
    let db = mixed();
    // WHERE rowid % 2 = 0 (rowids 2,4,...,20), ORDER BY rowid, LIMIT 3.
    let got: Vec<Value> = first_col(
        &db,
        "SELECT a FROM t WHERE rowid % 2 = 0 ORDER BY rowid LIMIT 3",
    );
    assert_eq!(
        got,
        vec![Value::Integer(1), Value::Integer(3), Value::Integer(5)]
    );
    // ORDER BY rowid % 3 (0-class first: 3,6,9,...).
    let got: Vec<Value> = first_col(&db, "SELECT rowid FROM t ORDER BY rowid % 3, rowid LIMIT 4");
    assert_eq!(
        got,
        vec![
            Value::Integer(3),
            Value::Integer(6),
            Value::Integer(9),
            Value::Integer(12)
        ]
    );
    // Aggregates over rowid.
    assert_eq!(
        first_col(&db, "SELECT max(rowid) FROM t"),
        vec![Value::Integer(20)]
    );
    assert_eq!(
        first_col(&db, "SELECT sum(rowid) FROM t"),
        vec![Value::Integer(210)]
    );
    assert_eq!(
        first_col(&db, "SELECT count(*) FROM t WHERE rowid % 2 = 0"),
        vec![Value::Integer(10)]
    );
}

#[test]
fn rowid_expr_no_alias_table() {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE u (a INTEGER)", []).unwrap();
    for i in 0..10i64 {
        db.execute("INSERT INTO u (a) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    // Trailing hidden rowid slot: every path answers.
    assert_eq!(
        first_col(&db, "SELECT rowid*2 FROM u"),
        (1..=10i64)
            .map(|r| Value::Integer(r * 2))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        first_col(&db, "SELECT rowid*2 FROM u WHERE a = 2"),
        vec![Value::Integer(6)]
    );
    assert_eq!(
        first_col(&db, "SELECT rowid*2 FROM u WHERE a > 7"),
        vec![Value::Integer(18), Value::Integer(20)]
    );
    assert_eq!(
        first_col(&db, "SELECT max(rowid) FROM u"),
        vec![Value::Integer(10)]
    );
    assert_eq!(
        first_col(&db, "SELECT sum(rowid) FROM u"),
        vec![Value::Integer(55)]
    );
    // Predicate referencing rowid.
    assert_eq!(
        first_col(&db, "SELECT a FROM u WHERE rowid % 2 = 0 ORDER BY rowid"),
        vec![
            Value::Integer(1),
            Value::Integer(3),
            Value::Integer(5),
            Value::Integer(7),
            Value::Integer(9)
        ]
    );
    // Rowid-range residual references rowid (strict bounds).
    assert_eq!(
        first_col(&db, "SELECT rowid FROM u WHERE rowid > 2 AND rowid < 6"),
        vec![Value::Integer(3), Value::Integer(4), Value::Integer(5)]
    );
    // star arity: the hidden slot never leaks.
    let (cols, rows) = db.query_with_columns("SELECT * FROM u", []).unwrap();
    assert_eq!(cols, vec!["a".to_string()]);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows.len(), 10);
}

#[test]
fn rowid_expr_no_alias_prepare_step() {
    // The statement/driver API (prepare + step) — same answers.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE u (a INTEGER)", []).unwrap();
    for i in 0..10i64 {
        db.execute("INSERT INTO u (a) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    let mut stmt = db.prepare("SELECT rowid*2 FROM u WHERE a = 2").unwrap();
    let mut got: Vec<Value> = Vec::new();
    while let StepResult::Row = stmt.step().unwrap() {
        got.push(stmt.column_value(0).unwrap().clone());
    }
    assert_eq!(got, vec![Value::Integer(6)]);
}

#[test]
fn real_rowid_column_shadows_pseudo() {
    // A REAL column named `rowid` shadows the pseudo-column (SQLite
    // resolution): expressions read the column, never a rowid.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE s (rowid INTEGER, v INTEGER)", [])
        .unwrap();
    for i in 0..5i64 {
        db.execute(
            "INSERT INTO s (rowid, v) VALUES (?, ?)",
            [Value::Integer(i * 100), Value::Integer(i)],
        )
        .unwrap();
    }
    assert_eq!(
        first_col(&db, "SELECT rowid FROM s ORDER BY rowid"),
        vec![
            Value::Integer(0),
            Value::Integer(100),
            Value::Integer(200),
            Value::Integer(300),
            Value::Integer(400)
        ]
    );
    assert_eq!(
        first_col(&db, "SELECT rowid + 1 FROM s WHERE v = 2"),
        vec![Value::Integer(201)]
    );
}

#[test]
fn rowid_expr_dml_roundtrip() {
    // INSERT ... SELECT rowid: the rewritten hidden name must produce
    // real values through the materialized path.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE src (a INTEGER)", []).unwrap();
    for i in 0..5i64 {
        db.execute("INSERT INTO src (a) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    db.execute("CREATE TABLE dst (r INTEGER)", []).unwrap();
    db.execute("INSERT INTO dst (r) SELECT rowid * 10 FROM src", [])
        .unwrap();
    let got: Vec<i64> = db
        .query("SELECT r FROM dst ORDER BY r", [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => *i,
            v => panic!("INSERT..SELECT rowid gave {v:?}"),
        })
        .collect();
    assert_eq!(got, vec![10, 20, 30, 40, 50]);
}
