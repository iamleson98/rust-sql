//! Cross-schema UPDATE/DELETE (foreign target) — the mixed-DML
//! federation round: `UPDATE aux.t … WHERE … main.t …` executes whole
//! on the TARGET engine with the parent-side tables injected through
//! the channel (see `src/attach.rs::prepare_cross_dml`).
//!
//! The regression at the top pins the query-path wrong-table bug this
//! round closes (a mixed `UPDATE aux.t … RETURNING` used to plan
//! against the PARENT's same-named table and write THERE). The
//! differential cases at the bottom run the same programs through
//! rusqlite's bundled real SQLite.

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().expect("open")
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    let (_, rs) = db.query_with_columns(sql, []).expect(sql);
    rs.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(t) => t.to_string(),
                    Value::Blob(b) => format!("x'{}'", hex(b)),
                })
                .collect()
        })
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// The standard cross-DML sandbox: main.m (id, v) 1..=3, aux.a (id, v)
/// 1..=3 (same ids — the interesting intersection), aux.audit log.
fn sandbox() -> Database {
    let mut db = mem();
    db.execute("CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')", [])
        .unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')", [])
        .unwrap();
    db.execute("CREATE TABLE aux.audit (msg TEXT)", []).unwrap();
    db
}

#[test]
fn cross_update_subquery_from_main() {
    let mut db = sandbox();
    db.execute(
        "UPDATE aux.a SET v = 'm:' || (SELECT v FROM m WHERE m.id = aux.a.id) WHERE id <= 2",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.a ORDER BY id"),
        vec![vec!["1", "m:one"], vec!["2", "m:two"], vec!["3", "a3"],]
    );
    // main untouched.
    assert_eq!(
        rows(&db, "SELECT id, v FROM m ORDER BY id"),
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "three"],]
    );
}

#[test]
fn cross_update_in_subquery() {
    let mut db = sandbox();
    db.execute(
        "UPDATE aux.a SET v = 'hit' WHERE id IN (SELECT id FROM m WHERE v = 'two')",
        [],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT v FROM aux.a WHERE v = 'hit'").len(), 1);
    assert_eq!(
        rows(&db, "SELECT id FROM aux.a WHERE v = 'hit'"),
        vec![vec!["2"]]
    );
}

#[test]
fn cross_delete_in_subquery() {
    let mut db = sandbox();
    db.execute(
        "DELETE FROM aux.a WHERE id IN (SELECT id FROM m WHERE id >= 2)",
        [],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT id, v FROM aux.a"), vec![vec!["1", "a1"]]);
}

#[test]
fn cross_update_delete_where_exists() {
    let mut db = sandbox();
    db.execute(
        "UPDATE aux.a SET v = 'x' WHERE EXISTS (SELECT 1 FROM m WHERE m.id = aux.a.id AND m.v = 'one')",
        [],
    )
    .unwrap();
    db.execute(
        "DELETE FROM aux.a WHERE EXISTS (SELECT 1 FROM m WHERE m.id = aux.a.id AND m.v = 'three')",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.a ORDER BY id"),
        vec![vec!["1", "x"], vec!["2", "a2"]]
    );
}

/// THE REGRESSION: a mixed `UPDATE aux.t … RETURNING` through the QUERY
/// path used to plan against the PARENT's same-named table (writes
/// landed in MAIN, aux untouched, wrong rows returned).
#[test]
fn cross_update_returning_never_writes_main() {
    let mut db = mem();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1,'one'),(2,'two')", [])
        .unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO aux.t VALUES (1,'a1'),(2,'a2')", [])
        .unwrap();

    let got = db
        .query(
            "UPDATE aux.t SET v = 'ret' WHERE id IN (SELECT id FROM main.t) RETURNING id, v",
            [],
        )
        .unwrap();
    // The rows that came back are AUX's (v was aN before the set).
    assert_eq!(got.len(), 2, "RETURNING rows: {got:?}");
    // main.t untouched — the wrong-table bug.
    assert_eq!(
        rows(&db, "SELECT id, v FROM main.t ORDER BY id"),
        vec![vec!["1", "one"], vec!["2", "two"]],
        "main.t must not be touched by a cross-database UPDATE on aux.t"
    );
    // aux.t actually updated.
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.t ORDER BY id"),
        vec![vec!["1", "ret"], vec!["2", "ret"]]
    );
}

#[test]
fn cross_update_returning_values_from_target() {
    let db = sandbox();
    let got = db
        .query(
            "UPDATE aux.a SET v = 'u' || id WHERE id IN (SELECT id FROM m WHERE id <= 2) RETURNING id, v",
            [],
        )
        .unwrap();
    assert_eq!(got.len(), 2);
    let flat: Vec<String> = got
        .iter()
        .flat_map(|r| r.iter().map(|v| v.to_string()))
        .collect();
    assert!(flat.contains(&"u1".to_string()) && flat.contains(&"u2".to_string()));
    assert_eq!(
        rows(&db, "SELECT v FROM aux.a WHERE id = 3"),
        vec![vec!["a3"]]
    );
}

#[test]
fn cross_update_from_join() {
    let mut db = sandbox();
    // UPDATE ... FROM (SQLite 3.33+): the target joined with main's
    // table through the channel.
    db.execute(
        "UPDATE aux.a SET v = m.v FROM m WHERE aux.a.id = m.id AND m.id >= 2",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.a ORDER BY id"),
        vec![vec!["1", "a1"], vec!["2", "two"], vec!["3", "three"],]
    );
}

#[test]
fn cross_update_bare_and_qualified_main_refs() {
    let mut db = sandbox();
    // Unqualified `m` (main-resolved) AND explicit main.m in one
    // statement — both must reach the same materialization, and aux's
    // own tables must never shadow main's names.
    db.execute("CREATE TABLE aux.m (z INT)", []).unwrap(); // shadow bait
    db.execute("INSERT INTO aux.m VALUES (999)", []).unwrap();
    db.execute(
        "UPDATE aux.a SET v = (SELECT v FROM m WHERE m.id = aux.a.id) WHERE EXISTS (SELECT 1 FROM main.m WHERE main.m.id = aux.a.id)",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.a ORDER BY id"),
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "three"],]
    );
}

#[test]
fn cross_update_fires_target_triggers() {
    let mut db = sandbox();
    db.execute(
        "CREATE TRIGGER aux.a_au AFTER UPDATE ON aux.a BEGIN INSERT INTO audit VALUES ('up:' || new.id); END",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE aux.a SET v = 't' WHERE id IN (SELECT id FROM m WHERE id <= 2)",
        [],
    )
    .unwrap();
    // Triggers fired ON AUX (twice — once per matched row).
    assert_eq!(
        rows(&db, "SELECT msg FROM aux.audit ORDER BY msg"),
        vec![vec!["up:1"], vec!["up:2"]]
    );
}

#[test]
fn cross_update_respects_target_unique_constraint() {
    let mut db = sandbox();
    db.execute("CREATE UNIQUE INDEX aux.ix_v ON a(v)", [])
        .unwrap();
    // Every row would collide on 'same' — the whole statement fails
    // atomically on the TARGET (SQLite semantics), aux unchanged.
    let err = db
        .execute(
            "UPDATE aux.a SET v = 'same' WHERE id IN (SELECT id FROM m)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "err: {err}");
    assert_eq!(
        rows(&db, "SELECT count(*) FROM aux.a WHERE v = 'same'"),
        vec![vec!["0"]]
    );
    assert_eq!(
        rows(&db, "SELECT count(*) FROM aux.a"),
        vec![vec!["3"]],
        "the failed cross-UPDATE must leave aux.a untouched (statement atomicity)"
    );
}

#[test]
fn cross_dml_changes_counter() {
    let mut db = sandbox();
    db.execute(
        "UPDATE aux.a SET v = 'c' WHERE id IN (SELECT id FROM m WHERE id <= 2)",
        [],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT changes()"), vec![vec!["2"]]);
    db.execute("DELETE FROM aux.a WHERE id IN (SELECT id FROM m)", [])
        .unwrap();
    // All three rows match (m carries ids 1..=3): changes() = 3.
    assert_eq!(rows(&db, "SELECT changes()"), vec![vec!["3"]]);
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.a"), vec![vec!["0"]]);
}

#[test]
fn cross_update_three_part_column_refs() {
    let mut db = sandbox();
    // Three-part column references (main.m.v) in the WHERE/SET.
    db.execute(
        "UPDATE aux.a SET v = 'z' WHERE aux.a.id = (SELECT m.id FROM m WHERE main.m.v = 'two')",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.a WHERE v = 'z'"),
        vec![vec!["2", "z"]]
    );
}

#[test]
fn cross_dml_without_rowid_target() {
    let mut db = sandbox();
    db.execute(
        "CREATE TABLE aux.wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO aux.wr VALUES ('a', 1), ('b', 2)", [])
        .unwrap();
    db.execute(
        "UPDATE aux.wr SET v = v + 100 WHERE v IN (SELECT id FROM m WHERE id <= 2)",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT k, v FROM aux.wr ORDER BY k"),
        vec![vec!["a", "101"], vec!["b", "102"]]
    );
}

#[test]
fn cross_dml_through_transaction_spans_engines() {
    let mut db = sandbox();
    db.execute("BEGIN", []).unwrap();
    db.execute(
        "UPDATE aux.a SET v = 'tx' WHERE id IN (SELECT id FROM m WHERE id = 1)",
        [],
    )
    .unwrap();
    db.execute("UPDATE m SET v = 'txm' WHERE id = 1", [])
        .unwrap();
    db.execute("ROLLBACK", []).unwrap();
    // BOTH engines rolled back (the spanning transaction).
    assert_eq!(
        rows(&db, "SELECT v FROM aux.a WHERE id = 1"),
        vec![vec!["a1"]]
    );
    assert_eq!(rows(&db, "SELECT v FROM m WHERE id = 1"), vec![vec!["one"]]);
}

#[test]
fn cross_dml_second_attached_source() {
    // Target aux, source b (a THIRD database): the target's channel
    // carries b's table under its own name.
    let mut db = sandbox();
    db.execute("ATTACH ':memory:' AS b", []).unwrap();
    db.execute("CREATE TABLE b.src (id INT, w TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO b.src VALUES (2,'from-b')", [])
        .unwrap();
    db.execute(
        "UPDATE aux.a SET v = (SELECT w FROM b.src WHERE b.src.id = aux.a.id) WHERE id IN (SELECT id FROM b.src)",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT v FROM aux.a WHERE id = 2"),
        vec![vec!["from-b"]]
    );
    assert_eq!(
        rows(&db, "SELECT v FROM aux.a WHERE id = 1"),
        vec![vec!["a1"]]
    );
}

#[test]
fn cross_dml_with_clause_rejected_clearly() {
    let mut db = sandbox();
    let err = db
        .execute(
            "WITH src AS (SELECT id FROM m) UPDATE aux.a SET v = 'w' WHERE id IN (SELECT id FROM src)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("WITH"), "err: {err}");
}

#[test]
fn cross_dml_unknown_table_error_is_qualified() {
    let mut db = sandbox();
    let err = db
        .execute(
            "UPDATE aux.a SET v = 'x' WHERE id IN (SELECT id FROM main.nope)",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("main.nope") || err.to_string().contains("nope"),
        "err: {err}"
    );
    let err = db
        .execute(
            "UPDATE aux.nope SET v = 'x' WHERE id IN (SELECT id FROM m)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("aux.nope"), "err: {err}");
}

#[test]
fn cross_dml_update_limit_order() {
    let mut db = sandbox();
    db.execute(
        "UPDATE aux.a SET v = 'L' WHERE id IN (SELECT id FROM m) ORDER BY id DESC LIMIT 2",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT id, v FROM aux.a ORDER BY id"),
        vec![vec!["1", "a1"], vec!["2", "L"], vec!["3", "L"],]
    );
}

// ---------------------------------------------------------------------------
// Differential cases — the same program runs against real SQLite
// (rusqlite bundled) and this engine; outputs must match value-by-value.
// ---------------------------------------------------------------------------

fn diff_case(name: &str, sql: &[&str]) {
    let oracle = rusqlite::Connection::open_in_memory().expect("oracle open");
    let mut ours = mem();
    let mut oracle_cols = Vec::new();
    let mut oracle_rows: Vec<Vec<rusqlite::types::Value>> = Vec::new();
    for s in sql {
        let mut stmt = oracle.prepare(s).expect("oracle prepare");
        let ncols = stmt.column_count();
        if ncols > 0 {
            let names: Vec<String> = (0..ncols)
                .map(|i| stmt.column_name(i).unwrap().to_string())
                .collect();
            let mut rows = stmt.query([]).expect("oracle query");
            oracle_cols = names;
            oracle_rows.clear();
            while let Ok(Some(r)) = rows.next() {
                oracle_rows.push(
                    (0..ncols)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .unwrap_or(rusqlite::types::Value::Null)
                        })
                        .collect(),
                );
            }
        } else {
            let _ = stmt.execute([]);
        }
    }
    let mut statements: Vec<&str> = sql.to_vec();
    let last = statements.pop().unwrap();
    for s in &statements {
        ours.execute(s, [])
            .unwrap_or_else(|e| panic!("[{name}] ours failed on {s}: {e}"));
    }
    let (cols, rs) = ours
        .query_with_columns(last, [])
        .unwrap_or_else(|e| panic!("[{name}] ours query failed: {e}"));
    assert_eq!(
        cols, oracle_cols,
        "[{name}] column names diverge: ours {cols:?} vs sqlite {oracle_cols:?}"
    );
    assert_eq!(
        rs.len(),
        oracle_rows.len(),
        "[{name}] row count diverges (ours {rs:?} vs sqlite {oracle_rows:?})"
    );
    for (i, (o, m)) in rs.iter().zip(oracle_rows.iter()).enumerate() {
        for (j, (a, b)) in o.iter().zip(m.iter()).enumerate() {
            let same = match (a, b) {
                (Value::Null, rusqlite::types::Value::Null) => true,
                (Value::Integer(x), rusqlite::types::Value::Integer(y)) => x == y,
                (Value::Real(x), rusqlite::types::Value::Real(y)) => x == y,
                (Value::Text(x), rusqlite::types::Value::Text(y)) => x == y,
                (Value::Blob(x), rusqlite::types::Value::Blob(y)) => x == y,
                _ => false,
            };
            assert!(
                same,
                "[{name}] row {i} col {j} diverges: ours {a:?} vs sqlite {b:?}"
            );
        }
    }
}

#[test]
fn differential_cross_dml_suite() {
    diff_case(
        "cross_update_subquery",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "UPDATE aux.a SET v = 'm:' || (SELECT v FROM m WHERE m.id = aux.a.id) WHERE id <= 2",
            "SELECT id, v FROM aux.a ORDER BY id",
        ],
    );
    diff_case(
        "cross_update_in",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "UPDATE aux.a SET v = 'hit' WHERE id IN (SELECT id FROM m WHERE v = 'two')",
            "SELECT id, v FROM aux.a ORDER BY id",
        ],
    );
    diff_case(
        "cross_delete_in",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "DELETE FROM aux.a WHERE id IN (SELECT id FROM m WHERE id >= 2)",
            "SELECT id, v FROM aux.a ORDER BY id",
        ],
    );
    diff_case(
        "cross_update_from_join",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "UPDATE aux.a SET v = m.v FROM m WHERE aux.a.id = m.id AND m.id >= 2",
            "SELECT id, v FROM aux.a ORDER BY id",
        ],
    );
    diff_case(
        "cross_update_returning",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "UPDATE aux.a SET v = 'u' || id WHERE id IN (SELECT id FROM m WHERE id <= 2) RETURNING id, v",
        ],
    );
    diff_case(
        "cross_update_trigger",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "CREATE TABLE aux.audit (msg TEXT)",
            "CREATE TRIGGER aux.a_au AFTER UPDATE ON aux.a BEGIN INSERT INTO audit VALUES ('up:' || new.id); END",
            "UPDATE aux.a SET v = 't' WHERE id IN (SELECT id FROM m WHERE id <= 2)",
            "SELECT msg FROM aux.audit ORDER BY msg",
        ],
    );
    diff_case(
        "cross_update_main_untouched",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.m VALUES (1,'a1'),(2,'a2')",
            "UPDATE aux.m SET v = 'x' WHERE id IN (SELECT id FROM m)",
            "SELECT id, v FROM m ORDER BY id",
        ],
    );
    diff_case(
        "cross_update_changes",
        &[
            "CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO m VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO aux.a VALUES (1,'a1'),(2,'a2'),(3,'a3')",
            "UPDATE aux.a SET v = 'c' WHERE id IN (SELECT id FROM m WHERE id <= 2)",
            "SELECT changes()",
        ],
    );
}
