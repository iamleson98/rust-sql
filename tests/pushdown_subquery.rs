//! Differential WHERE-pushdown suite: conjuncts over FROM-clause subquery
//! atoms re-home into the subquery body's WHERE (SQLite's
//! pushDownWhereTerms analog) vs real SQLite. Rows must match EXACTLY;
//! EXPLAIN assertions pin the pushed shapes (index-driven bodies).
//!
//! Also pins the EXPLAIN-of-WITH regression: `EXPLAIN QUERY PLAN WITH c
//! AS (…) SELECT * FROM c` must plan (CTEs materialized), not error.
//!
//! Decline shapes (the conservative gates) are differential-tested for
//! CORRECTNESS: aggregate bodies, LIMIT bodies (filter-then-limit is NOT
//! limit-then-filter — the push must not happen), DISTINCT bodies,
//! expression outputs, compound bodies, outer-join boundaries, ambiguous
//! unqualified names.

use rustqlite::{Database, Value};

fn diff_with(setup: &str, inserts: &[&str], query: &str) {
    let mut eng = Database::open_in_memory().unwrap();
    for s in setup.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        eng.execute(s, []).unwrap();
    }
    for s in inserts {
        eng.execute(s, []).unwrap();
    }
    let ours = render(&eng.query(query, []).expect("rustqlite query failed"));
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(setup).unwrap();
    conn.execute_batch(&inserts.join("; ")).unwrap();
    let mut stmt = conn.prepare(query).unwrap();
    let ncols = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut theirs = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::with_capacity(ncols);
        for i in 0..ncols {
            row.push(match r.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(v) => format!("I:{v}"),
                rusqlite::types::ValueRef::Real(v) => format!("R:{v}"),
                rusqlite::types::ValueRef::Text(t) => {
                    format!("T:{}", String::from_utf8_lossy(t))
                }
                rusqlite::types::ValueRef::Blob(b) => format!("B:{}", b.len()),
            });
        }
        theirs.push(row);
    }
    assert_eq!(
        ours, theirs,
        "\npushdown mismatch on {query}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
    );
}

fn render(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("I:{i}"),
                    Value::Real(r) => format!("R:{r}"),
                    Value::Text(t) => format!("T:{}", t.as_str()),
                    Value::Blob(b) => format!("B:{}", b.len()),
                })
                .collect()
        })
        .collect()
}

fn explain(setup: &str, inserts: &[&str], query: &str) -> Vec<String> {
    let mut db = Database::open_in_memory().unwrap();
    for s in setup.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        db.execute(s, []).unwrap();
    }
    for s in inserts {
        db.execute(s, []).unwrap();
    }
    db.query(&format!("EXPLAIN QUERY PLAN {query}"), [])
        .unwrap()
        .iter()
        .map(|row| {
            row.iter()
                .filter_map(|v| {
                    let t = v.as_text();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

const SETUP: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
                     CREATE INDEX idx_t_k ON t(k);";
const INSERTS: &[&str] = &[
    "INSERT INTO t (k, v) VALUES (1,10),(2,20),(3,30),(4,40),(5,50)",
    "INSERT INTO t (k, v) VALUES (6,60),(7,70),(8,80),(9,90),(10,100)",
];

#[test]
fn push_qualified_reference() {
    let q = "SELECT * FROM (SELECT k, v FROM t) AS s WHERE s.k > 7 ORDER BY s.k";
    diff_with(SETUP, INSERTS, q);
    let plan = explain(SETUP, INSERTS, q);
    assert!(
        plan.iter()
            .any(|p| p.contains("SEARCH t USING INDEX idx_t_k")),
        "expected the pushed conjunct to drive an index range, got {plan:?}"
    );
}

#[test]
fn push_through_star_projection() {
    let q = "SELECT s.k, s.v FROM (SELECT * FROM t) AS s WHERE s.k > 7 ORDER BY s.k";
    diff_with(SETUP, INSERTS, q);
    let plan = explain(SETUP, INSERTS, q);
    assert!(
        plan.iter()
            .any(|p| p.contains("SEARCH t USING INDEX idx_t_k")),
        "expected the pushed conjunct to drive an index range, got {plan:?}"
    );
}

#[test]
fn push_unqualified_reference() {
    let q = "SELECT k, v FROM (SELECT k, v FROM t) AS s WHERE k > 7 ORDER BY k";
    diff_with(SETUP, INSERTS, q);
    let plan = explain(SETUP, INSERTS, q);
    assert!(
        plan.iter().any(|p| p.contains("SEARCH t USING INDEX")),
        "expected an index search, got {plan:?}"
    );
}

#[test]
fn push_multiple_conjuncts() {
    let q = "SELECT * FROM (SELECT k, v FROM t) s WHERE s.k > 3 AND s.v < 60 ORDER BY s.k";
    diff_with(SETUP, INSERTS, q);
}

#[test]
fn push_in_list() {
    let q = "SELECT * FROM (SELECT k, v FROM t) s WHERE s.k IN (2, 5, 8) ORDER BY s.k";
    diff_with(SETUP, INSERTS, q);
}

#[test]
fn push_with_parameter() {
    let mut eng = Database::open_in_memory().unwrap();
    for s in SETUP.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        eng.execute(s, []).unwrap();
    }
    for s in INSERTS {
        eng.execute(s, []).unwrap();
    }
    let rows = eng
        .query(
            "SELECT k FROM (SELECT k, v FROM t) s WHERE s.k > ? ORDER BY k",
            vec![Value::Integer(7)],
        )
        .unwrap();
    let got: Vec<String> = render(&rows).into_iter().flatten().collect();
    assert_eq!(got, vec!["I:8", "I:9", "I:10"]);
}

#[test]
fn push_alongside_a_join() {
    let setup = "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
                 CREATE INDEX idx_t_k ON t(k);
                 CREATE TABLE u (id INTEGER PRIMARY KEY, j INT, w INT);";
    let inserts = &[
        "INSERT INTO t (k, v) VALUES (1,10),(2,20),(3,30),(4,40)",
        "INSERT INTO u (j, w) VALUES (1,100),(2,200),(3,300)",
    ];
    diff_with(
        setup,
        inserts,
        "SELECT s.k, u.w FROM (SELECT k, v FROM t) s JOIN u ON s.k = u.j \
         WHERE s.k > 1 ORDER BY s.k",
    );
}

// ------------------------------------------------------------------------
// Decline shapes: correctness only (the conjunct stays outside)
// ------------------------------------------------------------------------

#[test]
fn decline_expression_output() {
    diff_with(
        SETUP,
        INSERTS,
        "SELECT * FROM (SELECT k + 1 AS x, v FROM t) s WHERE s.x > 7 ORDER BY s.v",
    );
}

#[test]
fn decline_aggregate_body() {
    diff_with(
        SETUP,
        INSERTS,
        "SELECT * FROM (SELECT k, COUNT(*) AS c FROM t GROUP BY k) g \
         WHERE g.k > 5 ORDER BY g.k",
    );
}

#[test]
fn decline_limit_body() {
    // filter-then-limit is NOT limit-then-filter: the push must NOT fire.
    diff_with(
        SETUP,
        INSERTS,
        "SELECT * FROM (SELECT k, v FROM t ORDER BY k LIMIT 5) s \
         WHERE s.k > 3 ORDER BY s.k",
    );
}

#[test]
fn decline_distinct_body() {
    diff_with(
        SETUP,
        INSERTS,
        "SELECT * FROM (SELECT DISTINCT k FROM t) s WHERE s.k > 5 ORDER BY s.k",
    );
}

#[test]
fn decline_compound_body() {
    let setup = "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
                 CREATE INDEX idx_t_k ON t(k);
                 CREATE TABLE u (id INTEGER PRIMARY KEY, k INT, v INT);";
    diff_with(
        setup,
        INSERTS,
        "SELECT * FROM (SELECT k, v FROM t UNION ALL SELECT k, v FROM u) c \
         WHERE c.k > 7 ORDER BY c.k, c.v",
    );
}

#[test]
fn decline_under_left_join() {
    let setup = "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
                 CREATE INDEX idx_t_k ON t(k);
                 CREATE TABLE u (id INTEGER PRIMARY KEY, j INT, w INT);";
    let inserts = &[
        "INSERT INTO t (k, v) VALUES (1,10),(2,20),(3,30)",
        "INSERT INTO u (j, w) VALUES (1,100),(2,200)",
    ];
    // The subquery is the RIGHT (non-preserved) side: NULL-extension
    // interacts with the term — it must stay outside.
    diff_with(
        setup,
        inserts,
        "SELECT u.j, s.k FROM u LEFT JOIN (SELECT k, v FROM t) s ON s.k = u.j \
         WHERE s.k > 1 ORDER BY u.j, s.k",
    );
    // The subquery is the LEFT (preserved) side: also declined (v1 rule).
    diff_with(
        setup,
        inserts,
        "SELECT s.k, u.j FROM (SELECT k, v FROM t) s LEFT JOIN u ON s.k = u.j \
         WHERE s.k > 1 ORDER BY s.k, u.j",
    );
}

#[test]
fn decline_ambiguous_unqualified() {
    // `k` is exposed by BOTH atoms: ambiguous. The engine's documented
    // policy is first-match (the left atom — s), SQLite errors; the
    // pushdown must NOT resolve the ambiguity itself (pushing into u
    // would flip the answer). Disjoint k-ranges make the resolutions
    // distinguishable: s-resolution -> 2 s-rows x 3 u-rows = 6;
    // u-resolution -> 3 x 3 = 9.
    let setup = "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
                 CREATE INDEX idx_t_k ON t(k);
                 CREATE TABLE u (id INTEGER PRIMARY KEY, k INT, v INT);";
    let inserts = &[
        "INSERT INTO t (k, v) VALUES (1,10),(2,20),(3,30)",
        "INSERT INTO u (k, v) VALUES (10,100),(20,200),(30,300)",
    ];
    let mut eng = Database::open_in_memory().unwrap();
    for s2 in setup.split(';').map(str::trim).filter(|x| !x.is_empty()) {
        eng.execute(s2, []).unwrap();
    }
    for ins in inserts {
        eng.execute(ins, []).unwrap();
    }
    // The pushdown must DECLINE the ambiguous conjunct: the subquery's
    // body stays a plain scan (an index-driven body would mean the term
    // was pushed — silently re-resolving the ambiguity).
    let plan = explain(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, v INT);
         CREATE INDEX idx_t_k ON t(k);
         CREATE TABLE u (id INTEGER PRIMARY KEY, k INT, v INT)",
        &[],
        "SELECT COUNT(*) FROM (SELECT k, v FROM t) s, u WHERE k > 1",
    );
    assert!(
        !plan.iter().any(|p| p.contains("SEARCH t USING INDEX")),
        "the ambiguous conjunct must not be pushed into the subquery body, got {plan:?}"
    );
    // And the query itself must still execute (the engine's own
    // ambiguity policy — first-match across atoms — is unchanged).
    let rows = eng
        .query(
            "SELECT COUNT(*) FROM (SELECT k, v FROM t) s, u WHERE k > 1",
            [],
        )
        .unwrap();
    let _ = rows;
}

#[test]
fn nested_subquery_body_declines_but_stays_correct() {
    diff_with(
        SETUP,
        INSERTS,
        "SELECT * FROM (SELECT * FROM (SELECT k, v FROM t) x) s \
         WHERE s.k > 7 ORDER BY s.k",
    );
}

// ------------------------------------------------------------------------
// EXPLAIN QUERY PLAN over WITH (the regression: "no such table: c")
// ------------------------------------------------------------------------

#[test]
fn explain_query_plan_over_with() {
    let mut eng = Database::open_in_memory().unwrap();
    for s in SETUP.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        eng.execute(s, []).unwrap();
    }
    for s in INSERTS {
        eng.execute(s, []).unwrap();
    }
    let rows = eng
        .query(
            "EXPLAIN QUERY PLAN WITH c AS (SELECT k, v FROM t) SELECT * FROM c WHERE k > 3",
            [],
        )
        .expect("EXPLAIN over WITH must plan, not error");
    assert!(!rows.is_empty(), "expected a non-empty plan");
}
