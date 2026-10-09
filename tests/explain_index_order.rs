//! EXPLAIN QUERY PLAN shows the index-order plans the executor runs:
//! `ORDER BY <indexed key> LIMIT k` walks the index (no temp b-tree) and
//! a lone `min(c)` / `max(c)` seeks it. Shapes the executor sorts or scans
//! keep their sort / scan rows.

use rustqlite::{Database, Value};

fn plan(db: &Database, sql: &str) -> Vec<String> {
    db.query(&format!("EXPLAIN QUERY PLAN {sql}"), [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[3] {
            Value::Text(t) => t.to_string(),
            v => panic!("detail column: {v:?}"),
        })
        .collect()
}

#[test]
fn index_order_plans_are_explained() {
    let mut db = Database::open_in_memory().unwrap();
    for sql in [
        "CREATE TABLE t (a INTEGER, b TEXT)",
        "CREATE INDEX ta ON t(a)",
        "CREATE TABLE w (k TEXT PRIMARY KEY, v) WITHOUT ROWID",
        "CREATE INDEX wv ON w(v)",
    ] {
        db.execute(sql, []).unwrap();
    }
    assert_eq!(
        plan(&db, "SELECT * FROM t ORDER BY a LIMIT 5"),
        ["SCAN t USING INDEX ta"]
    );
    assert_eq!(
        plan(
            &db,
            "SELECT b FROM t WHERE a > 3 ORDER BY a DESC LIMIT ? OFFSET 2"
        ),
        ["SEARCH t USING INDEX ta (a>?)"]
    );
    assert_eq!(
        plan(&db, "SELECT max(a) FROM t"),
        ["SEARCH t USING INDEX ta"]
    );
    assert_eq!(
        plan(&db, "SELECT min(a) FROM t WHERE a < 10"),
        ["SEARCH t USING INDEX ta (a<?)"]
    );
    // Sorted / scanned shapes.
    let sorted = ["SCAN t", "USE TEMP B-TREE FOR ORDER BY"];
    assert_eq!(plan(&db, "SELECT * FROM t ORDER BY b LIMIT 5"), sorted);
    assert_eq!(plan(&db, "SELECT * FROM t ORDER BY a LIMIT -1"), sorted);
    assert_eq!(plan(&db, "SELECT * FROM t ORDER BY a"), sorted);
    assert_eq!(plan(&db, "SELECT min(a), max(a) FROM t"), ["SCAN t"]);
    assert_eq!(plan(&db, "SELECT max(a) FROM t GROUP BY b").len(), 2);
    // WITHOUT ROWID tables take neither path.
    assert_eq!(plan(&db, "SELECT max(v) FROM w"), ["SCAN w"]);
}
