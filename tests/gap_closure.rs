//! Gap-closure battery: differential tests against bundled SQLite for
//! the correctness gaps closed in this pass — multi-row VALUES, FILTER
//! clauses, like()/likelihood() scalars, window EXCLUDE TIES, ON UPDATE
//! actions (+ the FK action parser order bug), and WITH-prefixed DML.
//!
//! Every test runs the same SQL on the engine and on rusqlite/bundled
//! SQLite and asserts identical results. These pin the fixes; they are
//! not benchmarks and make no performance claims.

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().unwrap()
}

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

fn engine_rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

fn lite_rows(con: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut st = con.prepare(sql).unwrap();
    let n = st.column_count();
    st.query_map([], |r| {
        let mut parts = Vec::new();
        for i in 0..n {
            let v: rusqlite::types::Value = r.get(i)?;
            parts.push(format!("{:?}", v));
        }
        Ok(parts.join("|"))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn engine_strs(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "Null".to_string(),
                    Value::Integer(i) => format!("Integer({})", i),
                    Value::Real(f) => format!("Real({:?})", f),
                    Value::Text(t) => format!("Text({:?})", t.to_string()),
                    Value::Blob(b) => format!("Blob({:?})", b),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn assert_same(sql: &str) {
    let db = mem();
    let con = lite();
    let ours = engine_strs(&engine_rows(&db, sql));
    let theirs = lite_rows(&con, sql);
    assert_eq!(ours, theirs, "mismatch on {}", sql);
}

// ---------------------------------------------------------------------------
// Multi-row VALUES
// ---------------------------------------------------------------------------

#[test]
fn values_multi_row() {
    let db = mem();
    let rows = db.query("VALUES (1,2),(3,4)", []).unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(3), Value::Integer(4)],
        ]
    );
}

#[test]
fn values_multi_row_differential() {
    for sql in [
        "VALUES (1,2),(3,4)",
        "VALUES (1),(2),(3)",
        "VALUES (1),(2) UNION ALL VALUES (3)",
        "VALUES ('a',1),('b',2)",
    ] {
        assert_same(sql);
    }
    // ORDER BY over a VALUES chain: SQLite requires a compound SELECT
    // wrapper; the engine accepts the bare form (superset, same rows).
    let db = mem();
    let rows = db
        .query("VALUES (2),(1),(3) ORDER BY 1 LIMIT 2", [])
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
}

#[test]
fn values_mismatched_width_errors() {
    let db = mem();
    assert!(db.query("VALUES (1),(2,3)", []).is_err());
}

// ---------------------------------------------------------------------------
// FILTER clause
// ---------------------------------------------------------------------------

#[test]
fn filter_no_group_by_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE g(x)", []).unwrap();
    db.execute("INSERT INTO g VALUES (1),(2),(NULL),(3)", [])
        .unwrap();
    con.execute_batch("CREATE TABLE g(x); INSERT INTO g VALUES (1),(2),(NULL),(3);")
        .unwrap();
    for sql in [
        "SELECT COUNT(*) FILTER (WHERE x > 1) FROM g",
        "SELECT SUM(x) FILTER (WHERE x > 1) FROM g",
        "SELECT COUNT(*) FILTER (WHERE x IS NULL) FROM g",
        "SELECT COUNT(*) FILTER (WHERE x > 1), SUM(x) FILTER (WHERE x > 1) FROM g",
        "SELECT AVG(x) FILTER (WHERE x > 1) FROM g",
        "SELECT MIN(x) FILTER (WHERE x > 1), MAX(x) FILTER (WHERE x > 1) FROM g",
        "SELECT COUNT(DISTINCT x) FILTER (WHERE x > 1) FROM g",
        "SELECT GROUP_CONCAT(x) FILTER (WHERE x > 1) FROM g",
    ] {
        let ours = engine_strs(&engine_rows(&db, sql));
        let theirs = lite_rows(&con, sql);
        assert_eq!(ours, theirs, "mismatch on {}", sql);
    }
}

#[test]
fn filter_group_by_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE c(k, v)", []).unwrap();
    db.execute(
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',NULL)",
        [],
    )
    .unwrap();
    con.execute_batch(
        "CREATE TABLE c(k, v); INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',NULL);",
    )
    .unwrap();
    for sql in [
        "SELECT k, SUM(v) FILTER (WHERE v > 2) FROM c GROUP BY k ORDER BY k",
        "SELECT k, COUNT(*) FILTER (WHERE v > 2) FROM c GROUP BY k ORDER BY k",
        "SELECT SUM(v) FILTER (WHERE v > 2) FROM (SELECT * FROM c)",
    ] {
        let ours = engine_strs(&engine_rows(&db, sql));
        let theirs = lite_rows(&con, sql);
        assert_eq!(ours, theirs, "mismatch on {}", sql);
    }
}

#[test]
fn filter_window_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE c(k, v)", []).unwrap();
    db.execute(
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',NULL)",
        [],
    )
    .unwrap();
    con.execute_batch(
        "CREATE TABLE c(k, v); INSERT INTO c VALUES ('a',1),('a',5),('b',10),('b',NULL);",
    )
    .unwrap();
    let sql = "SELECT SUM(v) FILTER (WHERE v > 2) OVER () FROM c ORDER BY v";
    let ours = engine_strs(&engine_rows(&db, sql));
    let theirs = lite_rows(&con, sql);
    assert_eq!(ours, theirs, "mismatch on {}", sql);
}

// ---------------------------------------------------------------------------
// like() / likelihood()
// ---------------------------------------------------------------------------

#[test]
fn like_function_differential() {
    for sql in [
        "SELECT like('a%', 'abc')",
        "SELECT like('a%', NULL)",
        "SELECT like('a%', 'abc', NULL)",
        "SELECT likelihood(1, 0.5)",
        "SELECT likelihood(NULL, 0.5)",
        "SELECT likely(1)",
        "SELECT unlikely(1)",
    ] {
        assert_same(sql);
    }
}

// ---------------------------------------------------------------------------
// Window EXCLUDE TIES
// ---------------------------------------------------------------------------

#[test]
fn exclude_ties_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE h(x)", []).unwrap();
    db.execute("INSERT INTO h VALUES (1),(1),(2),(3)", [])
        .unwrap();
    con.execute_batch("CREATE TABLE h(x); INSERT INTO h VALUES (1),(1),(2),(3);")
        .unwrap();
    for sql in [
        "SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM h",
        "SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW) FROM h",
        "SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP) FROM h",
        "SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM h",
        "SELECT SUM(x) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE NO OTHERS) FROM h",
    ] {
        let ours = engine_strs(&engine_rows(&db, sql));
        let theirs = lite_rows(&con, sql);
        assert_eq!(ours, theirs, "mismatch on {}", sql);
    }
}

// ---------------------------------------------------------------------------
// ON UPDATE actions (+ parser order bug)
// ---------------------------------------------------------------------------

fn fk_pair() -> (Database, rusqlite::Connection) {
    let mut db = mem();
    db.execute("PRAGMA foreign_keys=ON", []).unwrap();
    let con = lite();
    con.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    (db, con)
}

#[test]
fn on_update_cascade_rowid_key() {
    let (mut db, con) = fk_pair();
    for (e, c) in [
        ("CREATE TABLE p(id INTEGER PRIMARY KEY)", true),
        (
            "CREATE TABLE c(id INTEGER REFERENCES p(id) ON UPDATE CASCADE)",
            true,
        ),
        ("INSERT INTO p VALUES (1)", true),
        ("INSERT INTO c VALUES (1)", true),
    ] {
        let _ = c;
        db.execute(e, []).unwrap();
        con.execute(e, []).unwrap();
    }
    db.execute("UPDATE p SET id=2", []).unwrap();
    con.execute("UPDATE p SET id=2", []).unwrap();
    let ours = engine_strs(&engine_rows(&db, "SELECT id FROM c"));
    let theirs = lite_rows(&con, "SELECT id FROM c");
    assert_eq!(ours, theirs);
    assert_eq!(ours, vec!["Integer(2)".to_string()]);
}

#[test]
fn on_update_actions_differential() {
    // SET NULL / RESTRICT / composite CASCADE, compared against SQLite.
    let (mut db, con) = fk_pair();
    let setup = [
        "CREATE TABLE p2(id INTEGER PRIMARY KEY)",
        "CREATE TABLE c2(id INTEGER REFERENCES p2(id) ON UPDATE SET NULL)",
        "INSERT INTO p2 VALUES (1)",
        "INSERT INTO c2 VALUES (1)",
        "CREATE TABLE p4(a, b, UNIQUE(a,b))",
        "CREATE TABLE c4(x, y, FOREIGN KEY(x,y) REFERENCES p4(a,b) ON UPDATE CASCADE)",
        "INSERT INTO p4 VALUES (1,2)",
        "INSERT INTO c4 VALUES (1,2)",
    ];
    for s in setup {
        db.execute(s, []).unwrap();
        con.execute(s, []).unwrap();
    }
    db.execute("UPDATE p2 SET id=2", []).unwrap();
    con.execute("UPDATE p2 SET id=2", []).unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT id FROM c2")),
        lite_rows(&con, "SELECT id FROM c2")
    );
    db.execute("UPDATE p4 SET a=9 WHERE a=1", []).unwrap();
    con.execute("UPDATE p4 SET a=9 WHERE a=1", []).unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT x, y FROM c4")),
        lite_rows(&con, "SELECT x, y FROM c4")
    );
    // RESTRICT rejects on both engines.
    db.execute("CREATE TABLE p3(id INTEGER PRIMARY KEY)", [])
        .unwrap();
    db.execute("CREATE TABLE c3(id INTEGER REFERENCES p3(id))", [])
        .unwrap();
    db.execute("INSERT INTO p3 VALUES (1)", []).unwrap();
    db.execute("INSERT INTO c3 VALUES (1)", []).unwrap();
    con.execute("CREATE TABLE p3(id INTEGER PRIMARY KEY)", [])
        .unwrap();
    con.execute("CREATE TABLE c3(id INTEGER REFERENCES p3(id))", [])
        .unwrap();
    con.execute("INSERT INTO p3 VALUES (1)", []).unwrap();
    con.execute("INSERT INTO c3 VALUES (1)", []).unwrap();
    assert!(db.execute("UPDATE p3 SET id=2", []).is_err());
    assert!(con.execute("UPDATE p3 SET id=2", []).is_err());
}

#[test]
fn fk_action_clause_order() {
    // Both clause orders must parse to the same actions (the parser used
    // to assign the first ON clause to both slots).
    let mut db = mem();
    db.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)", [])
        .unwrap();
    db.execute(
        "CREATE TABLE c(id INTEGER REFERENCES p(id) ON DELETE CASCADE ON UPDATE SET NULL)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE c2(id INTEGER REFERENCES p(id) ON UPDATE SET NULL ON DELETE CASCADE)",
        [],
    )
    .unwrap();
    for t in ["c", "c2"] {
        let rows = db
            .query(
                &format!(
                    "SELECT on_delete, on_update FROM pragma_foreign_key_list('{}')",
                    t
                ),
                [],
            )
            .unwrap();
        assert_eq!(rows[0][0], Value::Text("CASCADE".into()), "table {}", t);
        assert_eq!(rows[0][1], Value::Text("SET NULL".into()), "table {}", t);
    }
}

// ---------------------------------------------------------------------------
// WITH-prefixed DML
// ---------------------------------------------------------------------------

#[test]
fn with_dml_matches_sqlite() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE g(x)", []).unwrap();
    db.execute("INSERT INTO g VALUES (1)", []).unwrap();
    con.execute_batch("CREATE TABLE g(x); INSERT INTO g VALUES (1);")
        .unwrap();
    db.execute(
        "WITH a AS (SELECT 1 AS v) INSERT INTO g SELECT * FROM a",
        [],
    )
    .unwrap();
    con.execute(
        "WITH a AS (SELECT 1 AS v) INSERT INTO g SELECT * FROM a",
        [],
    )
    .unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT x FROM g ORDER BY x")),
        lite_rows(&con, "SELECT x FROM g ORDER BY x")
    );
    db.execute(
        "WITH a AS (SELECT 2 AS v) UPDATE g SET x = (SELECT v FROM a)",
        [],
    )
    .unwrap();
    con.execute(
        "WITH a AS (SELECT 2 AS v) UPDATE g SET x = (SELECT v FROM a)",
        [],
    )
    .unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT x FROM g ORDER BY x")),
        lite_rows(&con, "SELECT x FROM g ORDER BY x")
    );
    db.execute(
        "WITH a AS (SELECT 1 AS v) DELETE FROM g WHERE x IN (SELECT v FROM a)",
        [],
    )
    .unwrap();
    con.execute(
        "WITH a AS (SELECT 1 AS v) DELETE FROM g WHERE x IN (SELECT v FROM a)",
        [],
    )
    .unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT x FROM g ORDER BY x")),
        lite_rows(&con, "SELECT x FROM g ORDER BY x")
    );
}

// ---------------------------------------------------------------------------
// HAVING over projection aliases
// ---------------------------------------------------------------------------

#[test]
fn having_alias_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE c(k, v)", []).unwrap();
    db.execute("INSERT INTO c VALUES ('a',1),('a',5),('b',10)", [])
        .unwrap();
    con.execute_batch("CREATE TABLE c(k, v); INSERT INTO c VALUES ('a',1),('a',5),('b',10);")
        .unwrap();
    for sql in [
        "SELECT k, SUM(v) AS s FROM c GROUP BY k HAVING s > 4 ORDER BY k",
        "SELECT k, SUM(v) FROM c GROUP BY k HAVING SUM(v) > 4 ORDER BY k",
        "SELECT v % 2 AS p, COUNT(*) AS n FROM c GROUP BY p HAVING n > 1 ORDER BY p",
    ] {
        let ours = engine_strs(&engine_rows(&db, sql));
        let theirs = lite_rows(&con, sql);
        assert_eq!(ours, theirs, "mismatch on {}", sql);
    }
}

// ---------------------------------------------------------------------------
// Partial UNIQUE indexes + UPSERT target WHERE
// ---------------------------------------------------------------------------

#[test]
fn partial_unique_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE u(a, b)", []).unwrap();
    db.execute("CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0", [])
        .unwrap();
    con.execute_batch("CREATE TABLE u(a, b); CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0;")
        .unwrap();
    db.execute("INSERT INTO u VALUES (1, -5)", []).unwrap();
    db.execute("INSERT INTO u VALUES (1, -6)", []).unwrap();
    con.execute("INSERT INTO u VALUES (1, -5)", []).unwrap();
    con.execute("INSERT INTO u VALUES (1, -6)", []).unwrap();
    db.execute("INSERT INTO u VALUES (1, 7)", []).unwrap();
    con.execute("INSERT INTO u VALUES (1, 7)", []).unwrap();
    assert!(db.execute("INSERT INTO u VALUES (1, 8)", []).is_err());
    assert!(con.execute("INSERT INTO u VALUES (1, 8)", []).is_err());
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT a, b FROM u ORDER BY b")),
        lite_rows(&con, "SELECT a, b FROM u ORDER BY b")
    );
}

#[test]
fn upsert_partial_where_differential() {
    let mut db = mem();
    let con = lite();
    db.execute("CREATE TABLE u(a, b)", []).unwrap();
    db.execute("CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0", [])
        .unwrap();
    db.execute("INSERT INTO u VALUES (1, 1)", []).unwrap();
    con.execute_batch("CREATE TABLE u(a, b); CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0; INSERT INTO u VALUES (1, 1);")
        .unwrap();
    db.execute(
        "INSERT INTO u VALUES (1, -5) ON CONFLICT(a) WHERE b>0 DO UPDATE SET b=2",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO u VALUES (1, -5) ON CONFLICT(a) WHERE b>0 DO UPDATE SET b=2",
        [],
    )
    .unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT a, b FROM u ORDER BY b")),
        lite_rows(&con, "SELECT a, b FROM u ORDER BY b")
    );
    db.execute(
        "INSERT INTO u VALUES (1, 5) ON CONFLICT(a) WHERE b>0 DO UPDATE SET b=excluded.b",
        [],
    )
    .unwrap();
    con.execute(
        "INSERT INTO u VALUES (1, 5) ON CONFLICT(a) WHERE b>0 DO UPDATE SET b=excluded.b",
        [],
    )
    .unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT a, b FROM u")),
        lite_rows(&con, "SELECT a, b FROM u")
    );
}

// ---------------------------------------------------------------------------
// Triggers: UPDATE OF column filtering, WHEN guards, BEFORE phases
// ---------------------------------------------------------------------------

#[test]
fn trigger_update_of_and_when_differential() {
    let mut db = mem();
    let con = lite();
    for s in [
        "CREATE TABLE t(a, b)",
        "INSERT INTO t VALUES (1, 1)",
        "CREATE TABLE log(m)",
        "CREATE TRIGGER tu AFTER UPDATE OF a ON t BEGIN INSERT INTO log VALUES ('upd'); END",
    ] {
        db.execute(s, []).unwrap();
        con.execute(s, []).unwrap();
    }
    // UPDATE of an unlisted column: no fire on either engine.
    db.execute("UPDATE t SET b = 2", []).unwrap();
    con.execute("UPDATE t SET b = 2", []).unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT * FROM log")),
        lite_rows(&con, "SELECT * FROM log")
    );
    // UPDATE of the listed column: fires on both.
    db.execute("UPDATE t SET a = 2", []).unwrap();
    con.execute("UPDATE t SET a = 2", []).unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT * FROM log")),
        lite_rows(&con, "SELECT * FROM log")
    );
    // WHEN guard: only NEW.a > 100 fires.
    let s = "CREATE TRIGGER tw AFTER INSERT ON t WHEN NEW.a > 100 BEGIN INSERT INTO log VALUES ('big'); END";
    db.execute(s, []).unwrap();
    con.execute(s, []).unwrap();
    db.execute("INSERT INTO t(a) VALUES (5)", []).unwrap();
    con.execute("INSERT INTO t(a) VALUES (5)", []).unwrap();
    db.execute("INSERT INTO t(a) VALUES (200)", []).unwrap();
    con.execute("INSERT INTO t(a) VALUES (200)", []).unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT * FROM log")),
        lite_rows(&con, "SELECT * FROM log")
    );
}

#[test]
fn trigger_before_phases_differential() {
    let mut db = mem();
    let con = lite();
    for s in [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a)",
        "INSERT INTO t VALUES (1,1),(2,2),(3,3)",
        "CREATE TABLE log(m)",
        "CREATE TRIGGER bu BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES ('bu'); END",
        "CREATE TRIGGER bd BEFORE DELETE ON t BEGIN INSERT INTO log VALUES ('bd'); END",
    ] {
        db.execute(s, []).unwrap();
        con.execute(s, []).unwrap();
    }
    db.execute("UPDATE t SET a = 9 WHERE id = 1", []).unwrap();
    con.execute("UPDATE t SET a = 9 WHERE id = 1", []).unwrap();
    db.execute("DELETE FROM t WHERE id > 1", []).unwrap();
    con.execute("DELETE FROM t WHERE id > 1", []).unwrap();
    db.execute("DELETE FROM t WHERE id = 1", []).unwrap();
    con.execute("DELETE FROM t WHERE id = 1", []).unwrap();
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT * FROM log")),
        lite_rows(&con, "SELECT * FROM log")
    );
    assert_eq!(
        engine_strs(&engine_rows(&db, "SELECT * FROM t ORDER BY id")),
        lite_rows(&con, "SELECT * FROM t ORDER BY id")
    );
}
