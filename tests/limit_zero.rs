//! `LIMIT 0` short-circuit (SQLite's computeLimitRegisters: a LIMIT that
//! is 0 jumps straight to the end of the statement — no row is read, and
//! no WHERE / projection / OFFSET expression is evaluated).
//!
//! The engine answers LIMIT 0 by running an EMPTIED copy of the plan
//! (every access leaf replaced by an empty row source carrying the
//! executor's own column names). This suite pins the two contracts:
//!
//! 1. the result COLUMNS are exactly those the ordinary execution reports
//!    (`LIMIT 1` over the same statement), on the `query_with_columns` path
//!    and — each against itself — the prepared-statement path: the
//!    `SELECT … LIMIT 0` metadata probe drivers send must not change shape;
//! 2. the outcome (rows / error) matches SQLite statement-by-statement —
//!    expressions SQLite never evaluates under LIMIT 0 must not raise here.

use rustqlite::{Database, StepResult, Value};

const SETUP: &[&str] = &[
    "CREATE TABLE t(x INTEGER PRIMARY KEY, y TEXT, z REAL)",
    "CREATE INDEX t_y ON t(y)",
    "CREATE TABLE u(x INTEGER, w TEXT COLLATE NOCASE)",
    "CREATE INDEX u_x ON u(x)",
    "CREATE TABLE k(id TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID",
    "CREATE TABLE n(a, b)",
    "CREATE VIEW vw AS SELECT x, y FROM t WHERE x > 1",
    "INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.5),(3,'a',-9223372036854775808)",
    "INSERT INTO u VALUES (1,'A'),(2,'b'),(9,'c')",
    "INSERT INTO k VALUES ('p',1),('q',-9223372036854775808)",
    "INSERT INTO n VALUES (1,2),(NULL,'x')",
];

/// Statements WITHOUT their LIMIT clause; the harness appends
/// `LIMIT 0` / `LIMIT 1`.
const SHAPES: &[&str] = &[
    "SELECT * FROM t",
    "SELECT rowid, * FROM t",
    "SELECT x, y FROM t WHERE z > 0",
    "SELECT * FROM t WHERE x = 2",
    "SELECT * FROM t WHERE x IN (1, 3)",
    "SELECT * FROM t WHERE x BETWEEN 1 AND 2",
    "SELECT y FROM t WHERE y = 'a'",
    "SELECT * FROM t WHERE y > 'a'",
    "SELECT * FROM t WHERE y IN ('a', 'b')",
    "SELECT count(*) FROM t",
    "SELECT count(*), sum(z), max(y) FROM t WHERE x > 0",
    "SELECT y, count(*) AS c FROM t GROUP BY y HAVING c > 0",
    "SELECT y, count(*) FROM t GROUP BY y ORDER BY 2 DESC",
    "SELECT * FROM t JOIN u ON t.x = u.x",
    "SELECT t.y, u.w FROM t JOIN u ON u.x = t.x WHERE t.z > 0",
    "SELECT * FROM t LEFT JOIN u USING (x)",
    "SELECT * FROM t, u",
    "SELECT * FROM t, u, k WHERE t.x = u.x AND k.v = t.x",
    "SELECT * FROM (SELECT x AS a, y || 'z' AS b FROM t) AS s",
    "SELECT s.a FROM (SELECT x AS a FROM t LIMIT 2) AS s",
    "SELECT * FROM vw",
    "WITH c AS (SELECT x, y FROM t) SELECT * FROM c",
    "WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 3) SELECT * FROM r",
    "SELECT x FROM t UNION SELECT x FROM u ORDER BY 1",
    "SELECT x FROM t UNION ALL SELECT v FROM k",
    "SELECT x FROM t INTERSECT SELECT x FROM u",
    "SELECT x FROM t EXCEPT SELECT x FROM u",
    "SELECT DISTINCT y FROM t",
    "SELECT x, row_number() OVER (ORDER BY x) AS rn FROM t",
    "SELECT y, sum(z) OVER (PARTITION BY y) FROM t",
    "SELECT * FROM k",
    "SELECT * FROM k WHERE id = 'p'",
    "SELECT * FROM n",
    "SELECT 1, 'a' AS b",
    "SELECT * FROM (VALUES (1, 2), (3, 4))",
    "SELECT * FROM json_each('[1,2]')",
    "SELECT x, (SELECT count(*) FROM u WHERE u.x = t.x) AS c FROM t",
    "SELECT x FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.x)",
    "SELECT x FROM t ORDER BY y, x",
    "SELECT x FROM t ORDER BY y COLLATE NOCASE",
    // Expressions SQLite never evaluates under LIMIT 0.
    "SELECT abs(-9223372036854775808) FROM t",
    "SELECT abs(-9223372036854775808) + count(*) FROM t",
    "SELECT id, abs(v) FROM k",
    "SELECT 1 FROM t WHERE abs(-9223372036854775808)",
    "SELECT 1 FROM t WHERE x IN (1, abs(-9223372036854775808))",
    "SELECT abs(v) FROM k ORDER BY 1",
    "SELECT id, abs(sum(v)) FROM k GROUP BY id",
    "SELECT * FROM k JOIN u ON abs(k.v) > u.x",
    "SELECT abs(-9223372036854775808)",
];

fn sqlite() -> rusqlite::Connection {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    for s in SETUP {
        c.execute_batch(s).unwrap();
    }
    c
}

fn ours() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    for s in SETUP {
        db.execute(s, []).unwrap();
    }
    db
}

fn sqlite_outcome(c: &rusqlite::Connection, sql: &str) -> Result<usize, String> {
    let mut st = c.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = st.raw_query();
    let mut n = 0;
    while rows.next().map_err(|e| e.to_string())?.is_some() {
        n += 1;
    }
    Ok(n)
}

/// Column names through the prepared-statement path (step to completion).
fn stmt_columns(db: &Database, sql: &str) -> Result<(Vec<String>, usize), String> {
    let mut st = db.prepare(sql).map_err(|e| e.to_string())?;
    let mut n = 0;
    while let StepResult::Row = st.step().map_err(|e| e.to_string())? {
        n += 1;
    }
    let cols = (0..st.column_count())
        .map(|i| st.column_name(i).unwrap_or_default().to_string())
        .collect();
    Ok((cols, n))
}

#[test]
fn limit_zero_columns_and_outcome_match() {
    let db = ours();
    let lite = sqlite();
    let mut failures = Vec::new();
    for shape in SHAPES {
        let q0 = format!("{shape} LIMIT 0");
        let q1 = format!("{shape} LIMIT 1");
        let ours0 = db.query_with_columns(&q0, []);
        let lite0 = sqlite_outcome(&lite, &q0);
        match (&ours0, &lite0) {
            (Ok((_, rows)), Ok(0)) if rows.is_empty() => {}
            (Err(_), Err(_)) => {}
            _ => failures.push(format!(
                "{q0}\n  ours   = {:?}\n  sqlite = {lite0:?}",
                ours0
                    .as_ref()
                    .map(|(_, r)| r.len())
                    .map_err(|e| e.to_string())
            )),
        }
        // Column-shape contract: LIMIT 0 reports what LIMIT 1 reports
        // (only where the ordinary execution itself succeeds).
        if let (Ok((cols0, _)), Ok((cols1, _))) = (&ours0, db.query_with_columns(&q1, [])) {
            if cols0 != &cols1 {
                failures.push(format!(
                    "{q0}\n  LIMIT 0 columns {cols0:?}\n  LIMIT 1 columns {cols1:?}"
                ));
            }
            match (stmt_columns(&db, &q0), stmt_columns(&db, &q1)) {
                (Ok((s0, 0)), Ok((s1, _))) if s0 == s1 => {}
                other => failures.push(format!("{q0}\n  prepared path: {other:?}")),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Computed limits: a parameter or subquery that evaluates to 0 takes the
/// same short-circuit (OP_IfNot), and OFFSET is never evaluated.
#[test]
fn computed_limit_zero_skips_offset_and_body() {
    let db = ours();
    let lite = sqlite();
    for sql in [
        "SELECT 1 FROM t LIMIT 0 OFFSET abs(-9223372036854775808)",
        "SELECT 1 FROM t LIMIT (SELECT 0) OFFSET abs(-9223372036854775808)",
        "SELECT abs(v) FROM k LIMIT 0.0",
        "SELECT abs(v) FROM k LIMIT '0'",
        "SELECT abs(v) FROM k LIMIT (SELECT count(*) FROM n WHERE a > 5)",
    ] {
        assert_eq!(sqlite_outcome(&lite, sql), Ok(0), "oracle: {sql}");
        let rows = db.query(sql, []).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(rows.is_empty(), "{sql}");
    }
    let rows = db
        .query("SELECT abs(v) FROM k LIMIT ?1", [Value::Integer(0)])
        .unwrap();
    assert!(rows.is_empty());
    // A non-zero computed limit still evaluates the body (and raises).
    assert!(db
        .query("SELECT abs(v) FROM k LIMIT ?1", [Value::Integer(5)])
        .is_err());
    assert!(sqlite_outcome(&lite, "SELECT abs(v) FROM k LIMIT 5").is_err());
}
