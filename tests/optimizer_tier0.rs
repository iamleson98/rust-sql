//! Tier-0 optimizer features: constant folding, LIKE/GLOB-prefix index
//! pushdown, covering index scans (rowid-only), and the planner
//! soundness fix for multi-bound range conjunctions.

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn one(db: &Database, sql: &str) -> Value {
    let rows = db.query(sql, []).unwrap();
    rows.first()
        .and_then(|r| r.first().cloned())
        .unwrap_or(Value::Null)
}

fn int(db: &Database, sql: &str) -> i64 {
    match one(db, sql) {
        Value::Integer(i) => i,
        v => panic!("expected INTEGER from `{sql}`, got {v:?}"),
    }
}

fn text(db: &Database, sql: &str) -> String {
    match one(db, sql) {
        Value::Text(s) => s.to_string(),
        v => panic!("expected TEXT from `{sql}`, got {v:?}"),
    }
}

fn explain(db: &Database, sql: &str) -> Vec<String> {
    let rows = db.query(sql, []).unwrap();
    rows.iter().map(|r| r[3].as_text().to_string()).collect()
}

#[test]
fn constant_folding_values() {
    let d = db();
    // literal arithmetic in projections
    assert_eq!(int(&d, "SELECT 2+3*4"), 14);
    assert_eq!(int(&d, "SELECT (2+3)*4"), 20);
    // SQLite semantics preserved: division by zero is NULL, integer
    // overflow promotes to real.
    assert_eq!(one(&d, "SELECT 1/0"), Value::Null);
    assert_eq!(int(&d, "SELECT 2/2"), 1);
    match one(&d, "SELECT 9223372036854775807 + 1") {
        Value::Real(f) => assert!(f > 9.22e18),
        v => panic!("expected REAL, got {v:?}"),
    }
    // concatenation + comparison folds
    assert_eq!(text(&d, "SELECT 'a' || 'b' || 'c'"), "abc");
    assert_eq!(int(&d, "SELECT 2 < 3"), 1);
    assert_eq!(int(&d, "SELECT 'a' = 'a'"), 1);
    // unary
    assert_eq!(int(&d, "SELECT -5 + 10"), 5);
    assert_eq!(int(&d, "SELECT NOT 0"), 1);
    // functions are NOT folded (random() must stay per-row)
    let a = int(&d, "SELECT abs(-3)");
    assert_eq!(a, 3);
    // coalesce with literal prefix collapses
    assert_eq!(int(&d, "SELECT coalesce(NULL, NULL, 42)"), 42);
    assert_eq!(one(&d, "SELECT coalesce(NULL, NULL)"), Value::Null);
}

#[test]
fn constant_folding_where() {
    let mut d = db();
    d.execute("CREATE TABLE t(a INTEGER, b TEXT)", []).unwrap();
    for i in 0..10 {
        d.execute(
            "INSERT INTO t VALUES (?, ?)",
            [Value::Integer(i), Value::Text(format!("v{}", i).into())],
        )
        .unwrap();
    }
    // WHERE 1=1 is eliminated entirely (no filter rows lost)
    let rows = d.query("SELECT COUNT(*) FROM t WHERE 1=1", []).unwrap();
    assert!(matches!(rows[0][0], Value::Integer(10)));
    // AND with constant folds to the real predicate
    let rows = d
        .query("SELECT COUNT(*) FROM t WHERE 1=1 AND a > 7", [])
        .unwrap();
    assert!(matches!(rows[0][0], Value::Integer(2)));
    // FALSE filter: nothing passes
    let rows = d.query("SELECT COUNT(*) FROM t WHERE 1=0", []).unwrap();
    assert!(matches!(rows[0][0], Value::Integer(0)));
    // fold inside a comparison's operands
    let rows = d.query("SELECT COUNT(*) FROM t WHERE a > 2+3", []).unwrap();
    assert!(matches!(rows[0][0], Value::Integer(4)));
    // OR(false, x) keeps x
    let rows = d
        .query("SELECT COUNT(*) FROM t WHERE 0 OR a = 3", [])
        .unwrap();
    assert!(matches!(rows[0][0], Value::Integer(1)));
}

#[test]
fn constant_folding_value_context_not_identity() {
    let d = db();
    // SELECT 1 AND 5 is 1 — NOT 5 (SQLite AND yields 0/1/NULL): the fold
    // must not apply the boolean identity in value context.
    assert_eq!(int(&d, "SELECT 1 AND 5"), 1);
    assert_eq!(int(&d, "SELECT 0 OR 5"), 1);
    assert_eq!(one(&d, "SELECT NULL AND 1"), Value::Null);
}

#[test]
fn like_prefix_pushdown_results() {
    let mut d = db();
    d.execute(
        "CREATE TABLE t(id INTEGER, code TEXT);
         INSERT INTO t VALUES (1, 'ORD-2024-a'), (2, 'ORD-2023-b'),
                              (3, 'INV-2024-c'), (4, 'ord-2024-d'),
                              (5, 'ORDX-2026-e')",
        [],
    )
    .unwrap();
    d.execute("CREATE INDEX icode ON t(code)", []).unwrap();
    // LETTERS in the prefix on a BINARY index: no pushdown (case variants
    // would under-select) — results still correct via scan+filter, and
    // LIKE is case-insensitive so 'ord-2024-d' and 'ORD-2023-b' match.
    let rows = d
        .query("SELECT id FROM t WHERE code LIKE 'ORD-%' ORDER BY id", [])
        .unwrap();
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => -1,
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 4]);
    // embedded wildcard: no pushdown, still correct
    let rows = d
        .query("SELECT id FROM t WHERE code LIKE '%INV%'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    // GLOB is case-sensitive
    let rows = d
        .query("SELECT id FROM t WHERE code GLOB 'ORD-*' ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 2); // ids 1 and 5
                               // exact literal LIKE (no wildcard) -> equality-shaped range
    let rows = d
        .query("SELECT id FROM t WHERE code LIKE 'ORD-2023-b'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn like_prefix_pushdown_explain() {
    let mut d = db();
    d.execute("CREATE TABLE t(id INTEGER, code TEXT)", [])
        .unwrap();
    d.execute("CREATE INDEX icode ON t(code)", []).unwrap();
    // Digit-leading prefix (no ASCII letters): pushdown is sound on a
    // BINARY index — the plan shows an index RANGE (was: full SCAN).
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT id FROM t WHERE code LIKE '2024-%'",
    );
    assert!(
        details
            .iter()
            .any(|x| x.contains("SEARCH t USING INDEX icode")),
        "details: {:?}",
        details
    );
    // Letter-leading prefix on a BINARY index: NO pushdown (case
    // insensitivity would under-select through the range).
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT id FROM t WHERE code LIKE 'ORD-%'",
    );
    assert!(
        !details.iter().any(|x| x.contains("USING INDEX")),
        "details: {:?}",
        details
    );
    // Embedded wildcards do NOT produce a range
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT id FROM t WHERE code LIKE '%X%'",
    );
    assert!(
        !details.iter().any(|x| x.contains("USING INDEX")),
        "details: {:?}",
        details
    );
}

#[test]
fn like_prefix_nocase_index_letters() {
    let mut d = db();
    d.execute(
        "CREATE TABLE t(id INTEGER, name TEXT);
         INSERT INTO t VALUES (1, 'Alice'), (2, 'ALICE'), (3, 'alicia'), (4, 'Bob')",
        [],
    )
    .unwrap();
    d.execute("CREATE INDEX iname ON t(name COLLATE NOCASE)", [])
        .unwrap();
    // Letter prefix on a NOCASE index: sound pushdown, case-insensitive
    // matches all survive.
    let rows = d
        .query("SELECT id FROM t WHERE name LIKE 'ali%' ORDER BY id", [])
        .unwrap();
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => -1,
        })
        .collect();
    // Alice, ALICE, and alicia all start with 'ali' case-insensitively.
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn like_prefix_binary_index_letters_no_pushdown() {
    let mut d = db();
    d.execute(
        "CREATE TABLE t(id INTEGER, name TEXT);
         INSERT INTO t VALUES (1, 'Alice'), (2, 'ALICE')",
        [],
    )
    .unwrap();
    d.execute("CREATE INDEX iname ON t(name)", []).unwrap();
    // BINARY index + letter prefix: NO range (case-insensitive LIKE would
    // under-select) — but the RESULTS still honor LIKE's case-insensitivity
    // via the scan+filter path.
    let rows = d
        .query("SELECT id FROM t WHERE name LIKE 'ali%' ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT id FROM t WHERE name LIKE 'ali%'",
    );
    assert!(
        !details.iter().any(|x| x.contains("USING INDEX")),
        "BINARY letter-prefix must not push down: {:?}",
        details
    );
}

#[test]
fn like_prefix_numeric_index_no_pushdown() {
    let mut d = db();
    d.execute(
        "CREATE TABLE t(id INTEGER, n INTEGER);
         INSERT INTO t VALUES (1, 123), (2, 456)",
        [],
    )
    .unwrap();
    d.execute("CREATE INDEX inum ON t(n)", []).unwrap();
    // INTEGER-affinity column: LIKE '1%' matches 123's TEXT form but the
    // numeric index keys order differently — pushdown is unsound and must
    // not fire. Results stay correct via the filter path.
    let rows = d.query("SELECT id FROM t WHERE n LIKE '1%'", []).unwrap();
    assert_eq!(rows.len(), 1);
    let details = explain(&d, "EXPLAIN QUERY PLAN SELECT id FROM t WHERE n LIKE '1%'");
    assert!(
        !details.iter().any(|x| x.contains("USING INDEX")),
        "numeric LIKE prefix must not push down: {:?}",
        details
    );
}

#[test]
fn covering_scan_rowid_only() {
    let mut d = db();
    d.execute("CREATE TABLE t(payload TEXT, code TEXT)", [])
        .unwrap();
    for i in 0..200 {
        d.execute(
            "INSERT INTO t VALUES (?, ?)",
            [
                Value::Text(format!("payload-{}", i).into()),
                Value::Text(format!("CODE-{:03}", i).into()),
            ],
        )
        .unwrap();
    }
    d.execute("CREATE INDEX icode ON t(code)", []).unwrap();
    // rowid-only projection over an index range: answered from the index
    let rows = d
        .query(
            "SELECT rowid FROM t WHERE code > 'CODE-095' AND code < 'CODE-100' ORDER BY rowid",
            [],
        )
        .unwrap();
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => -1,
        })
        .collect();
    // inserts are 0-based; rowids are 1-based: CODE-096..099 -> rowids 97..100
    assert_eq!(ids, vec![97, 98, 99, 100]);
    // EXPLAIN reports the covering index
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT rowid FROM t WHERE code > 'CODE-095' AND code < 'CODE-100'",
    );
    assert!(
        details
            .iter()
            .any(|x| x.contains("USING COVERING INDEX icode")),
        "details: {:?}",
        details
    );
}

#[test]
fn covering_scan_point_lookup() {
    let mut d = db();
    d.execute("CREATE TABLE t(payload TEXT, code TEXT)", [])
        .unwrap();
    for i in 0..50 {
        d.execute(
            "INSERT INTO t VALUES (?, ?)",
            [
                Value::Text(format!("p{}", i).into()),
                Value::Text(format!("c{}", i).into()),
            ],
        )
        .unwrap();
    }
    d.execute("CREATE INDEX icode ON t(code)", []).unwrap();
    let rows = d
        .query("SELECT rowid FROM t WHERE code = 'c42'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Integer(43)));
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT rowid FROM t WHERE code = 'c7'",
    );
    assert!(
        details
            .iter()
            .any(|x| x.contains("USING COVERING INDEX icode")),
        "details: {:?}",
        details
    );
}

#[test]
fn covering_scan_needs_real_values_for_columns() {
    let mut d = db();
    d.execute("CREATE TABLE t(payload TEXT, code TEXT)", [])
        .unwrap();
    for i in 0..20 {
        d.execute(
            "INSERT INTO t VALUES (?, ?)",
            [
                Value::Text(format!("p{}", i).into()),
                Value::Text(format!("c{}", i).into()),
            ],
        )
        .unwrap();
    }
    d.execute("CREATE INDEX icode ON t(code)", []).unwrap();
    // Non-rowid projections still fetch the table rows (order keys are
    // type-lossy) — correctness over cleverness.
    let rows = d
        .query("SELECT payload FROM t WHERE code = 'c7'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "p7");
    // Mixed projection: payload + rowid — no covering
    let rows = d
        .query("SELECT rowid, payload FROM t WHERE code = 'c7'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Integer(8)));
    assert_eq!(rows[0][1].as_text(), "p7");
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT rowid, payload FROM t WHERE code = 'c7'",
    );
    assert!(
        !details.iter().any(|x| x.contains("COVERING")),
        "mixed projection must not report covering: {:?}",
        details
    );
}

#[test]
fn multi_bound_range_conjunction_soundness() {
    // Regression for the bound-overwrite bug: `b > 'c' AND b > 'a'` used
    // to consume both conjuncts and answer with the LOOSER [a, inf)
    // range — returning rows the SQL excludes.
    let mut d = db();
    d.execute(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, b TEXT);
         INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')",
        [],
    )
    .unwrap();
    d.execute("CREATE INDEX ib ON t(b)", []).unwrap();
    let rows = d
        .query("SELECT id FROM t WHERE b > 'b' AND b > 'c' ORDER BY id", [])
        .unwrap();
    // Only 'd' satisfies BOTH conjuncts.
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Integer(4)));
    // Same-direction bounds with params
    let rows = d
        .query(
            "SELECT id FROM t WHERE b > ? AND b > ? ORDER BY id",
            [Value::Text("b".into()), Value::Text("c".into())],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    // BETWEEN + extra bound
    let rows = d
        .query(
            "SELECT id FROM t WHERE b BETWEEN 'a' AND 'd' AND b > 'b' ORDER BY id",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 2); // 'c' and 'd'
}

#[test]
fn folding_and_pushdown_compose() {
    let mut d = db();
    d.execute("CREATE TABLE t(id INTEGER, code TEXT)", [])
        .unwrap();
    for i in 0..100 {
        d.execute(
            "INSERT INTO t VALUES (?, ?)",
            [Value::Integer(i), Value::Text(format!("k{:03}", i).into())],
        )
        .unwrap();
    }
    d.execute("CREATE INDEX icode ON t(code)", []).unwrap();
    // Constant-folded bounds feed the index range: 'k0' || '50' -> 'k050'
    let rows = d
        .query(
            "SELECT rowid FROM t WHERE code > 'k0' || '49' AND code < 'k0' || '55'",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    let details = explain(
        &d,
        "EXPLAIN QUERY PLAN SELECT rowid FROM t WHERE code > 'k0' || '49' AND code < 'k0' || '55'",
    );
    assert!(
        details
            .iter()
            .any(|x| x.contains("USING COVERING INDEX icode")),
        "details: {:?}",
        details
    );
}
