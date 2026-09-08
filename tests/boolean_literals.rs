//! SQLite 3.23+ boolean literals (TRUE / FALSE) — regression tests.
//!
//! History: the lexer's KEYWORDS table lacked TRUE/FALSE, so they lexed
//! as plain identifiers. The INSERT fast-path byte scanner recognized
//! them (bare `VALUES (TRUE)` stored 1), but every general expression
//! path evaluated the identifier to NULL: `SELECT TRUE` → NULL,
//! `WHERE a = TRUE` matched nothing, and — the bug that surfaced this —
//! sqlx 0.9's migrator writes
//! `INSERT INTO _sqlx_migrations (...) VALUES (?1, ?2, TRUE, ?3, -1)`,
//! where the mixed bind/literal row goes through the general path and
//! stored NULL into the `success BOOLEAN NOT NULL` column, failing the
//! whole migration with `NOT NULL constraint failed`.
//!
//! Fix: TRUE/FALSE are keywords (parser's `Expr::Literal(1/0)` arm);
//! they keep working as unquoted column names in DDL via the
//! `parse_ident_or_keyword` fallback (SQLite's `%fallback ID`).

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().unwrap()
}

#[test]
fn boolean_literals_select() {
    let db = mem();
    let rows = db
        .query("SELECT TRUE, FALSE, typeof(TRUE), typeof(FALSE)", ())
        .unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Integer(1),
            Value::Integer(0),
            Value::Text("integer".into()),
            Value::Text("integer".into()),
        ]
    );
}

#[test]
fn boolean_literals_in_comparisons() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INTEGER)", ()).unwrap();
    db.execute("INSERT INTO t VALUES (1), (0), (2)", ())
        .unwrap();

    let rows = db
        .query(
            "SELECT 1 = TRUE, 0 = FALSE, TRUE AND FALSE, TRUE OR FALSE",
            (),
        )
        .unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(1)
        ]
    );

    // WHERE with the literal on either side.
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE a = TRUE", ())
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
    let rows = db.query("SELECT COUNT(*) FROM t WHERE TRUE", ()).unwrap();
    assert_eq!(rows[0][0], Value::Integer(3));

    // NOT TRUE / IS NOT TRUE keep SQLite semantics.
    let rows = db.query("SELECT (NOT TRUE), (1 IS NOT TRUE)", ()).unwrap();
    assert_eq!(rows[0], vec![Value::Integer(0), Value::Integer(0)]);
}

#[test]
fn boolean_literal_mixed_with_binds_not_null() {
    // The exact sqlx-migrator failure shape: a VALUES row mixing bound
    // parameters with the bare TRUE literal, into a NOT NULL column.
    let mut db = mem();
    db.execute(
        "CREATE TABLE _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        )",
        (),
    )
    .unwrap();
    db.execute(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (?1, ?2, TRUE, ?3, -1)",
        [
            Value::Integer(1),
            Value::Text("create_users".into()),
            Value::Blob(vec![1, 2, 3]),
        ],
    )
    .expect("mixed binds + TRUE literal must insert");

    let rows = db
        .query(
            "SELECT version, success, typeof(success) FROM _sqlx_migrations",
            (),
        )
        .unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Integer(1),
            Value::Integer(1),
            Value::Text("integer".into()),
        ]
    );

    // FALSE literal alongside binds, into the same NOT NULL column.
    db.execute(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (?1, ?2, FALSE, ?3, -1)",
        [
            Value::Integer(2),
            Value::Text("failed".into()),
            Value::Blob(vec![9]),
        ],
    )
    .unwrap();
    let rows = db
        .query("SELECT success FROM _sqlx_migrations WHERE version = 2", ())
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(0));
}

#[test]
fn boolean_literal_default_and_check() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, flag BOOLEAN NOT NULL DEFAULT TRUE)",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO t (id) VALUES (1)", ()).unwrap();
    let rows = db.query("SELECT flag FROM t", ()).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1), "DEFAULT TRUE fills 1");

    // CHECK constraint written with the keyword literal.
    db.execute("CREATE TABLE c (x INTEGER CHECK (x > 0 OR FALSE))", ())
        .unwrap();
    db.execute("INSERT INTO c VALUES (5)", ()).unwrap();
    let err = db.execute("INSERT INTO c VALUES (-5)", ()).unwrap_err();
    assert!(format!("{err}").contains("CHECK"), "CHECK fires: {err}");
}

#[test]
fn boolean_literal_case_when_and_upsert() {
    let mut db = mem();
    let rows = db
        .query("SELECT CASE WHEN TRUE THEN 'yes' ELSE 'no' END", ())
        .unwrap();
    assert_eq!(rows[0][0], Value::Text("yes".into()));

    db.execute("CREATE TABLE t (k INTEGER PRIMARY KEY, a INTEGER)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 0)", ()).unwrap();
    db.execute(
        "INSERT INTO t (k, a) VALUES (1, TRUE)
         ON CONFLICT(k) DO UPDATE SET a = excluded.a WHERE TRUE",
        (),
    )
    .unwrap();
    let rows = db.query("SELECT a FROM t", ()).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
}

#[test]
fn true_false_still_usable_as_column_names() {
    // SQLite's %fallback ID: TRUE/FALSE remain legal unquoted column
    // names in DDL (parse_ident_or_keyword accepts keyword tokens).
    let mut db = mem();
    db.execute("CREATE TABLE t (true INTEGER, false TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (42, 'x')", ()).unwrap();
    let rows = db.query("SELECT \"true\", \"false\" FROM t", ()).unwrap();
    assert_eq!(rows[0], vec![Value::Integer(42), Value::Text("x".into())]);
}
