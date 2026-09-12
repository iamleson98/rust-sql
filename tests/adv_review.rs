//! ADVERSARIAL review battery for PR #2 — probes edge cases the PR's own
//! tests do NOT cover. Each case compares against bundled SQLite.

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().unwrap()
}

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
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

fn both_engine_strs(
    db: &Database,
    con: &rusqlite::Connection,
    sql: &str,
) -> (Vec<String>, Vec<String>) {
    let rows = db
        .query(sql, [])
        .map(|r| engine_strs(&r))
        .map_err(|e| format!("{:?}", e));
    let ours = match rows {
        Ok(r) => r,
        Err(e) => vec![format!("__ERR__{}", e)],
    };
    let theirs = match lite_rows_safe(con, sql) {
        Ok(r) => r,
        Err(e) => vec![format!("__ERR__{}", e)],
    };
    (ours, theirs)
}

fn lite_rows_safe(con: &rusqlite::Connection, sql: &str) -> Result<Vec<String>, rusqlite::Error> {
    let mut st = con.prepare(sql)?;
    let n = st.column_count();
    let rows: Vec<String> = st
        .query_map([], |r| {
            let mut parts = Vec::new();
            for i in 0..n {
                let v: rusqlite::types::Value = r.get(i)?;
                parts.push(format!("{:?}", v));
            }
            Ok(parts.join("|"))
        })?
        .map(|r| r.unwrap())
        .collect();
    Ok(rows)
}

fn diff(con: &rusqlite::Connection, db: &Database, sql: &str) {
    let (ours, theirs) = both_engine_strs(db, con, sql);
    assert_eq!(
        ours, theirs,
        "MISMATCH on: {}\n  ours:   {:?}\n  theirs: {:?}",
        sql, ours, theirs
    );
}

fn setup_both(script: &[&str]) -> (Database, rusqlite::Connection) {
    let mut db = mem();
    let con = lite();
    for s in script {
        let r1 = db.execute(s, []);
        let r2 = con.execute_batch(s);
        if let Err(e) = r2 {
            panic!("SQLite rejected setup {:?}: {:?}", s, e);
        }
        r1.unwrap_or_else(|e| panic!("engine rejected setup {:?}: {:?}", s, e));
    }
    (db, con)
}

// ---------------------------------------------------------------------------
// 1. Nested multi-row VALUES in IN / FROM positions
// ---------------------------------------------------------------------------

#[test]
fn adv_nested_values_in() {
    let db = mem();
    let con = lite();
    for sql in [
        "SELECT 1 IN (VALUES (1),(2))",
        "SELECT 2 IN (VALUES (1),(2))",
        "SELECT 3 IN (VALUES (1),(2))",
        "SELECT 3 NOT IN (VALUES (1),(2))",
        "SELECT 2 IN (VALUES (1),(NULL))",
        "SELECT 3 IN (VALUES (1),(NULL))",
    ] {
        let (ours, theirs) = both_engine_strs(&db, &con, sql);
        assert_eq!(
            ours, theirs,
            "nested VALUES IN mismatch on {}: ours {:?} theirs {:?}",
            sql, ours, theirs
        );
    }
}

#[test]
fn adv_values_in_from() {
    let db = mem();
    let con = lite();
    for sql in [
        "SELECT * FROM (VALUES (1,'a'),(2,'b'))",
        "SELECT column1, column2 FROM (VALUES (1,'a'),(2,'b')) ORDER BY 1",
        "SELECT count(*) FROM (VALUES (1),(2),(3))",
    ] {
        let (ours, theirs) = both_engine_strs(&db, &con, sql);
        assert_eq!(
            ours, theirs,
            "VALUES-in-FROM mismatch on {}: ours {:?} theirs {:?}",
            sql, ours, theirs
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Deep FK ON UPDATE CASCADE chains (3 levels)
// ---------------------------------------------------------------------------

#[test]
fn adv_fk_cascade_three_levels() {
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE gp(id INTEGER PRIMARY KEY)",
        "CREATE TABLE p(id INTEGER PRIMARY KEY REFERENCES gp(id) ON UPDATE CASCADE)",
        "CREATE TABLE c(id INTEGER PRIMARY KEY REFERENCES p(id) ON UPDATE CASCADE)",
        "CREATE TABLE gc(id INTEGER PRIMARY KEY REFERENCES c(id) ON UPDATE CASCADE)",
        "INSERT INTO gp VALUES (1)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1)",
        "INSERT INTO gc VALUES (1)",
    ]);
    db.execute("UPDATE gp SET id=100", []).unwrap();
    con.execute("UPDATE gp SET id=100", []).unwrap();
    diff(
        &con,
        &db,
        "SELECT (SELECT count(*) FROM gc), (SELECT id FROM gc)",
    );
}

#[test]
fn adv_fk_cascade_mixed_actions() {
    // gp -> (cascade) p -> (set null) c: update gp cascades p, which
    // nulls c.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE gp(id INTEGER PRIMARY KEY)",
        "CREATE TABLE p(id INTEGER PRIMARY KEY REFERENCES gp(id) ON UPDATE CASCADE)",
        "CREATE TABLE c(id INTEGER REFERENCES p(id) ON UPDATE SET NULL)",
        "INSERT INTO gp VALUES (1)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1)",
    ]);
    db.execute("UPDATE gp SET id=100", []).unwrap();
    con.execute("UPDATE gp SET id=100", []).unwrap();
    diff(&con, &db, "SELECT * FROM p");
    diff(&con, &db, "SELECT * FROM c");
}

#[test]
fn adv_fk_set_default() {
    // SQLite (pinned): SET DEFAULT whose default key has no parent row
    // errors "FOREIGN KEY constraint failed" and the WHOLE statement
    // rolls back (p keeps its old key). A valid default succeeds.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY)",
        "CREATE TABLE c(id INTEGER DEFAULT 0 REFERENCES p(id) ON UPDATE SET DEFAULT)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1)",
    ]);
    let r1 = db.execute("UPDATE p SET id=2", []);
    let r2 = con.execute_batch("UPDATE p SET id=2");
    assert!(r1.is_err(), "engine must reject missing-parent SET DEFAULT");
    assert!(r2.is_err(), "SQLite rejects missing-parent SET DEFAULT");
    // Statement atomicity: both engines keep p at 1.
    diff(&con, &db, "SELECT * FROM p");
    diff(&con, &db, "SELECT * FROM c");
    // Valid default: parent has the default key (0).
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY)",
        "CREATE TABLE c(id INTEGER DEFAULT 0 REFERENCES p(id) ON UPDATE SET DEFAULT)",
        "INSERT INTO p VALUES (0)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1)",
    ]);
    db.execute("UPDATE p SET id=2 WHERE id=1", []).unwrap();
    con.execute_batch("UPDATE p SET id=2 WHERE id=1").unwrap();
    diff(&con, &db, "SELECT * FROM p ORDER BY id");
    diff(&con, &db, "SELECT * FROM c");
    // SET DEFAULT onto a rowid-alias child column: the row MOVES to the
    // default key (pinned).
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY)",
        "CREATE TABLE c(id INTEGER PRIMARY KEY DEFAULT 7 REFERENCES p(id) ON UPDATE SET DEFAULT, v)",
        "INSERT INTO p VALUES (7)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1, 'x')",
    ]);
    db.execute("UPDATE p SET id=9 WHERE id=1", []).unwrap();
    con.execute_batch("UPDATE p SET id=9 WHERE id=1").unwrap();
    diff(&con, &db, "SELECT * FROM p ORDER BY id");
    diff(&con, &db, "SELECT * FROM c");
}

#[test]
fn adv_fk_update_noop_on_unreferenced_col() {
    // Updating a non-key column must not fire ON UPDATE actions.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY, extra)",
        "CREATE TABLE c(id INTEGER REFERENCES p(id) ON UPDATE CASCADE)",
        "INSERT INTO p VALUES (1, 'x')",
        "INSERT INTO c VALUES (1)",
    ]);
    db.execute("UPDATE p SET extra='y'", []).unwrap();
    con.execute("UPDATE p SET extra='y'", []).unwrap();
    diff(&con, &db, "SELECT * FROM c");
}

#[test]
fn adv_fk_update_to_same_value() {
    // SQLite: updating a parent key to the SAME value is a no-op for FK
    // actions.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY)",
        "CREATE TABLE c(id INTEGER REFERENCES p(id) ON UPDATE CASCADE)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1)",
    ]);
    db.execute("UPDATE p SET id=1", []).unwrap();
    con.execute("UPDATE p SET id=1", []).unwrap();
    diff(&con, &db, "SELECT * FROM c");
}

#[test]
fn adv_fk_set_null_composite_partial() {
    // Composite FK where only ONE column of the child key is updated.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(a, b, UNIQUE(a,b))",
        "CREATE TABLE c(x, y, FOREIGN KEY(x,y) REFERENCES p(a,b) ON UPDATE SET NULL)",
        "INSERT INTO p VALUES (1,2)",
        "INSERT INTO c VALUES (1,2)",
    ]);
    db.execute("UPDATE p SET a=9 WHERE a=1", []).unwrap();
    con.execute("UPDATE p SET a=9 WHERE a=1", []).unwrap();
    diff(&con, &db, "SELECT * FROM c");
}

// ---------------------------------------------------------------------------
// 3. WITH RECURSIVE + DML
// ---------------------------------------------------------------------------

#[test]
fn adv_with_recursive_insert() {
    let (mut db, con) = setup_both(&["CREATE TABLE t(x)"]);
    let sql = "WITH RECURSIVE cnt(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM cnt WHERE x<5) INSERT INTO t SELECT x FROM cnt";
    db.execute(sql, []).unwrap();
    con.execute_batch(sql).unwrap();
    diff(&con, &db, "SELECT * FROM t ORDER BY x");
}

#[test]
fn adv_with_recursive_update_delete() {
    let (mut db, con) = setup_both(&[
        "CREATE TABLE t(x)",
        "INSERT INTO t VALUES (1),(2),(3),(10),(11)",
    ]);
    let sql = "WITH RECURSIVE cnt(x) AS (VALUES(10) UNION ALL SELECT x+1 FROM cnt WHERE x<12) DELETE FROM t WHERE x IN (SELECT x FROM cnt)";
    db.execute(sql, []).unwrap();
    con.execute_batch(sql).unwrap();
    diff(&con, &db, "SELECT * FROM t ORDER BY x");

    let sql2 =
        "WITH m(mx) AS (SELECT max(x) FROM t) UPDATE t SET x = x*2 WHERE x <= (SELECT mx FROM m)";
    db.execute(sql2, []).unwrap();
    con.execute_batch(sql2).unwrap();
    diff(&con, &db, "SELECT * FROM t ORDER BY x");
}

// ---------------------------------------------------------------------------
// 4. VIEW DML edge cases
// ---------------------------------------------------------------------------

#[test]
fn adv_view_insert_update_delete_instead_of() {
    let (mut db, con) = setup_both(&[
        "CREATE TABLE t(a, b)",
        "CREATE VIEW v AS SELECT a, b FROM t",
        "CREATE TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN INSERT INTO t VALUES (NEW.a, NEW.b); END",
        "CREATE TRIGGER v_upd INSTEAD OF UPDATE ON v BEGIN UPDATE t SET a=NEW.a, b=NEW.b WHERE a=OLD.a; END",
        "CREATE TRIGGER v_del INSTEAD OF DELETE ON v BEGIN DELETE FROM t WHERE a=OLD.a; END",
    ]);
    for sql in [
        "INSERT INTO v VALUES (1, 'one')",
        "INSERT INTO v VALUES (2, 'two')",
    ] {
        db.execute(sql, []).unwrap();
        con.execute_batch(sql).unwrap();
    }
    diff(&con, &db, "SELECT * FROM v ORDER BY a");

    db.execute("UPDATE v SET b='uno' WHERE a=1", []).unwrap();
    con.execute_batch("UPDATE v SET b='uno' WHERE a=1").unwrap();
    diff(&con, &db, "SELECT * FROM v ORDER BY a");

    db.execute("DELETE FROM v WHERE a=2", []).unwrap();
    con.execute_batch("DELETE FROM v WHERE a=2").unwrap();
    diff(&con, &db, "SELECT * FROM v ORDER BY a");
}

#[test]
fn adv_view_update_of_column() {
    // INSTEAD OF UPDATE OF a — fires only when `a` is assigned (pinned
    // against SQLite). SET b only: NO trigger matches, and a view DML
    // statement without a matching INSTEAD OF trigger ERRORS
    // ("cannot modify v because it is a view") — it is NOT a silent
    // no-op. SET a (or a + b): the trigger fires.
    let (mut db, con) = setup_both(&[
        "CREATE TABLE t(a, b)",
        "INSERT INTO t VALUES (1, 'x')",
        "CREATE VIEW v AS SELECT a, b FROM t",
        "CREATE TRIGGER v_upd INSTEAD OF UPDATE OF a ON v BEGIN UPDATE t SET a=NEW.a WHERE a=OLD.a; END",
    ]);
    // SET b only: both engines reject.
    let r1 = db.execute("UPDATE v SET b='y' WHERE a=1", []);
    let r2 = con.execute_batch("UPDATE v SET b='y' WHERE a=1");
    assert!(
        r1.is_err(),
        "engine must reject non-matching UPDATE OF, got {:?}",
        r1
    );
    assert!(r2.is_err(), "SQLite rejects non-matching UPDATE OF");
    diff(&con, &db, "SELECT * FROM v ORDER BY a");
    // SET a: fires.
    db.execute("UPDATE v SET a=5 WHERE a=1", []).unwrap();
    con.execute_batch("UPDATE v SET a=5 WHERE a=1").unwrap();
    diff(&con, &db, "SELECT * FROM v ORDER BY a");
    // SET a AND b: fires once — the trigger body alone decides the write
    // (b stays 'x', pinned).
    db.execute("UPDATE v SET a=6, b='z' WHERE a=5", []).unwrap();
    con.execute_batch("UPDATE v SET a=6, b='z' WHERE a=5")
        .unwrap();
    diff(&con, &db, "SELECT * FROM v ORDER BY a");
}

#[test]
fn adv_view_no_trigger_errors() {
    let (mut db, con) = setup_both(&["CREATE TABLE t(a)", "CREATE VIEW v AS SELECT a FROM t"]);
    let r1 = db.execute("INSERT INTO v VALUES (1)", []);
    let r2 = con.execute_batch("INSERT INTO v VALUES (1)");
    assert!(
        r1.is_err(),
        "engine must reject view DML without INSTEAD OF"
    );
    assert!(r2.is_err());
}

#[test]
fn adv_view_insert_column_list() {
    let (mut db, con) = setup_both(&[
        "CREATE TABLE t(a, b)",
        "CREATE VIEW v AS SELECT b AS x, a AS y FROM t",
        "CREATE TRIGGER v_ins INSTEAD OF INSERT ON v BEGIN INSERT INTO t(a, b) VALUES (NEW.y, NEW.x); END",
    ]);
    db.execute("INSERT INTO v(y, x) VALUES (1, 'one')", [])
        .unwrap();
    con.execute_batch("INSERT INTO v(y, x) VALUES (1, 'one')")
        .unwrap();
    diff(&con, &db, "SELECT * FROM t ORDER BY a");
}

// ---------------------------------------------------------------------------
// 5. Partial index: UPDATE moving row in/out of predicate
// ---------------------------------------------------------------------------

#[test]
fn adv_partial_index_update_moves_row() {
    let (mut db, con) = setup_both(&[
        "CREATE TABLE u(a, b)",
        "CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0",
        "INSERT INTO u VALUES (1, 1)",
        "INSERT INTO u VALUES (2, -1)",
        "INSERT INTO u VALUES (2, -2)",
    ]);
    // Move row 2 out of the predicate (b=-1 -> b=5 keeps a=2 unique? no —
    // a=2 then collides with nothing since only a=1 is indexed).
    db.execute("UPDATE u SET b=5 WHERE b=-1", []).unwrap();
    con.execute_batch("UPDATE u SET b=5 WHERE b=-1").unwrap();
    diff(&con, &db, "SELECT * FROM u ORDER BY a, b");
    // Now a=2, b=5 is indexed; moving the other a=2 row in must conflict.
    let r1 = db.execute("UPDATE u SET b=7 WHERE b=-2", []);
    let r2 = con.execute_batch("UPDATE u SET b=7 WHERE b=-2");
    assert!(r1.is_err(), "partial-unique conflict expected");
    assert!(r2.is_err());
    diff(&con, &db, "SELECT * FROM u ORDER BY a, b");
    // Move a=2/b=5 out of the index, then a=2/b=7 should succeed.
    db.execute("UPDATE u SET b=-9 WHERE b=5", []).unwrap();
    con.execute_batch("UPDATE u SET b=-9 WHERE b=5").unwrap();
    db.execute("UPDATE u SET b=7 WHERE b=-2", []).unwrap();
    con.execute_batch("UPDATE u SET b=7 WHERE b=-2").unwrap();
    diff(&con, &db, "SELECT * FROM u ORDER BY a, b");
}

#[test]
fn adv_partial_index_key_update() {
    // UPDATE the indexed key itself.
    let (mut db, con) = setup_both(&[
        "CREATE TABLE u(a, b)",
        "CREATE UNIQUE INDEX ui ON u(a) WHERE b > 0",
        "INSERT INTO u VALUES (1, 1)",
        "INSERT INTO u VALUES (5, 1)",
    ]);
    db.execute("UPDATE u SET a=9 WHERE a=1", []).unwrap();
    con.execute_batch("UPDATE u SET a=9 WHERE a=1").unwrap();
    diff(&con, &db, "SELECT * FROM u ORDER BY a");
    // Conflict via update.
    let r1 = db.execute("UPDATE u SET a=9 WHERE a=5", []);
    let r2 = con.execute_batch("UPDATE u SET a=9 WHERE a=5");
    assert!(r1.is_err());
    assert!(r2.is_err());
}

// ---------------------------------------------------------------------------
// 6. HAVING alias edge cases
// ---------------------------------------------------------------------------

#[test]
fn adv_having_alias_shadow() {
    // Alias `v` shadows the real column `v`: SQLite semantics — alias
    // wins in HAVING.
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, v AS v2 FROM c GROUP BY k HAVING v2 > 2 ORDER BY k",
    );
}

#[test]
fn adv_having_alias_in_arith() {
    let (db, con) = setup_both(&[
        "CREATE TABLE c(k, v)",
        "INSERT INTO c VALUES ('a',1),('a',5),('b',10)",
    ]);
    diff(
        &con,
        &db,
        "SELECT k, SUM(v) AS s FROM c GROUP BY k HAVING s * 2 > 8 ORDER BY k",
    );
    diff(
        &con,
        &db,
        "SELECT k, COUNT(*) AS n FROM c GROUP BY k HAVING n >= 2 ORDER BY k",
    );
}

// ---------------------------------------------------------------------------
// 7. Window EXCLUDE TIES with peers + RANGE frames
// ---------------------------------------------------------------------------

#[test]
fn adv_exclude_ties_peers() {
    let (db, con) = setup_both(&[
        "CREATE TABLE h(k, x)",
        "INSERT INTO h VALUES ('a',1),('a',1),('a',2),('a',3)",
    ]);
    for sql in [
        "SELECT x, SUM(x) OVER (PARTITION BY k ORDER BY x ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE TIES) FROM h ORDER BY x",
        "SELECT x, SUM(x) OVER (PARTITION BY k ORDER BY x RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE TIES) FROM h ORDER BY x",
        "SELECT x, SUM(x) OVER (PARTITION BY k ORDER BY x RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE GROUP) FROM h ORDER BY x",
        "SELECT x, SUM(x) OVER (PARTITION BY k ORDER BY x GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM h ORDER BY x",
    ] {
        diff(&con, &db, sql);
    }
}

// ---------------------------------------------------------------------------
// 8. likelihood argument validation
// ---------------------------------------------------------------------------

#[test]
fn adv_likelihood_edge() {
    let db = mem();
    let con = lite();
    // Out-of-range P: SQLite ERRORS.
    let r1 = db.query("SELECT likelihood(1, 1.5)", []);
    let r2 = lite_rows_safe(&con, "SELECT likelihood(1, 1.5)");
    assert!(r2.is_err(), "SQLite must reject P=1.5");
    assert!(
        r1.is_err(),
        "engine must reject P=1.5 too, got {:?}",
        r1.map(|_| ())
    );
    let r1 = db.query("SELECT likelihood(1, -0.5)", []);
    let r2 = lite_rows_safe(&con, "SELECT likelihood(1, -0.5)");
    assert!(r2.is_err());
    assert!(r1.is_err(), "engine must reject P=-0.5 too");
}

// ---------------------------------------------------------------------------
// 9. Unknown functions now error — verify a couple shapes
// ---------------------------------------------------------------------------

#[test]
fn adv_unknown_function_errors() {
    let db = mem();
    let con = lite();
    assert!(db.query("SELECT nonexistent_fn(1)", []).is_err());
    assert!(lite_rows_safe(&con, "SELECT nonexistent_fn(1)").is_err());
    // But overridable/registered names still work.
    diff(&con, &db, "SELECT abs(-5), upper('a'), length('abc')");
}

// ---------------------------------------------------------------------------
// 10. Multi-row VALUES interplay
// ---------------------------------------------------------------------------

#[test]
fn adv_values_set_ops_chain() {
    // SQLite grammar (pinned): ORDER BY / LIMIT may follow a select
    // statement only when the FINAL compound term is a SELECT core — a
    // trailing VALUES core cannot carry them. Statements WITHOUT a
    // trailing-VALUES core parse and produce identical results.
    let db = mem();
    let con = lite();
    for sql in [
        // Legal shapes (final term is a SELECT).
        "VALUES (1) UNION SELECT 2 ORDER BY 1",
        "SELECT 1 UNION VALUES (2) UNION SELECT 3 ORDER BY 1",
        "VALUES (1),(2) UNION SELECT 3 ORDER BY 1",
        "VALUES (1,'a'),(2,'b') UNION SELECT 2,'c' ORDER BY 1",
        "VALUES (1),(2) EXCEPT SELECT 2 ORDER BY 1",
        "VALUES (1),(2) INTERSECT VALUES (2),(3)",
        "SELECT count(*) FROM (VALUES (1),(2),(3))",
        "SELECT * FROM (VALUES (1),(2)) UNION ALL SELECT 3 ORDER BY 1",
        // Trailing-VALUES cores: syntax errors on BOTH engines.
        "VALUES (1) UNION VALUES (2) ORDER BY 1",
        "VALUES (1),(2) UNION VALUES (2),(3) ORDER BY 1",
        "VALUES (1),(2) EXCEPT VALUES (2) ORDER BY 1",
        "VALUES (1) UNION VALUES (2) LIMIT 1",
        "VALUES (1),(2) ORDER BY 1 DESC",
        "VALUES (1),(2) LIMIT 1",
        "SELECT 1 UNION VALUES (2) ORDER BY 1",
    ] {
        let (ours, theirs) = both_engine_strs(&db, &con, sql);
        let both_err = ours.iter().any(|s| s.starts_with("__ERR__"))
            && theirs.iter().any(|s| s.starts_with("__ERR__"));
        let both_ok = !ours.iter().any(|s| s.starts_with("__ERR__"))
            && !theirs.iter().any(|s| s.starts_with("__ERR__"));
        assert!(
            both_err || both_ok,
            "parse mismatch on {}: ours {:?} theirs {:?}",
            sql,
            ours,
            theirs
        );
        if both_ok {
            assert_eq!(ours, theirs, "result mismatch on {}", sql);
        }
    }
}

// ---------------------------------------------------------------------------
// 11. FK clause order + duplicates
// ---------------------------------------------------------------------------

#[test]
fn adv_fk_double_clause_rejected() {
    // SQLite (pinned): duplicate ON DELETE / ON UPDATE clauses are
    // ACCEPTED (the refargs grammar rule has no uniqueness check) and
    // the LAST clause of each kind WINS — CASCADE-then-SET-NULL behaves
    // as SET NULL; the reversed order behaves as CASCADE.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY)",
        "CREATE TABLE x(a REFERENCES p ON DELETE CASCADE ON DELETE SET NULL)",
        "CREATE TABLE y(a REFERENCES p ON DELETE SET NULL ON DELETE CASCADE)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO x VALUES (1)",
        "INSERT INTO y VALUES (1)",
    ]);
    db.execute("DELETE FROM p", []).unwrap();
    con.execute_batch("DELETE FROM p").unwrap();
    // ON DELETE CASCADE ON DELETE SET NULL — last wins: SET NULL.
    diff(&con, &db, "SELECT * FROM x");
    // ON DELETE SET NULL ON DELETE CASCADE — last wins: CASCADE.
    diff(&con, &db, "SELECT count(*) FROM y");
    // The same overwrite rule for ON UPDATE, either order.
    let (mut db, con) = setup_both(&[
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE p(id INTEGER PRIMARY KEY)",
        "CREATE TABLE x(a REFERENCES p ON UPDATE CASCADE ON UPDATE SET NULL)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO x VALUES (1)",
    ]);
    db.execute("UPDATE p SET id=9", []).unwrap();
    con.execute_batch("UPDATE p SET id=9").unwrap();
    diff(&con, &db, "SELECT * FROM x");
}

// ---------------------------------------------------------------------------
// 12. Trigger ordering: BEFORE fires before write; RAISE works
// ---------------------------------------------------------------------------

#[test]
fn adv_trigger_order_and_raise() {
    let (mut db, con) = setup_both(&[
        "CREATE TABLE t(a)",
        "CREATE TABLE log(m)",
        "CREATE TRIGGER bu BEFORE UPDATE ON t BEGIN INSERT INTO log VALUES ('before'); END",
        "CREATE TRIGGER au AFTER UPDATE ON t BEGIN INSERT INTO log VALUES ('after'); END",
        "INSERT INTO t VALUES (1)",
    ]);
    db.execute("UPDATE t SET a=2", []).unwrap();
    con.execute_batch("UPDATE t SET a=2").unwrap();
    diff(&con, &db, "SELECT * FROM log ORDER BY rowid");

    // RAISE(ABORT) in BEFORE trigger: statement aborts, row not changed.
    let (mut db2, con2) = setup_both(&[
        "CREATE TABLE t(a)",
        "CREATE TRIGGER bd BEFORE DELETE ON t WHEN OLD.a = 1 BEGIN SELECT RAISE(ABORT, 'nope'); END",
        "INSERT INTO t VALUES (1)",
    ]);
    let r1 = db2.execute("DELETE FROM t", []);
    let r2 = con2.execute_batch("DELETE FROM t");
    assert!(r1.is_err(), "engine must abort");
    assert!(r2.is_err());
    diff(&con2, &db2, "SELECT * FROM t");
}
