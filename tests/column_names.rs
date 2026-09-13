//! Output column-name contract tests — modeled on SQLite's colname.test.
//!
//! SQLite pins its result-column naming rules in colname.test because
//! every consumer that resolves output by NAME (sqlite3_column_name,
//! the CLI's .headers mode, sqlx's try_get("name"), sea-orm's row
//! mapping) depends on them. The rules under test:
//!
//! - **An explicit AS alias always wins** — on every execution route
//!   (materialized executor, precompiled COUNT fast paths, streaming
//!   scan/range/group drivers, RETURNING). Route divergence here is
//!   exactly the `SELECT COUNT(*) AS n` bug class the fast paths had:
//!   the alias was dropped and name-based consumers resolved nothing.
//! - **Unqualified short names**: `SELECT t.a` reports "a" (SQLite's
//!   short-column-name rule), never the qualified form.
//! - **Star expansion**: `*` / `t.*` expand to the underlying columns,
//!   unqualified; join stars may repeat names (duplicates are legal).
//! - **The rowid pseudo-column** renders as "rowid" (the engine
//!   normalizes all spellings — rowid/_rowid_/oid — to the canonical
//!   form) and NEVER leaks the internal hidden-slot sentinel, whose
//!   NUL prefix would make the C ABI's column_name return nothing at
//!   all (CString::new rejects interior NULs).
//! - **Aggregate display names**: unaliased aggregates report the call
//!   text ("COUNT(*)", "SUM(a)"); aliased ones report the alias.
//! - **Route parity**: the same statement reports IDENTICAL names
//!   through Database::query_with_columns (general path) and through
//!   prepare/step (fast paths + drivers).
//! - **Durability**: views with aliased projections keep their names
//!   across close/reopen.
//!
//! Run: cargo test --test column_names

use rustqlite::{Database, StepResult, Value};

// ===========================================================================
// Helpers
// ===========================================================================

/// Two tables with overlapping column name `a` (join duplicate-name
/// coverage) plus an index for covering-count shapes.
fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b TEXT)", [])
        .unwrap();
    db.execute("CREATE TABLE u (a INTEGER, x INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX idx_u_a ON u(a)", []).unwrap();
    db.execute(
        "INSERT INTO t (a, b) VALUES (1,'one'),(2,'two'),(3,'three')",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO u (a, x) VALUES (1,10),(2,20),(2,200)", [])
        .unwrap();
    db
}

/// Names through the general route (Database::query_with_columns).
fn q_names(db: &Database, sql: &str) -> Vec<String> {
    let (cols, rows) = db.query_with_columns(sql, []).expect(sql);
    // Queries in this suite must return at least one row so the name
    // check exercises the row-serving path too, not just metadata.
    assert!(!rows.is_empty(), "no rows for: {}", sql);
    cols
}

/// Names through the statement route (prepare / step) — fast paths and
/// streaming drivers announce columns after the first step.
fn s_names(db: &Database, sql: &str) -> Vec<String> {
    let mut stmt = db.prepare(sql).expect(sql);
    assert!(
        stmt.step().expect(sql) == StepResult::Row,
        "no row: {}",
        sql
    );
    let names: Vec<String> = (0..stmt.column_count())
        .map(|i| stmt.column_name(i).expect("name").to_string())
        .collect();
    names
}

// ===========================================================================
// SQLite short-column-name rule + alias precedence
// ===========================================================================

#[test]
fn bare_qualified_and_duplicate_names() {
    let db = setup();
    assert_eq!(q_names(&db, "SELECT a, b FROM t"), ["a", "b"]);
    // Qualified reference reports the SHORT name (SQLite rule).
    assert_eq!(q_names(&db, "SELECT t.a FROM t"), ["a"]);
    assert_eq!(q_names(&db, "SELECT t.a, t.b FROM t"), ["a", "b"]);
    // Explicit aliases always win.
    assert_eq!(q_names(&db, "SELECT a AS x, b AS y FROM t"), ["x", "y"]);
    assert_eq!(q_names(&db, "SELECT t.a AS x FROM t"), ["x"]);
    // Duplicate output names are legal (SQLite allows them).
    assert_eq!(q_names(&db, "SELECT a, a FROM t"), ["a", "a"]);
    assert_eq!(
        q_names(&db, "SELECT a, a AS a2, a FROM t"),
        ["a", "a2", "a"]
    );
}

#[test]
fn alias_precedence_across_expression_shapes() {
    let db = setup();
    // Every expression shape: alias beats any computed display name.
    assert_eq!(q_names(&db, "SELECT a + 1 AS inc FROM t"), ["inc"]);
    assert_eq!(
        q_names(
            &db,
            "SELECT t.a * 2 + u.x AS calc FROM t JOIN u ON t.a = u.a"
        ),
        ["calc"]
    );
    assert_eq!(q_names(&db, "SELECT CAST(a AS TEXT) AS s FROM t"), ["s"]);
    assert_eq!(
        q_names(
            &db,
            "SELECT CASE WHEN a = 1 THEN 'x' ELSE 'y' END AS c FROM t"
        ),
        ["c"]
    );
    assert_eq!(
        q_names(&db, "SELECT (SELECT MAX(x) FROM u) AS mx FROM t"),
        ["mx"]
    );
    assert_eq!(
        q_names(
            &db,
            "SELECT EXISTS(SELECT 1 FROM u WHERE a = 1) AS e FROM t"
        ),
        ["e"]
    );
    assert_eq!(q_names(&db, "SELECT -a AS neg FROM t"), ["neg"]);
    // Literals: SQLite names them by their text; the engine renders the
    // value — only the CONTRACT (a stable, non-empty, NUL-free name) is
    // pinned here, the alias form pins the exact value.
    let lit = q_names(&db, "SELECT 'hi' AS greeting FROM t");
    assert_eq!(lit, ["greeting"]);
    let plain_lit = q_names(&db, "SELECT 'hi' FROM t");
    assert_eq!(plain_lit.len(), 1);
    assert!(!plain_lit[0].is_empty() && !plain_lit[0].contains('\u{0}'));
}

// ===========================================================================
// Star expansion
// ===========================================================================

#[test]
fn star_expansion_names() {
    let db = setup();
    assert_eq!(q_names(&db, "SELECT * FROM t"), ["a", "b"]);
    assert_eq!(q_names(&db, "SELECT t.* FROM t"), ["a", "b"]);
    // Join star: both sides in order — duplicate `a` is legal.
    assert_eq!(
        q_names(&db, "SELECT * FROM t JOIN u ON t.a = u.a"),
        ["a", "b", "a", "x"]
    );
    // Mixed star + named column.
    assert_eq!(
        q_names(&db, "SELECT u.x, * FROM t JOIN u ON t.a = u.a"),
        ["x", "a", "b", "a", "x"]
    );
    // Subquery in FROM: the subquery's output names flow through.
    assert_eq!(q_names(&db, "SELECT * FROM (SELECT a FROM t) AS s"), ["a"]);
    assert_eq!(
        q_names(&db, "SELECT * FROM (SELECT a AS q FROM t) AS s"),
        ["q"]
    );
    assert_eq!(
        q_names(&db, "SELECT s.a FROM (SELECT a FROM t) AS s"),
        ["a"]
    );
    // Aliasing a star's result columns is not possible; aliasing the
    // qualified table-star projection column is done per-column.
    assert_eq!(q_names(&db, "SELECT b AS label FROM t"), ["label"]);
}

// ===========================================================================
// The rowid pseudo-column
// ===========================================================================

#[test]
fn rowid_pseudo_column_names() {
    let db = setup();
    // All spellings normalize to the canonical "rowid".
    assert_eq!(q_names(&db, "SELECT rowid FROM t"), ["rowid"]);
    assert_eq!(q_names(&db, "SELECT oid FROM t"), ["rowid"]);
    assert_eq!(q_names(&db, "SELECT _rowid_ FROM t"), ["rowid"]);
    // Alias wins as always.
    assert_eq!(q_names(&db, "SELECT rowid AS r FROM t"), ["r"]);
    // Qualified through an alias, and on join sides.
    assert_eq!(q_names(&db, "SELECT a.rowid FROM t a"), ["rowid"]);
    assert_eq!(
        q_names(&db, "SELECT a.rowid FROM t a JOIN u b ON a.a = b.a"),
        ["rowid"]
    );
    assert_eq!(
        q_names(&db, "SELECT b.rowid, a.a FROM t a JOIN u b ON a.a = b.a"),
        ["rowid", "a"]
    );
    // Mixed with ordinary columns and predicates (the hidden slot rides
    // the row without changing any other name).
    assert_eq!(
        q_names(&db, "SELECT rowid, a FROM t WHERE a > 0"),
        ["rowid", "a"]
    );
    // rowid in GROUP BY / ORDER BY output position.
    assert_eq!(
        q_names(&db, "SELECT rowid, COUNT(*) AS n FROM t GROUP BY a"),
        ["rowid", "n"]
    );
    // A real column named "rowid" shadows the pseudo-column (SQLite
    // resolution rule) — the USER column is what reports.
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE s (rowid TEXT, v INTEGER)", [])
        .unwrap();
    db2.execute("INSERT INTO s (rowid, v) VALUES ('k', 1)", [])
        .unwrap();
    assert_eq!(q_names(&db2, "SELECT rowid FROM s"), ["rowid"]);
    assert_eq!(q_names(&db2, "SELECT rowid AS key FROM s"), ["key"]);
    let rows = db2.query_with_columns("SELECT rowid FROM s", []).unwrap();
    assert_eq!(
        rows.1[0][0].as_text(),
        "k",
        "real column value, not the rowid"
    );
}

#[test]
fn hidden_rowid_sentinel_never_leaks_into_names() {
    // The internal hidden-slot sentinel ("\0rowid" / "prefix.\0rowid")
    // must NEVER surface as an output column name: a NUL in the name
    // makes the C ABI's column_name fail (CString::new rejects interior
    // NULs) and sqlx would see no name at all. Battery across routes.
    let db = setup();
    let battery = [
        "SELECT rowid FROM t",
        "SELECT oid FROM t",
        "SELECT _rowid_ FROM t",
        "SELECT rowid AS r FROM t",
        "SELECT rowid, * FROM t",
        "SELECT a.rowid FROM t a",
        "SELECT b.rowid FROM t a JOIN u b ON a.a = b.a",
        "SELECT rowid FROM t WHERE a > 1",
        "SELECT rowid FROM t ORDER BY rowid DESC",
        "SELECT rowid, a FROM t WHERE rowid BETWEEN 1 AND 2",
    ];
    for sql in battery {
        for name in q_names(&db, sql) {
            assert!(
                !name.contains('\u{0}'),
                "NUL leak in {:?} from {}",
                name,
                sql
            );
            assert!(!name.is_empty(), "empty name from {}", sql);
        }
        for name in s_names(&db, sql) {
            assert!(
                !name.contains('\u{0}'),
                "NUL leak (stmt) in {:?} from {}",
                name,
                sql
            );
            assert!(!name.is_empty(), "empty name (stmt) from {}", sql);
        }
    }
}

// ===========================================================================
// Aggregates
// ===========================================================================

#[test]
fn aggregate_display_names() {
    let db = setup();
    // Unaliased: the call text (SQLite short-column-name form).
    assert_eq!(q_names(&db, "SELECT COUNT(*) FROM t"), ["COUNT(*)"]);
    assert_eq!(
        q_names(&db, "SELECT COUNT(*) FROM u WHERE a = 2"),
        ["COUNT(*)"]
    );
    assert_eq!(q_names(&db, "SELECT SUM(x) FROM u"), ["SUM(x)"]);
    assert_eq!(
        q_names(&db, "SELECT COUNT(DISTINCT a) FROM t"),
        ["COUNT(DISTINCT a)"]
    );
    assert_eq!(
        q_names(&db, "SELECT SUM(x), AVG(x) FROM u"),
        ["SUM(x)", "AVG(x)"]
    );
    // Aliased: the alias.
    assert_eq!(q_names(&db, "SELECT COUNT(*) AS n FROM t"), ["n"]);
    assert_eq!(q_names(&db, "SELECT count(a) AS c FROM t"), ["c"]);
    assert_eq!(q_names(&db, "SELECT SUM(x) AS total FROM u"), ["total"]);
    // Mixed with GROUP BY keys and aliases.
    assert_eq!(
        q_names(&db, "SELECT b AS k, COUNT(*) AS n FROM t GROUP BY b"),
        ["k", "n"]
    );
    assert_eq!(
        q_names(&db, "SELECT a, COUNT(*) AS n FROM t GROUP BY a ORDER BY a"),
        ["a", "n"]
    );
}

// ===========================================================================
// Route parity — the divergence catcher
// ===========================================================================

#[test]
fn route_parity_query_vs_statement() {
    // The same statement must report IDENTICAL names through the
    // general route (query_with_columns) and the statement route
    // (prepare/step: COUNT fast paths, scan/range/filter/group
    // drivers). The COUNT(*)-alias bug was exactly a route divergence.
    let db = setup();
    let battery = [
        "SELECT a, b FROM t",
        "SELECT t.a FROM t",
        "SELECT a AS x FROM t",
        "SELECT a, a FROM t",
        "SELECT * FROM t",
        "SELECT t.* FROM t",
        "SELECT b AS label FROM t",
        "SELECT * FROM t JOIN u ON t.a = u.a",
        "SELECT * FROM (SELECT a FROM t) AS s",
        "SELECT rowid FROM t",
        "SELECT rowid AS r FROM t",
        "SELECT oid FROM t",
        "SELECT a.rowid FROM t a JOIN u b ON a.a = b.a",
        "SELECT COUNT(*) FROM t",
        "SELECT COUNT(*) AS n FROM t",
        "SELECT COUNT(*) FROM u WHERE a = 2",
        "SELECT COUNT(*) AS hits FROM u WHERE a = 2",
        "SELECT COUNT(DISTINCT a) FROM t",
        "SELECT SUM(x) FROM u",
        "SELECT SUM(x) AS total FROM u",
        "SELECT b AS k, COUNT(*) AS n FROM t GROUP BY b",
        "SELECT a AS x FROM t WHERE rowid BETWEEN 1 AND 2",
        "SELECT a AS x FROM t WHERE a > 1",
        "SELECT a AS x FROM t ORDER BY a DESC",
        "SELECT CAST(a AS TEXT) AS s FROM t",
        "SELECT (SELECT MAX(x) FROM u) AS mx FROM t",
        "SELECT EXISTS(SELECT 1 FROM u) AS e FROM t",
    ];
    for sql in battery {
        let q = q_names(&db, sql);
        let s = s_names(&db, sql);
        assert_eq!(q, s, "route divergence for: {}", sql);
    }
}

#[test]
fn count_fast_paths_match_materialized_names() {
    // Regression family: the COUNT fast paths (CountStar for the bare
    // shape, IndexCount for the covering-index shape) previously
    // reported the aggregate's display name and dropped the AS alias —
    // sqlx/sea-orm count() queries could not resolve by name. Both the
    // aliased and the unaliased forms must match the general route.
    let db = setup();
    let shapes = [
        "SELECT COUNT(*) FROM t",
        "SELECT COUNT(*) AS n FROM t",
        "SELECT COUNT(*) FROM u WHERE a = 2",
        "SELECT COUNT(*) AS hits FROM u WHERE a = 2",
    ];
    for sql in shapes {
        let q = q_names(&db, sql);
        let s = s_names(&db, sql);
        assert_eq!(q, s, "COUNT fast path name divergence for: {}", sql);
        assert!(
            s.len() == 1 && !s[0].is_empty() && !s[0].contains('\u{0}'),
            "bad COUNT name {:?} for {}",
            s,
            sql
        );
    }
    // Bound-parameter covering count through the statement route.
    let mut stmt = db
        .prepare("SELECT COUNT(*) AS hits FROM u WHERE a = ?")
        .unwrap();
    stmt.bind(1, Value::Integer(2)).unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_int(0), 2);
    assert_eq!(stmt.column_name(0), Some("hits"));
}

// ===========================================================================
// Views, RETURNING, subqueries
// ===========================================================================

#[test]
fn view_column_names() {
    let mut db = setup();
    db.execute("CREATE VIEW v AS SELECT a AS x, b FROM t", [])
        .unwrap();
    assert_eq!(q_names(&db, "SELECT * FROM v"), ["x", "b"]);
    assert_eq!(q_names(&db, "SELECT x AS y FROM v"), ["y"]);
    assert_eq!(q_names(&db, "SELECT v.x FROM v"), ["x"]);
    // Aggregate over a view keeps its own names.
    assert_eq!(q_names(&db, "SELECT COUNT(*) AS n FROM v"), ["n"]);
    // Join view + table: names compose.
    assert_eq!(
        q_names(&db, "SELECT v.x, u.x AS ux FROM v JOIN u ON v.x = u.a"),
        ["x", "ux"]
    );
}

#[test]
fn returning_column_names() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t (b) VALUES ('seed')", []).unwrap();

    let mut stmt = db
        .prepare("INSERT INTO t (b) VALUES ('x') RETURNING rowid")
        .unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_name(0), Some("rowid"));
    assert_eq!(stmt.column_int(0), 2);
    while stmt.step().unwrap() == StepResult::Row {}

    let mut stmt = db
        .prepare("INSERT INTO t (b) VALUES ('y') RETURNING rowid AS r, b AS label")
        .unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_name(0), Some("r"));
    assert_eq!(stmt.column_name(1), Some("label"));
    assert_eq!(stmt.column_int(0), 3);
    assert_eq!(stmt.column_text(1).unwrap(), "y");

    let mut stmt = db
        .prepare("UPDATE t SET b = b WHERE id = 1 RETURNING id, b AS bb")
        .unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_name(0), Some("id"));
    assert_eq!(stmt.column_name(1), Some("bb"));

    // The rowid pseudo-column's VALUE in every DML flavor (the name
    // alone was never enough: RETURNING rowid must report the affected
    // row's rowid — the new one after a rowid move, the pre-delete one
    // for DELETE).
    let mut stmt = db
        .prepare("DELETE FROM t WHERE id = 2 RETURNING rowid AS r, b")
        .unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_name(0), Some("r"));
    assert_eq!(
        stmt.column_int(0),
        2,
        "DELETE returns the deleted row's rowid"
    );
    assert_eq!(stmt.column_text(1).unwrap(), "x");

    // Rowid MOVE through UPDATE: SQLite reports the NEW rowid.
    let mut stmt = db
        .prepare("UPDATE t SET id = 100 WHERE id = 3 RETURNING rowid, id")
        .unwrap();
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    assert_eq!(stmt.column_name(0), Some("rowid"));
    assert_eq!(
        stmt.column_int(0),
        100,
        "RETURNING rowid reports the moved-to rowid"
    );
    assert_eq!(stmt.column_int(1), 100);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t WHERE id = 100", [])
            .unwrap()[0][0]
            .as_integer(),
        1
    );
}

// ===========================================================================
// Durability: names survive close/reopen
// ===========================================================================

fn tmpdb(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("rustqlite_colname_{}.db", name));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn view_and_projection_names_survive_reopen() {
    let path = tmpdb("reopen");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (a INTEGER, b TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO t (a, b) VALUES (1,'one'),(2,'two')", [])
            .unwrap();
        db.execute("CREATE VIEW v AS SELECT a AS x, b FROM t", [])
            .unwrap();
        // Pin the names BEFORE close.
        assert_eq!(q_names(&db, "SELECT * FROM v"), ["x", "b"]);
        assert_eq!(q_names(&db, "SELECT COUNT(*) AS n FROM t"), ["n"]);
        db.flush().unwrap();
        drop(db);
    }
    {
        let db = Database::open(&path).unwrap();
        // Same names after reopen — the view's aliased projection is
        // part of the persisted schema, and the naming contract must
        // hold for every fresh session.
        assert_eq!(q_names(&db, "SELECT * FROM v"), ["x", "b"]);
        assert_eq!(q_names(&db, "SELECT x AS y FROM v"), ["y"]);
        assert_eq!(q_names(&db, "SELECT COUNT(*) AS n FROM t"), ["n"]);
        assert_eq!(q_names(&db, "SELECT COUNT(*) FROM t"), ["COUNT(*)"]);
        assert_eq!(q_names(&db, "SELECT rowid FROM t"), ["rowid"]);
        let _ = std::fs::remove_file(&path);
    }
}
