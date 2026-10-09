//! DML through the prepared-statement (`prepare` + `step`) and `query`
//! paths — the routes the sqlx driver and the C ABI take for every DML
//! statement:
//!
//! - every clause's parameters bind on the statement (WITH CTEs, UPDATE …
//!   FROM, RETURNING, ORDER BY, LIMIT — they used to be missed, so the
//!   bind failed "parameter index 1 out of range");
//! - a WITH-prefixed INSERT / UPDATE / DELETE writes (it used to step to
//!   DONE, or return an empty Ok from `query`, without writing);
//! - what these `&self` paths cannot run is REPORTED, never a silent
//!   success: DML on a view (INSTEAD OF triggers), DDL, transaction
//!   control and write pragmas name `Database::execute`.

use rustqlite::{Database, StepResult, Value};

fn table(db: &Database) -> Vec<Vec<Value>> {
    db.query("SELECT * FROM t ORDER BY id", []).unwrap()
}

/// Bind `binds` positionally, step to completion, return the rows.
fn step_all(db: &Database, sql: &str, binds: Vec<Value>) -> Vec<Vec<Value>> {
    let mut st = db.prepare(sql).unwrap();
    for (i, b) in binds.into_iter().enumerate() {
        st.bind(i + 1, b)
            .unwrap_or_else(|e| panic!("{sql}: bind {}: {e}", i + 1));
    }
    let mut rows = Vec::new();
    loop {
        match st.step().unwrap_or_else(|e| panic!("{sql}: {e}")) {
            StepResult::Row => rows.push(st.row().unwrap().clone()),
            StepResult::Done => return rows,
        }
    }
}

fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t(v) VALUES (1), (2), (3), (4)", [])
        .unwrap();
    db
}

#[test]
fn every_dml_clause_binds_on_the_statement() {
    let db = setup();
    step_all(
        &db,
        "DELETE FROM t WHERE v > 0 ORDER BY v LIMIT ?",
        vec![Value::Integer(1)],
    );
    assert_eq!(table(&db).len(), 3);
    let rows = step_all(
        &db,
        "UPDATE t SET v = v + 10 WHERE v > ? RETURNING v + ?",
        vec![Value::Integer(2), Value::Integer(100)],
    );
    assert_eq!(
        rows,
        vec![vec![Value::Integer(113)], vec![Value::Integer(114)]]
    );
    step_all(
        &db,
        "UPDATE t SET v = s.x FROM (SELECT ? AS x) s WHERE t.id = ?",
        vec![Value::Integer(55), Value::Integer(2)],
    );
    assert_eq!(table(&db)[0], vec![Value::Integer(2), Value::Integer(55)]);
    step_all(
        &db,
        "WITH k(n) AS (SELECT ?) DELETE FROM t WHERE v IN k ORDER BY id LIMIT ?",
        vec![Value::Integer(55), Value::Integer(5)],
    );
    assert_eq!(table(&db).len(), 2);
}

#[test]
fn cte_prefixed_dml_writes_on_step_and_query() {
    let db = setup();
    step_all(
        &db,
        "WITH x(n) AS (SELECT ?) INSERT INTO t(v) SELECT n FROM x",
        vec![Value::Integer(7)],
    );
    let rows = step_all(
        &db,
        "WITH x(n) AS (SELECT 8) INSERT INTO t(v) SELECT n FROM x RETURNING id, v",
        vec![],
    );
    assert_eq!(rows, vec![vec![Value::Integer(6), Value::Integer(8)]]);
    let (cols, rows) = db
        .query_with_columns(
            "WITH x(n) AS (SELECT 7) UPDATE t SET v = v * 10 WHERE v IN x RETURNING v",
            [],
        )
        .unwrap();
    assert_eq!(cols, vec!["v".to_string()]);
    assert_eq!(rows, vec![vec![Value::Integer(70)]]);
    db.query("WITH x(n) AS (SELECT 8) DELETE FROM t WHERE v IN x", [])
        .unwrap();
    let vs: Vec<i64> = table(&db).iter().map(|r| r[1].as_integer()).collect();
    assert_eq!(vs, vec![1, 2, 3, 4, 70]);
}

#[test]
fn unsupported_statements_are_reported_not_ignored() {
    let mut db = setup();
    db.execute("CREATE VIEW v AS SELECT v AS a FROM t", [])
        .unwrap();
    db.execute(
        "CREATE TRIGGER vi INSTEAD OF INSERT ON v BEGIN INSERT INTO t(v) VALUES (new.a); END",
        [],
    )
    .unwrap();
    // View DML: the drivers route it to execute; stepping it directly
    // reports it (it used to step to DONE without running the trigger).
    let mut st = db.prepare("INSERT INTO v VALUES (9)").unwrap();
    assert!(st.targets_view());
    let err = st.step().unwrap_err().to_string();
    assert!(err.contains("Database::execute"), "{err}");
    drop(st);
    let err = db
        .query("INSERT INTO v VALUES (9)", [])
        .unwrap_err()
        .to_string();
    assert!(err.contains("Database::execute"), "{err}");
    assert_eq!(table(&db).len(), 4);
    db.execute("INSERT INTO v VALUES (9)", []).unwrap();
    assert_eq!(table(&db).len(), 5);
    // DDL / transaction control / write pragma through query().
    for sql in [
        "CREATE TABLE u (a)",
        "BEGIN",
        "PRAGMA user_version = 3",
        "VACUUM",
    ] {
        let err = db.query(sql, []).unwrap_err().to_string();
        assert!(err.contains("Database::execute"), "{sql}: {err}");
    }
    assert!(db
        .query("SELECT name FROM sqlite_master WHERE name = 'u'", [])
        .unwrap()
        .is_empty());
    // Read pragmas (including over a missing object) still answer.
    assert!(db
        .query("PRAGMA table_info(nonexistent)", [])
        .unwrap()
        .is_empty());
    assert_eq!(
        db.query("PRAGMA user_version", []).unwrap()[0][0].as_integer(),
        0
    );
}
