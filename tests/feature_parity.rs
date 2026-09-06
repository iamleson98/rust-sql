//! SQLite feature-parity regression tests for the surface added in the
//! parity push: CTAS, STRICT tables, table-valued functions (json_each /
//! json_tree / pragma_*), expression indexes, UPDATE/DELETE ORDER BY +
//! LIMIT, multi-statement execute, and last_insert_rowid().

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().unwrap()
}

// ---------------------------------------------------------------------------
// CREATE TABLE ... AS SELECT
// ---------------------------------------------------------------------------

#[test]
fn ctas_bare_column_names_from_select() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE src(id INTEGER PRIMARY KEY, name TEXT, score REAL)",
        (),
    )
    .unwrap();
    db.execute(
        "INSERT INTO src(id, name, score) VALUES (1,'alice',9.5), (2,'bob',8.0), (3,'carol',NULL)",
        (),
    )
    .unwrap();
    db.execute("CREATE TABLE copy1 AS SELECT * FROM src", ())
        .unwrap();
    let rows = db
        .query("SELECT id, name, score FROM copy1 ORDER BY id", ())
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1], Value::Text("alice".into()));
    assert_eq!(rows[0][2], Value::Real(9.5));
    assert_eq!(rows[2][2], Value::Null);
}

#[test]
fn ctas_declared_column_list() {
    let mut db = mem();
    db.execute("CREATE TABLE src(a INT, b TEXT)", ()).unwrap();
    db.execute("INSERT INTO src VALUES (1,'x'), (2,'y')", ())
        .unwrap();
    db.execute(
        "CREATE TABLE renamed(p, q) AS SELECT a, b FROM src WHERE a = 1",
        (),
    )
    .unwrap();
    let rows = db.query("SELECT p, q FROM renamed", ()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Text("x".into()));
}

#[test]
fn ctas_aggregate_and_empty_shapes() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT)", ()).unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3), (4)", ())
        .unwrap();
    db.execute(
        "CREATE TABLE agg AS SELECT count(*) AS n, avg(a) AS m FROM t",
        (),
    )
    .unwrap();
    let rows = db.query("SELECT n, m FROM agg", ()).unwrap();
    assert_eq!(rows[0][0], Value::Integer(4));
    assert_eq!(rows[0][1], Value::Real(2.5));

    db.execute("CREATE TABLE empty AS SELECT a FROM t WHERE 0", ())
        .unwrap();
    let n = db.query("SELECT count(*) FROM empty", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(0));
}

#[test]
fn ctas_column_count_mismatch_errors() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT, b TEXT)", ()).unwrap();
    let e = db
        .execute("CREATE TABLE bad(x) AS SELECT a, b FROM t", ())
        .unwrap_err();
    assert!(
        e.to_string().contains("2 columns but 2 values") || e.to_string().contains("columns but"),
        "got: {e}"
    );
}

#[test]
fn ctas_constraints_rejected() {
    let mut db = mem();
    let e = db
        .execute("CREATE TABLE bad(a INT PRIMARY KEY) AS SELECT 1", ())
        .unwrap_err();
    assert!(
        e.to_string().contains("constraints are not allowed"),
        "got: {e}"
    );
}

#[test]
fn ctas_persist_and_reopen() {
    let path = std::env::temp_dir().join("rq_ctas_reopen_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut file = Database::open(&path).unwrap();
        file.execute(
            "CREATE TABLE t AS SELECT 1 AS one, 'x' AS two, 3.5 AS three",
            (),
        )
        .unwrap();
    }
    let mut file = Database::open(&path).unwrap();
    let rows = file.query("SELECT one, two, three FROM t", ()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[0][1], Value::Text("x".into()));
    assert_eq!(rows[0][2], Value::Real(3.5));
    // And further inserts into the CTAS table work (columns round-trip).
    file.execute("INSERT INTO t VALUES (2, 'y', 1.0)", ())
        .unwrap();
    let n = file.query("SELECT count(*) FROM t", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(2));
}

// ---------------------------------------------------------------------------
// STRICT tables
// ---------------------------------------------------------------------------

#[test]
fn strict_rejects_wrong_type_insert() {
    let mut db = mem();
    db.execute("CREATE TABLE ts (a INTEGER, b TEXT) STRICT", ())
        .unwrap();
    db.execute("INSERT INTO ts VALUES (1, 'x')", ()).unwrap();
    let e = db
        .execute("INSERT INTO ts VALUES ('text', 'y')", ())
        .unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("cannot store"), "got: {msg}");
}

#[test]
fn strict_rejects_wrong_type_update() {
    let mut db = mem();
    db.execute("CREATE TABLE ts (a INTEGER) STRICT", ())
        .unwrap();
    db.execute("INSERT INTO ts VALUES (1)", ()).unwrap();
    let e = db
        .execute("UPDATE ts SET a = 'not an int'", ())
        .unwrap_err();
    assert!(e.to_string().contains("cannot store"), "got: {}", e);
}

#[test]
fn strict_real_folds_integers() {
    let mut db = mem();
    db.execute("CREATE TABLE tr (a REAL) STRICT", ()).unwrap();
    db.execute("INSERT INTO tr VALUES (5)", ()).unwrap();
    let rows = db.query("SELECT typeof(a), a FROM tr", ()).unwrap();
    assert_eq!(rows[0][0], Value::Text("real".into()));
    assert_eq!(rows[0][1], Value::Real(5.0));
}

#[test]
fn strict_any_accepts_everything() {
    let mut db = mem();
    db.execute("CREATE TABLE ta (a ANY) STRICT", ()).unwrap();
    db.execute(
        "INSERT INTO ta VALUES (1), ('x'), (1.5), (x'00ff'), (NULL)",
        (),
    )
    .unwrap();
    let n = db.query("SELECT count(*) FROM ta", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(5));
}

#[test]
fn strict_unknown_datatype_rejected() {
    let mut db = mem();
    let e = db
        .execute("CREATE TABLE bad (a VARCHAR(10)) STRICT", ())
        .unwrap_err();
    assert!(e.to_string().contains("unknown datatype"), "got: {e}");
}

#[test]
fn strict_persists_across_reopen() {
    let path = std::env::temp_dir().join("rq_strict_reopen_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut file = Database::open(&path).unwrap();
        file.execute("CREATE TABLE ts (a INTEGER) STRICT", ())
            .unwrap();
        file.execute("INSERT INTO ts VALUES (42)", ()).unwrap();
    }
    let mut file = Database::open(&path).unwrap();
    let e = file
        .execute("INSERT INTO ts VALUES ('nope')", ())
        .unwrap_err();
    assert!(e.to_string().contains("cannot store"), "got: {e}");
}

// ---------------------------------------------------------------------------
// Table-valued functions
// ---------------------------------------------------------------------------

#[test]
fn json_each_one_level() {
    let db = mem();
    let rows = db
        .query("SELECT key, value, type FROM json_each('[1, 2, 3]')", ())
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Integer(0));
    assert_eq!(rows[1][0], Value::Integer(1));
    assert_eq!(rows[2][2], Value::Text("integer".into()));
}

#[test]
fn json_each_object_keys() {
    let db = mem();
    let rows = db
        .query(
            "SELECT key, value, fullkey FROM json_each('{\"a\": 1, \"b\": 2}')",
            (),
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Text("a".into()));
    assert_eq!(rows[0][1], Value::Integer(1));
    assert_eq!(rows[1][2], Value::Text("$.b".into()));
}

#[test]
fn json_each_nested_not_walked() {
    let db = mem();
    // json_each yields only the DIRECT children: one row for the inner
    // object, none for its contents.
    let rows = db
        .query(
            "SELECT count(*) FROM json_each('{\"outer\": {\"inner\": 1}}')",
            (),
        )
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
}

#[test]
fn json_tree_recursive() {
    let db = mem();
    let rows = db
        .query("SELECT path, fullkey FROM json_tree('{\"a\": [1, 2]}')", ())
        .unwrap();
    // Root object + "a" array + two elements.
    assert_eq!(rows.len(), 4);
}

#[test]
fn json_each_null_and_malformed() {
    let db = mem();
    let rows = db
        .query("SELECT count(*) FROM json_each(NULL)", ())
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(0));
    let e = db
        .query("SELECT count(*) FROM json_each('{oops}')", ())
        .unwrap_err();
    assert!(e.to_string().contains("malformed"), "got: {e}");
}

#[test]
fn json_each_with_alias() {
    let db = mem();
    let rows = db
        .query(
            "SELECT je.value FROM json_each('[10, 20]') je WHERE je.key = 1",
            (),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(20));
}

#[test]
fn json_each_bound_parameter_argument() {
    let db = mem();
    let rows = db
        .query(
            "SELECT value FROM json_each(?)",
            [Value::Text("[7, 8]".into())],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(7));
}

#[test]
fn pragma_table_info_function() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE p (a INTEGER PRIMARY KEY, b TEXT COLLATE NOCASE)",
        (),
    )
    .unwrap();
    let rows = db
        .query("SELECT name, type, pk FROM pragma_table_info('p')", ())
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Text("a".into()));
    assert_eq!(rows[0][1], Value::Text("INTEGER".into()));
    assert_eq!(rows[0][2], Value::Integer(1));
    assert_eq!(rows[1][2], Value::Integer(0));
}

#[test]
fn pragma_index_list_and_info_functions() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT, b TEXT)", ()).unwrap();
    db.execute("CREATE INDEX ti ON t (b)", ()).unwrap();
    let rows = db
        .query("SELECT name, \"unique\" FROM pragma_index_list('t')", ())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Text("ti".into()));
    let info = db
        .query("SELECT cid, name FROM pragma_index_info('ti')", ())
        .unwrap();
    assert_eq!(info[0][1], Value::Text("b".into()));
}

#[test]
fn pragma_foreign_key_list_function() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE)",
        (),
    )
    .unwrap();
    db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", ())
        .unwrap();
    let rows = db
        .query(
            "SELECT \"table\", \"from\", on_delete FROM pragma_foreign_key_list('child')",
            (),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Text("parent".into()));
    assert_eq!(rows[0][1], Value::Text("pid".into()));
    assert_eq!(rows[0][2], Value::Text("CASCADE".into()));
}

#[test]
fn unknown_table_function_errors() {
    let db = mem();
    let e = db
        .query("SELECT * FROM no_such_function('x')", ())
        .unwrap_err();
    assert!(
        e.to_string().contains("no such table-valued function"),
        "got: {e}"
    );
}

// ---------------------------------------------------------------------------
// Expression indexes
// ---------------------------------------------------------------------------

#[test]
fn expression_index_create_and_unique_enforce() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT, b INT)", ()).unwrap();
    db.execute("INSERT INTO t VALUES (1, 2), (3, 4)", ())
        .unwrap();
    // Sum-index: rows (1,2) and (2,1) collide on the expression key.
    db.execute("CREATE UNIQUE INDEX isum ON t (a + b)", ())
        .unwrap();
    let e = db
        .execute("INSERT INTO t VALUES (0, 3)", ()) // 0+3 == 1+2
        .unwrap_err();
    assert!(e.to_string().contains("UNIQUE"), "got: {e}");
    // Non-colliding insert succeeds.
    db.execute("INSERT INTO t VALUES (10, 10)", ()).unwrap();
}

#[test]
fn expression_index_maintained_on_update() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT, b INT)", ()).unwrap();
    db.execute("INSERT INTO t VALUES (1, 2)", ()).unwrap();
    db.execute("CREATE UNIQUE INDEX isum ON t (a + b)", ())
        .unwrap();
    // Moving to 5+5=10 must free the 3 slot.
    db.execute("UPDATE t SET a = 5, b = 5", ()).unwrap();
    db.execute("INSERT INTO t VALUES (1, 2)", ()).unwrap();
    let n = db.query("SELECT count(*) FROM t", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(2));
}

#[test]
fn expression_index_function_keys() {
    let mut db = mem();
    db.execute("CREATE TABLE t (name TEXT)", ()).unwrap();
    db.execute("INSERT INTO t VALUES ('Alice'), ('BOB')", ())
        .unwrap();
    db.execute("CREATE UNIQUE INDEX ilower ON t (lower(name))", ())
        .unwrap();
    // lower('alice') == lower('Alice') → unique violation.
    let e = db
        .execute("INSERT INTO t VALUES ('alice')", ())
        .unwrap_err();
    assert!(e.to_string().contains("UNIQUE"), "got: {e}");
}

#[test]
fn expression_index_persists_across_reopen() {
    let path = std::env::temp_dir().join("rq_expridx_reopen_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut file = Database::open(&path).unwrap();
        file.execute("CREATE TABLE t (a INT, b INT)", ()).unwrap();
        file.execute("INSERT INTO t VALUES (1, 2)", ()).unwrap();
        file.execute("CREATE UNIQUE INDEX isum ON t (a + b)", ())
            .unwrap();
    }
    let mut file = Database::open(&path).unwrap();
    let e = file
        .execute("INSERT INTO t VALUES (2, 1)", ()) // 2+1 == 1+2
        .unwrap_err();
    assert!(e.to_string().contains("UNIQUE"), "got: {e}");
}

#[test]
fn plain_collate_index_still_works() {
    // Regression guard: the parse_indexed_column rewrite must keep the
    // `col COLLATE x [ASC|DESC]` grammar path intact.
    let mut db = mem();
    db.execute("CREATE TABLE t (email TEXT)", ()).unwrap();
    db.execute("INSERT INTO t VALUES ('A@x')", ()).unwrap();
    db.execute("CREATE UNIQUE INDEX ie ON t (email COLLATE NOCASE)", ())
        .unwrap();
    let e = db.execute("INSERT INTO t VALUES ('a@X')", ()).unwrap_err();
    assert!(e.to_string().contains("UNIQUE"), "got: {e}");
    // DESC index columns still parse.
    db.execute("CREATE INDEX idsc ON t (email DESC)", ())
        .unwrap();
}

// ---------------------------------------------------------------------------
// UPDATE / DELETE ORDER BY + LIMIT
// ---------------------------------------------------------------------------

#[test]
fn update_limit_applies_to_matched_rows() {
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)", ())
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES (1), (2), (3), (4)", ())
        .unwrap();
    db.execute("UPDATE t SET v = 99 WHERE id > 1 LIMIT 2", ())
        .unwrap();
    let rows = db.query("SELECT id, v FROM t ORDER BY id", ()).unwrap();
    assert_eq!(rows[0][1], Value::Integer(1));
    assert_eq!(rows[1][1], Value::Integer(99));
    assert_eq!(rows[2][1], Value::Integer(99));
    assert_eq!(rows[3][1], Value::Integer(4));
}

#[test]
fn update_order_by_limit_combination() {
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)", ())
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES (1), (2), (3), (4)", ())
        .unwrap();
    db.execute(
        "UPDATE t SET v = 0 WHERE id > 0 ORDER BY id DESC LIMIT 2",
        (),
    )
    .unwrap();
    let rows = db.query("SELECT id, v FROM t ORDER BY id", ()).unwrap();
    assert_eq!(rows[2][1], Value::Integer(0));
    assert_eq!(rows[3][1], Value::Integer(0));
    assert_eq!(rows[0][1], Value::Integer(1));
}

#[test]
fn delete_limit_enforced() {
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)", ())
        .unwrap();
    db.execute("DELETE FROM t WHERE id > 0 LIMIT 2", ())
        .unwrap();
    let n = db.query("SELECT count(*) FROM t", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(3));
}

#[test]
fn update_limit_with_from_rejected() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT)", ()).unwrap();
    db.execute("CREATE TABLE s (a INT, b INT)", ()).unwrap();
    let e = db
        .execute("UPDATE t SET a = s.b FROM s WHERE t.a = s.a LIMIT 1", ())
        .unwrap_err();
    assert!(
        e.to_string().contains("not supported with UPDATE ... FROM"),
        "got: {e}"
    );
}

// ---------------------------------------------------------------------------
// Multi-statement execute (sqlite3_exec semantics)
// ---------------------------------------------------------------------------

#[test]
fn multi_statement_script_executes_in_order() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE ts (a INTEGER) STRICT; INSERT INTO ts VALUES (1)",
        (),
    )
    .unwrap();
    let n = db.query("SELECT count(*) FROM ts", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(1));
}

#[test]
fn multi_statement_script_stops_at_first_error() {
    let mut db = mem();
    let e = db
        .execute(
            "CREATE TABLE t (a INT); INSERT INTO nope VALUES (1); INSERT INTO t VALUES (2)",
            (),
        )
        .unwrap_err();
    assert!(e.to_string().contains("nope"), "got: {e}");
    // First statement applied, the failing one and the rest not.
    let n = db.query("SELECT count(*) FROM t", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(0));
}

#[test]
fn multi_statement_respects_literals_and_triggers() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a TEXT); CREATE TABLE log (msg TEXT)", ())
        .unwrap();
    // Semicolons inside string literals must not split.
    db.execute("INSERT INTO t VALUES ('a;b;c')", ()).unwrap();
    let rows = db.query("SELECT a FROM t", ()).unwrap();
    assert_eq!(rows[0][0], Value::Text("a;b;c".into()));
    // A trigger body's inner statements stay inside the trigger.
    db.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO log(msg) VALUES ('hit'); END",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES ('x')", ()).unwrap();
    let n = db.query("SELECT count(*) FROM log", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(1));
}

#[test]
fn single_statement_with_trailing_semicolon_unchanged() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT);", ()).unwrap();
    let n = db.query("SELECT count(*) FROM t", ()).unwrap();
    assert_eq!(n[0][0], Value::Integer(0));
}

// ---------------------------------------------------------------------------
// last_insert_rowid()
// ---------------------------------------------------------------------------

#[test]
fn last_insert_rowid_sql_function() {
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .unwrap();
    let before = db.query("SELECT last_insert_rowid()", ()).unwrap();
    assert_eq!(before[0][0], Value::Integer(0));
    db.execute("INSERT INTO t (v) VALUES ('a'), ('b'), ('c')", ())
        .unwrap();
    let after = db.query("SELECT last_insert_rowid()", ()).unwrap();
    // SQLite: the rowid of the LAST row inserted by the statement.
    assert_eq!(after[0][0], Value::Integer(3));
}

#[test]
fn last_insert_rowid_is_per_connection() {
    let mut db_a = mem();
    let mut db_b = mem();
    for db in [&mut db_a, &mut db_b] {
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", ())
            .unwrap();
    }
    db_a.execute("INSERT INTO t VALUES (7)", ()).unwrap();
    // db_b never inserted: its function call must NOT see db_a's rowid.
    let v = db_b.query("SELECT last_insert_rowid()", ()).unwrap();
    assert_eq!(v[0][0], Value::Integer(0));
    let v = db_a.query("SELECT last_insert_rowid()", ()).unwrap();
    assert_eq!(v[0][0], Value::Integer(7));
}

#[test]
fn last_insert_rowid_rust_api_matches_sql_function() {
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (41)", ()).unwrap();
    let api = db.last_insert_rowid();
    let sql = db
        .query("SELECT last_insert_rowid()", ())
        .unwrap()
        .pop()
        .and_then(|r| r.into_iter().next());
    assert_eq!(api, 41);
    assert_eq!(sql, Some(Value::Integer(41)));
}

// ---------------------------------------------------------------------------
// Statement-splitting edge cases
// ---------------------------------------------------------------------------

#[test]
fn split_script_comments_and_literals() {
    use rustqlite::sql::parser::split_script;
    let parts = split_script(
        "-- leading comment\nCREATE TABLE a (x INT); /* block ; comment */ CREATE TABLE b (y INT);\nINSERT INTO a VALUES ('semi;colon')",
    );
    assert_eq!(parts.len(), 3);
    assert!(parts[0].starts_with("--") || parts[0].contains("CREATE TABLE a"));
    assert!(parts[2].contains("'semi;colon'"));
    let trig = split_script(
        "CREATE TRIGGER g AFTER INSERT ON a BEGIN INSERT INTO b VALUES (1); INSERT INTO b VALUES (2); END; INSERT INTO a VALUES (9)",
    );
    assert_eq!(trig.len(), 2);
    assert!(trig[0].starts_with("CREATE TRIGGER"));
    assert!(trig[1].starts_with("INSERT INTO a"));
}

#[test]
fn split_script_transaction_begin_unaffected() {
    use rustqlite::sql::parser::split_script;
    // A plain BEGIN TRANSACTION must not swallow the following statements
    // (only CREATE TRIGGER blocks suppress inner semicolons).
    let parts = split_script("BEGIN; INSERT INTO t VALUES (1); COMMIT;");
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], "BEGIN");
}

// ---------------------------------------------------------------------------
// Rowid pseudo-column (SELECT list / range cursors)
// ---------------------------------------------------------------------------

#[test]
fn rowid_pseudo_column_select() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a TEXT, b INT)", ()).unwrap();
    db.execute(
        "INSERT INTO t (rowid, a, b) VALUES (10, 'x', 1), (25, 'y', 2)",
        (),
    )
    .unwrap();
    let rows = db.query("SELECT rowid, a FROM t", ()).unwrap();
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[1][0], Value::Integer(25));
    // _rowid_ / oid spellings + alias
    let rows = db
        .query("SELECT _rowid_ AS r FROM t ORDER BY r", ())
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(10));
    let rows = db.query("SELECT oid FROM t WHERE oid = 25", ()).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn rowid_pseudo_column_range_cursor() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a TEXT)", ()).unwrap();
    for i in 0..50i64 {
        db.execute(
            "INSERT INTO t (rowid, a) VALUES (?, ?)",
            [Value::Integer(i), Value::Text(format!("v{i}").into())],
        )
        .unwrap();
    }
    let rows = db
        .query(
            "SELECT a FROM t WHERE rowid > 10 ORDER BY rowid LIMIT 5",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0][0], Value::Text("v11".into()));
    // Strict bound excludes the boundary row.
    let rows = db
        .query("SELECT rowid FROM t WHERE rowid >= 10 AND rowid <= 12", [])
        .unwrap();
    assert_eq!(rows.len(), 3);
    // Pseudo-rowid projection fused with a strict range residual.
    let rows = db
        .query("SELECT rowid, a FROM t WHERE rowid > 45 ORDER BY rowid", [])
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0], Value::Integer(46));
}

#[test]
fn rowid_pseudo_with_alias_table() {
    // INTEGER PRIMARY KEY table: rowid IS the alias value.
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (7, 'seven')", ()).unwrap();
    let rows = db.query("SELECT rowid, id, a FROM t", ()).unwrap();
    assert_eq!(rows[0][0], Value::Integer(7));
    assert_eq!(rows[0][1], Value::Integer(7));
    let rows = db.query("SELECT a FROM t WHERE rowid = 7", ()).unwrap();
    assert_eq!(rows[0][0], Value::Text("seven".into()));
}

// ---------------------------------------------------------------------------
// VACUUM
// ---------------------------------------------------------------------------

fn vacuum_churn(db: &mut rustqlite::Database) {
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, f REAL)",
        (),
    )
    .unwrap();
    db.execute("CREATE INDEX iv ON t (v)", ()).unwrap();
    db.execute("CREATE INDEX isum ON t (id + f)", ()).unwrap();
    for i in 1..=500i64 {
        db.execute(
            "INSERT INTO t (v, f) VALUES (?, ?)",
            [Value::Text(format!("row{i}").into()), Value::Real(i as f64)],
        )
        .unwrap();
    }
    db.execute("DELETE FROM t WHERE id <= 400", []).unwrap();
}

#[test]
fn vacuum_in_memory_compacts_and_preserves() {
    let mut db = mem();
    vacuum_churn(&mut db);
    let pages_before: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    db.execute("VACUUM", []).unwrap();
    let pages_after: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
    assert!(
        pages_after < pages_before,
        "{pages_after} !< {pages_before}"
    );
    // Rowids, values, and all three indexes intact.
    let rows = db
        .query("SELECT id, v, f FROM t WHERE v = 'row500'", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(500));
    assert_eq!(rows[0][2], Value::Real(500.0));
    let n = db
        .query("SELECT COUNT(*) FROM t WHERE v > 'row4'", [])
        .unwrap();
    assert_eq!(n[0][0], Value::Integer(100));
    // rowid generation continues past the vacuumed state.
    db.execute("INSERT INTO t (v, f) VALUES ('new', 1.5)", [])
        .unwrap();
    let rows = db.query("SELECT id FROM t WHERE v = 'new'", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(501));
}

#[test]
fn vacuum_file_backed_compacts_and_reopens() {
    let path = std::env::temp_dir().join("rq_vacuum_file_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = rustqlite::Database::open(&path).unwrap();
        vacuum_churn(&mut db);
        let before = std::fs::metadata(&path).unwrap().len();
        db.execute("VACUUM", []).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "file {after} !< {before}");
        // Same handle keeps working.
        let rows = db.query("SELECT id FROM t WHERE v = 'row499'", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(499));
    }
    // Fresh open from disk: compact state is durable.
    let db = rustqlite::Database::open(&path).unwrap();
    let rows = db
        .query("SELECT id, v FROM t WHERE v = 'row500'", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(500));
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(100));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn vacuum_into_writes_standalone_copy() {
    let mut db = mem();
    vacuum_churn(&mut db);
    let into = std::env::temp_dir().join("rq_vacuum_into_test.db");
    let _ = std::fs::remove_file(&into);
    let sql = format!("VACUUM INTO '{}'", into.display());
    db.execute(sql.as_str(), []).unwrap();
    // Source untouched.
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(100));
    // Target is a complete standalone database.
    let tgt = rustqlite::Database::open(&into).unwrap();
    let rows = tgt
        .query("SELECT id, v FROM t WHERE v = 'row498'", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(498));
    let n = tgt.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(100));
    let _ = std::fs::remove_file(&into);
}

#[test]
fn vacuum_refused_inside_transaction() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT)", ()).unwrap();
    db.execute("BEGIN", []).unwrap();
    let e = db.execute("VACUUM", []).unwrap_err();
    assert!(e.to_string().contains("transaction"), "got: {e}");
    db.execute("COMMIT", []).unwrap();
    db.execute("VACUUM", []).unwrap();
}

#[test]
fn vacuum_into_refuses_existing_file() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT)", ()).unwrap();
    let into = std::env::temp_dir().join("rq_vacuum_exists_test.db");
    let _ = std::fs::remove_file(&into);
    std::fs::write(&into, b"x").unwrap();
    let sql = format!("VACUUM INTO '{}'", into.display());
    let e = db.execute(sql.as_str(), []).unwrap_err();
    assert!(e.to_string().contains("existing"), "got: {e}");
    let _ = std::fs::remove_file(&into);
}

/// WAL-mode file VACUUM takes the IN-PLACE install path (one WAL commit +
/// checkpoint — no pager replacement, no reopen). This is SQLite's own
/// VACUUM shape, and it has the sharpest failure modes:
///   1. the compact image must land as WAL frames (crash-consistent),
///   2. the checkpoint must truncate the file tail (pages >= new_n gone),
///   3. the SAME handle must keep working (catalog reloaded from the new
///      roots; every pre-VACUUM cache — plans, root overrides, count
///      memo, join builds, committed-view scopes — invalidated),
///   4. a fresh open must see the compact state with no stale sidecar
///      frames replaying the OLD tree over the new image.
#[test]
fn vacuum_wal_mode_in_place_install() {
    let path = std::env::temp_dir().join("rq_vacuum_wal_test.db");
    let wal_path = {
        let mut p = path.clone();
        p.set_extension("db-wal");
        p
    };
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal_path);
    {
        let mut db = rustqlite::Database::open(&path).unwrap();
        let mode: String = db.query("PRAGMA journal_mode = WAL", []).unwrap()[0][0]
            .as_text()
            .to_string();
        assert_eq!(mode, "wal");

        vacuum_churn(&mut db);
        let before = std::fs::metadata(&path).unwrap().len();
        db.execute("VACUUM", []).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "file {after} !< {before}");

        // Same handle: catalog adopted from the compact roots, all
        // statement-level caches dropped, values/indexes intact.
        let rows = db
            .query("SELECT id, v, f FROM t WHERE v = 'row499'", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(499));
        assert_eq!(rows[0][2], Value::Real(499.0));
        let n = db
            .query("SELECT COUNT(*) FROM t WHERE id + f > 990", [])
            .unwrap(); // exercises the expression index's new root (ids 496..500)
        assert_eq!(n[0][0], Value::Integer(5));
        // rowid generation continues past the vacuumed state.
        db.execute("INSERT INTO t (v, f) VALUES ('new', 2.5)", [])
            .unwrap();
        let rows = db.query("SELECT id FROM t WHERE v = 'new'", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(501));
        // Post-VACUUM writes keep landing in WAL and survive a reopen.
        db.execute("INSERT INTO t (v, f) VALUES ('new2', 3.5)", [])
            .unwrap();
    }
    // Fresh open from disk: compact state + the post-VACUUM rows durable.
    let db = rustqlite::Database::open(&path).unwrap();
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(102));
    let rows = db
        .query(
            "SELECT id FROM t WHERE v IN ('new', 'new2') ORDER BY id",
            [],
        )
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(501));
    assert_eq!(rows[1][0], Value::Integer(502));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal_path);
}

/// WAL-mode VACUUM with a schema big enough that page-level remap moved
/// table roots: a fresh connection's PRAGMA integrity-style round trip
/// (page_count + freelist_count) must show the compact state (empty
/// freelist, page_count == live pages only).
#[test]
fn vacuum_wal_mode_freelist_drained() {
    let path = std::env::temp_dir().join("rq_vacuum_wal2_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = rustqlite::Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        vacuum_churn(&mut db);
        db.execute("VACUUM", []).unwrap();
        let freelist: i64 = db.query("PRAGMA freelist_count", []).unwrap()[0][0].as_integer();
        assert_eq!(freelist, 0, "freelist must be drained after VACUUM");
        let pages: i64 = db.query("PRAGMA page_count", []).unwrap()[0][0].as_integer();
        assert!(pages > 0);
    }
    let db = rustqlite::Database::open(&path).unwrap();
    let freelist: i64 = db.query("PRAGMA freelist_count", []).unwrap()[0][0].as_integer();
    assert_eq!(freelist, 0, "freelist drained on reopen too");
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(100));
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// NUMBERED POSITIONAL PARAMETERS (?N)
// ---------------------------------------------------------------------------

/// Regression: `?N` was resolved as params[N] (0-based) instead of
/// params[N-1] (SQLite numbers ?N 1-BASED: ?1 is the FIRST parameter).
/// Every `INSERT ... VALUES (?1, ...)` statement silently inserted NULL.
#[test]
fn numbered_params_one_based() {
    let mut db = mem();
    db.execute("CREATE TABLE r (id INTEGER PRIMARY KEY, v INT, w INT)", ())
        .unwrap();
    // ?1 / ?2 / mixed forms.
    db.execute(
        "INSERT INTO r (v, w) VALUES (?1, ?2)",
        [Value::Integer(7), Value::Integer(8)],
    )
    .unwrap();
    let rows = db.query("SELECT v, w FROM r WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(7));
    assert_eq!(rows[0][1], Value::Integer(8));

    // Repeated ?N binds the same slot.
    let rows = db
        .query(
            "SELECT ?1, ?1, ?2",
            [Value::Integer(10), Value::Integer(20)],
        )
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[0][1], Value::Integer(10));
    assert_eq!(rows[0][2], Value::Integer(20));

    // Out-of-order appearance still binds by number.
    let rows = db
        .query("SELECT ?2, ?1", [Value::Integer(1), Value::Integer(2)])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(2));
    assert_eq!(rows[0][1], Value::Integer(1));

    // WHERE / SET / UPDATE paths.
    let rows = db
        .query("SELECT v FROM r WHERE w = ?1", [Value::Integer(8)])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(7));
    db.execute(
        "UPDATE r SET v = ?1 WHERE id = ?2",
        [Value::Integer(99), Value::Integer(1)],
    )
    .unwrap();
    let rows = db.query("SELECT v FROM r WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(99));
}

/// SQLite numbering rule: an anonymous `?` takes "one more than the
/// largest parameter number already assigned" — so `?` before `?1` means
/// BOTH are parameter 1, and a trailing `?` becomes parameter 2.
#[test]
fn anonymous_and_numbered_param_interaction() {
    let mut db = mem();
    // `? ?1 ?`: numbers 1, 1, 2 — the first two bind the SAME parameter,
    // the trailing `?` is parameter 2 (SQLite: "one more than the largest
    // number already assigned"; real SQLite reports "statement uses 2"
    // bindings for this SQL).
    let rows = db
        .query(
            "SELECT ? = ?1, ? <> ?1",
            [Value::Integer(5), Value::Integer(9)],
        )
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(1), "? and ?1 are the same param");
    assert_eq!(rows[0][1], Value::Integer(1), "the second ? is parameter 2");

    // `?5 ?` — the explicit 5 raises the floor: the trailing ? is 6.
    let rows = db
        .query(
            "SELECT ?1, ?2, ?3, ?4, ?5, ?",
            [
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3),
                Value::Integer(4),
                Value::Integer(5),
                Value::Integer(6),
            ],
        )
        .unwrap();
    assert_eq!(rows[0][5], Value::Integer(6), "trailing ? after ?5 is 6");

    // `?0` is rejected (SQLite: parameter number out of range).
    let e = db.execute("SELECT ?0", []).unwrap_err();
    assert!(e.to_string().contains("out of range"), "got: {e}");
}

/// The statement API binds 1-based (sqlite3_bind convention) — `bind(1)`
/// is the first parameter; `?1` must read it.
#[test]
fn statement_numbered_param_binding() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT, b INT)", ()).unwrap();
    let mut stmt = db.prepare("INSERT INTO t VALUES (?1, ?2)").unwrap();
    stmt.bind(1, Value::Integer(11)).unwrap();
    stmt.bind(2, Value::Integer(22)).unwrap();
    stmt.raw_execute().unwrap();
    let rows = db.query("SELECT a, b FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(11));
    assert_eq!(rows[0][1], Value::Integer(22));
}

// ---------------------------------------------------------------------------
// GENERATED COLUMNS (SQLite 3.31+)
// ---------------------------------------------------------------------------

#[test]
fn generated_columns_virtual_and_stored() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE g (a INT, b INT GENERATED ALWAYS AS (a * 2) VIRTUAL)",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO g (a) VALUES (21)", []).unwrap();
    let rows = db.query("SELECT a, b FROM g", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(21));
    assert_eq!(rows[0][1], Value::Integer(42), "VIRTUAL computes on write");

    db.execute("CREATE TABLE g2 (a INT, b INT AS (a + 1) STORED)", ())
        .unwrap();
    db.execute("INSERT INTO g2 (a) VALUES (1)", []).unwrap();
    let rows = db.query("SELECT a, b FROM g2", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
    assert_eq!(rows[0][1], Value::Integer(2), "STORED computes on write");

    // Mixed table: positional INSERT maps values to NON-generated columns
    // (SQLite rule: generated columns are excluded from the positional
    // count — `INSERT INTO m VALUES (10, 20)` binds a=10, d=20).
    db.execute(
        "CREATE TABLE m (a INT, b INT AS (a * 2) VIRTUAL, c INT AS (a + 1) STORED, d INT)",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO m VALUES (10, 20)", []).unwrap();
    let rows = db.query("SELECT a, b, c, d FROM m", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[0][1], Value::Integer(20));
    assert_eq!(rows[0][2], Value::Integer(11));
    assert_eq!(rows[0][3], Value::Integer(20));
    // Supplying the generated slot errors (arity now counts non-generated).
    let e = db
        .execute("INSERT INTO m VALUES (1, 2, 3)", [])
        .unwrap_err();
    assert!(e.to_string().contains("values were supplied"), "got: {e}");
}

#[test]
fn generated_columns_recompute_on_update() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE g (id INTEGER PRIMARY KEY, a INT, b INT AS (a * 2) STORED)",
        (),
    )
    .unwrap();
    for i in 1..=5 {
        db.execute("INSERT INTO g (a) VALUES (?1)", [Value::Integer(i)])
            .unwrap();
    }
    // Range UPDATE: every streaming path must recompute.
    db.execute("UPDATE g SET a = a * 10 WHERE id > 3", [])
        .unwrap();
    let rows = db.query("SELECT a, b FROM g ORDER BY id", []).unwrap();
    for (i, r) in rows.iter().enumerate() {
        let a = r[0].as_integer();
        assert_eq!(r[1].as_integer(), a * 2, "row {} b stale", i + 1);
    }
    // Single-row UPDATE (RowidLookup path).
    db.execute("UPDATE g SET a = 100 WHERE id = 1", []).unwrap();
    let rows = db.query("SELECT b FROM g WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(200));
    // RETURNING sees the computed value.
    let rows = db
        .query("UPDATE g SET a = 7 WHERE id = 2 RETURNING a, b", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(7));
    assert_eq!(rows[0][1], Value::Integer(14));
}

#[test]
fn generated_columns_write_rejections() {
    let mut db = mem();
    db.execute("CREATE TABLE g (a INT PRIMARY KEY, b INT AS (a + 1))", ())
        .unwrap();
    let e = db
        .execute("INSERT INTO g (a, b) VALUES (1, 2)", [])
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("cannot INSERT into generated column"),
        "got: {e}"
    );
    db.execute("INSERT INTO g (a) VALUES (1)", []).unwrap();
    let e = db
        .execute("UPDATE g SET b = 5 WHERE a = 1", [])
        .unwrap_err();
    assert!(
        e.to_string().contains("cannot UPDATE generated column"),
        "got: {e}"
    );
    // A conflicting upsert whose DO UPDATE targets the generated column
    // is rejected when the action fires.
    let e = db
        .execute(
            "INSERT INTO g (a) VALUES (1) ON CONFLICT(a) DO UPDATE SET b = excluded.b",
            [],
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("cannot UPDATE generated column"),
        "got: {e}"
    );
}

#[test]
fn generated_columns_filters_indexes_returning() {
    let mut db = mem();
    // Expression index over a generated column + WHERE over it.
    db.execute(
        "CREATE TABLE s (id INTEGER PRIMARY KEY, a INT, b INT AS (a * 2))",
        (),
    )
    .unwrap();
    for i in 1..=10 {
        db.execute("INSERT INTO s (a) VALUES (?1)", [Value::Integer(i)])
            .unwrap();
    }
    let rows = db
        .query("SELECT id FROM s WHERE b > 15 ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 3, "ids 8,9,10 have b > 15");
    // Index over the generated column.
    db.execute("CREATE INDEX ib ON s (b)", []).unwrap();
    let rows = db.query("SELECT id FROM s WHERE b = 8", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(4));
    // INSERT ... RETURNING includes the computed value.
    let rows = db
        .query("INSERT INTO s (a) VALUES (100) RETURNING a, b", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(100));
    assert_eq!(rows[0][1], Value::Integer(200));
    // Upsert recomputes the generated column on the merged row.
    db.execute(
        "INSERT INTO s (id, a) VALUES (1, 50) ON CONFLICT(id) DO UPDATE SET a = excluded.a",
        [],
    )
    .unwrap();
    let rows = db.query("SELECT b FROM s WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(100));
}

#[test]
fn generated_columns_schema_roundtrip() {
    let path = std::env::temp_dir().join("rq_gen_col_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = rustqlite::Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE g (a INT, b INT GENERATED ALWAYS AS (a * 2) VIRTUAL, t TEXT)",
            (),
        )
        .unwrap();
        db.execute("INSERT INTO g (a, t) VALUES (5, 'x')", [])
            .unwrap();
    }
    let mut db = rustqlite::Database::open(&path).unwrap();
    let rows = db.query("SELECT a, b, t FROM g", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(5));
    assert_eq!(rows[0][1], Value::Integer(10));
    assert_eq!(rows[0][2], Value::Text("x".into()));
    // Writes still recompute after reopen.
    db.execute("UPDATE g SET a = 6", []).unwrap();
    let rows = db.query("SELECT b FROM g", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(12));
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// INSERT arity (SQLite errors, was silently truncated)
// ---------------------------------------------------------------------------

#[test]
fn insert_arity_mismatch_errors() {
    let mut db = mem();
    db.execute("CREATE TABLE t (a INT, b INT)", ()).unwrap();
    // Column list + extra values: SQLite errors (values silently dropped
    // before — the fast path's `if i < target_indices.len()` guard). The
    // planner's own arity check fires first ("N VALUES for M columns").
    let e = db
        .execute("INSERT INTO t (a) VALUES (1, 2)", [])
        .unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("values were supplied") || msg.contains("VALUES for"),
        "got: {msg}"
    );
    // No column list + short VALUES.
    let e = db.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("values were supplied") || msg.contains("VALUES for"),
        "got: {msg}"
    );
    // INSERT ... SELECT with wrong width (2 values for a 1-column
    // target list) — errors even when the source is EMPTY (SQLite
    // checks at prepare time).
    db.execute("CREATE TABLE src (x INT)", ()).unwrap();
    let e = db
        .execute("INSERT INTO t (a) SELECT x, x FROM src", [])
        .unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("values were supplied") || msg.contains("VALUES for"),
        "got: {msg}"
    );
    // Matching width with a full-column target is fine.
    db.execute("INSERT INTO src VALUES (1)", []).unwrap();
    db.execute("INSERT INTO t SELECT x, x FROM src", [])
        .unwrap();
}

// ---------------------------------------------------------------------------
// VACUUM → append corruption (right=0 interiors)
// ---------------------------------------------------------------------------

/// Regression: interior pages can keep `right_most = 0` after mass
/// deletes (the delete/rebalance path zeroes the slot when the right-most
/// subtree empties; the page-level VACUUM copy preserves that state). The
/// insert descent, the append walk, and `right_most_leaf` all used to
/// treat `right = 0` as "descend into page 0" — PAGE 0 IS THE SCHEMA
/// PAGE, so appends silently inserted rows into the schema tree (rows
/// became unreachable, the COUNT drifted), and when the schema page
/// eventually split, the parent splice wrote a phantom `(0, key)` cell
/// into the table's interior: `PRAGMA integrity_check` reported
/// "rowid 1 out of order (prev N)". Shape: one big insert transaction,
/// delete 90%, VACUUM, INSERT..SELECT, delete — integrity at every step.
#[test]
fn vacuum_then_append_right_zero_interior() {
    let path = std::env::temp_dir().join("rq_vac_append_regression.db");
    let wal = {
        let mut w = path.clone();
        w.set_extension("db-wal");
        w
    };
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal);
    {
        let mut db = rustqlite::Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("PRAGMA synchronous = OFF", []).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=500_000i64 {
            db.execute(
                "INSERT INTO t (name, val, score) VALUES (?1, ?2, ?3)",
                [
                    Value::Text(format!("name{i}").into()),
                    Value::Integer(i),
                    Value::Real(i as f64 * 1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("DELETE FROM t WHERE id > 50000", []).unwrap();
        let ic = db.query("PRAGMA integrity_check", []).unwrap();
        assert_eq!(ic[0][0], Value::Text("ok".into()));
        db.execute("VACUUM", []).unwrap();
        let ic = db.query("PRAGMA integrity_check", []).unwrap();
        assert_eq!(ic[0][0], Value::Text("ok".into()), "post-vacuum");
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(n[0][0], Value::Integer(50000));
        // The regression trigger: append a full table's worth of rows.
        db.execute(
            "INSERT INTO t (name, val, score) SELECT name, val, score FROM t",
            [],
        )
        .unwrap();
        let ic = db.query("PRAGMA integrity_check", []).unwrap();
        assert_eq!(ic[0][0], Value::Text("ok".into()), "post-append");
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(n[0][0], Value::Integer(100000), "appended rows reachable");
        let mx = db.query("SELECT MAX(id) FROM t", []).unwrap();
        assert_eq!(mx[0][0], Value::Integer(100000));
        // And the delete side of the churn.
        db.execute("DELETE FROM t WHERE id > 50000", []).unwrap();
        let ic = db.query("PRAGMA integrity_check", []).unwrap();
        assert_eq!(ic[0][0], Value::Text("ok".into()), "post-delete");
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(n[0][0], Value::Integer(50000));
        // The schema page must be untouched: exactly one row in
        // sqlite_schema for this table.
        let sc = db
            .query(
                "SELECT COUNT(*) FROM sqlite_schema WHERE tbl_name = 't'",
                [],
            )
            .unwrap();
        assert_eq!(sc[0][0], Value::Integer(1), "schema page not polluted");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&wal);
}
