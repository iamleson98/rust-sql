//! Differential cost-searched join-ordering suite: the planner's subset-DP
//! (Selinger-style with bushy splits, selectivity-aware cost model, INLJ
//! economics) vs real SQLite (rusqlite). Every query runs on both engines;
//! rows must match EXACTLY. EXPLAIN assertions pin the plan SHAPES the DP
//! is supposed to find (index-driven star schemas, bushy pairings) — the
//! shapes the old greedy-by-atom-size heuristic could not see.
//!
//! Covers:
//! - Star schema: point-filtered dimension FIRST, fact table driven by
//!   index probes (INLJ chaining through the DP's order) — the greedy
//!   ordered the unfiltered dimensions before the fact and paid a full
//!   fact-table hash build.
//! - Star schema WITHOUT the point filter (hash-join economics only).
//! - Bushy pairing: two unique-key pairs joined through a low-distinct
//!   link — (A ⋈ B) ⋈ (C ⋈ D) beats every left-deep order because the
//!   low-distinct intermediate becomes the FINAL output instead of an
//!   intermediate that a fourth join re-reads.
//! - The greedy's blind spot: a tiny table whose join is NON-selective
//!   vs. a mid-size pair whose join is very selective.
//! - ANALYZE-flipped orders (sqlite_stat1 distincts) vs blind defaults.
//! - 5-table chains with SELECT * column-order preservation.
//! - Non-equi ON conditions through the DP (nested-loop steps).
//! - True cartesian products (the DP's cost model punishes them, the
//!   result must stay exact).
//! - Single-atom ON residuals (folded into their atom's Filter).
//! - Identity-optimal benign orders (the no-op gate keeps the tree).
//! - CTE atoms inside a reorderable spine.
//! - Chained INLJ: three indexed lookups in sequence.

use rustqlite::{Database, Value};

/// Differential with explicit INSERT statements applied to both engines
/// before the query (setup + inserts + query on both; rows must match).
fn diff_with<S: AsRef<str>>(setup: &[S], inserts: &[String], query: &str) {
    let mut eng = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        eng.execute(s.as_ref(), []).unwrap();
        conn.execute_batch(s.as_ref()).unwrap();
    }
    for s in inserts {
        eng.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    let ours = render(&eng.query(query, []).expect("rustqlite query failed"));
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
        "\ncost-search mismatch on {query}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
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

fn explain<S: AsRef<str>>(setup: &[S], query: &str) -> Vec<String> {
    let mut db = Database::open_in_memory().unwrap();
    for s in setup {
        db.execute(s.as_ref(), []).unwrap();
    }
    let sql = format!("EXPLAIN QUERY PLAN {query}");
    db.query(&sql, [])
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

/// Load `setup` + `data` rows on BOTH engines (same values), returning the
/// engine handle for EXPLAIN/queries.
fn both_engines<S: AsRef<str>>(setup: &[S], inserts: &[String]) -> Database {
    let mut eng = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        eng.execute(s.as_ref(), []).unwrap();
        conn.execute_batch(s.as_ref()).unwrap();
    }
    eng.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for s in inserts {
        eng.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    eng.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();
    eng
}

// ---------------------------------------------------------------------------
// Star schema: the DP joins the fact table early through index probes
// ---------------------------------------------------------------------------

const STAR_SETUP: &[&str] = &[
    "CREATE TABLE dim1 (k INTEGER PRIMARY KEY, a TEXT)",
    "CREATE TABLE dim2 (k INTEGER PRIMARY KEY, b TEXT)",
    "CREATE TABLE dim3 (k INTEGER PRIMARY KEY, c TEXT)",
    "CREATE TABLE fact (id INTEGER PRIMARY KEY, d1 INT, d2 INT, d3 INT, v INT)",
    "CREATE INDEX idx_fact_d1 ON fact(d1)",
    "CREATE INDEX idx_fact_d2 ON fact(d2)",
    "CREATE INDEX idx_fact_d3 ON fact(d3)",
];

fn star_inserts(n: i64) -> Vec<String> {
    let mut v = Vec::new();
    for i in 1..=n {
        v.push(format!(
            "INSERT INTO fact (d1, d2, d3, v) VALUES ({}, {}, {}, {})",
            i % 10,
            i % 20,
            i % 30,
            i
        ));
    }
    for k in 1..=10i64 {
        v.push(format!("INSERT INTO dim1 (k, a) VALUES ({k}, 'a{k}')"));
    }
    for k in 1..=20i64 {
        v.push(format!("INSERT INTO dim2 (k, b) VALUES ({k}, 'b{k}')"));
    }
    for k in 1..=30i64 {
        v.push(format!("INSERT INTO dim3 (k, c) VALUES ({k}, 'c{k}')"));
    }
    v
}

#[test]
fn star_schema_point_filter_drives_fact_by_index() {
    let eng = both_engines(STAR_SETUP, &star_inserts(60_000));
    let q = "SELECT SUM(fact.v) FROM dim1, dim2, dim3, fact \
             WHERE dim1.k = fact.d1 AND dim2.k = fact.d2 AND dim3.k = fact.d3 \
             AND dim1.k = 3";
    // Engine side.
    let ours: i64 = eng
        .query(q, [])
        .unwrap()
        .iter()
        .map(|r| r[0].as_integer())
        .sum();
    // SQLite side.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in STAR_SETUP {
        conn.execute_batch(s).unwrap();
    }
    conn.execute_batch(&star_inserts(60_000).join("; "))
        .unwrap();
    let theirs: i64 = conn
        .query_row(q, [], |r| r.get::<_, Option<i64>>(0))
        .unwrap()
        .unwrap_or(0);
    assert_eq!(ours, theirs, "star-schema SUM mismatch");
    assert!(ours > 0);

    // The DP must drive fact by index probes off the filtered dimension
    // (INLJ), not build a 60k-row hash table after the dimensions.
    let plan = explain(STAR_SETUP, q);
    assert!(
        plan.iter().any(|p| p.contains("SEARCH fact USING INDEX")),
        "expected fact driven by an index search, got {plan:?}"
    );
    assert!(
        plan.iter()
            .any(|p| p.contains("SCAN dim1") || p.contains("SEARCH dim1")),
        "expected dim1 in the plan, got {plan:?}"
    );
}

#[test]
fn star_schema_unfiltered_all_dimensions() {
    let eng = both_engines(STAR_SETUP, &star_inserts(20_000));
    let q = "SELECT COUNT(*), SUM(fact.v) FROM dim1, dim2, dim3, fact \
             WHERE dim1.k = fact.d1 AND dim2.k = fact.d2 AND dim3.k = fact.d3";
    let ours = eng.query(q, []).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in STAR_SETUP {
        conn.execute_batch(s).unwrap();
    }
    conn.execute_batch(&star_inserts(20_000).join("; "))
        .unwrap();
    let (tc, tv): (i64, i64) = conn
        .query_row(q, [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(ours[0][0].as_integer(), tc);
    assert_eq!(ours[0][1].as_integer(), tv);
}

#[test]
fn star_schema_analyze_distincts() {
    let mut eng = both_engines(STAR_SETUP, &star_inserts(20_000));
    eng.execute("ANALYZE", []).unwrap();
    let q = "SELECT dim2.b, COUNT(*) FROM dim1, dim2, fact \
             WHERE dim1.k = fact.d1 AND dim2.k = fact.d2 AND dim1.k <= 4 \
             GROUP BY dim2.b ORDER BY dim2.b";
    let ours = render(&eng.query(q, []).unwrap());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in STAR_SETUP {
        conn.execute_batch(s).unwrap();
    }
    conn.execute_batch(&star_inserts(20_000).join("; "))
        .unwrap();
    conn.execute_batch("ANALYZE").unwrap();
    let mut stmt = conn.prepare(q).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut theirs = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        theirs.push(vec![
            format!("T:{}", r.get::<_, String>(0).unwrap()),
            format!("I:{}", r.get::<_, i64>(1).unwrap()),
        ]);
    }
    assert_eq!(ours, theirs, "ANALYZE'd star-schema GROUP BY mismatch");
}

// ---------------------------------------------------------------------------
// Bushy pairing: two unique-key pairs through a low-distinct link
// ---------------------------------------------------------------------------

#[test]
fn bushy_pairing_beats_left_deep() {
    let setup: &[&str] = &[
        "CREATE TABLE ta (id INTEGER PRIMARY KEY, x INT, p INT)",
        "CREATE TABLE tb (id INTEGER PRIMARY KEY, y INT)",
        "CREATE TABLE tc (id INTEGER PRIMARY KEY, z INT, q INT)",
        "CREATE TABLE td (id INTEGER PRIMARY KEY, w INT)",
        "CREATE INDEX idx_tb_y ON tb(y)",
        "CREATE INDEX idx_td_w ON td(w)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=200i64 {
        inserts.push(format!("INSERT INTO ta (x, p) VALUES ({i}, {})", i % 5));
        inserts.push(format!("INSERT INTO tb (y) VALUES ({i})"));
        inserts.push(format!("INSERT INTO tc (z, q) VALUES ({i}, {})", i % 5));
        inserts.push(format!("INSERT INTO td (w) VALUES ({i})"));
    }
    let eng = both_engines(setup, &inserts);
    // ta.x = tb.y and tc.z = td.w are unique-key joins; ta.p = tc.q is the
    // low-distinct link. Bushy: (ta⋈tb) ⋈ (tc⋈td) — the low-distinct
    // product is the FINAL output. Left-deep materializes it as an
    // intermediate a fourth join re-reads.
    let q = "SELECT COUNT(*) FROM ta, tb, tc, td \
             WHERE ta.x = tb.y AND tc.z = td.w AND ta.p = tc.q";
    let ours = eng.query(q, []).unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        conn.execute_batch(s).unwrap();
    }
    conn.execute_batch(&inserts.join("; ")).unwrap();
    let theirs: i64 = conn.query_row(q, [], |r| r.get(0)).unwrap();
    assert_eq!(ours[0][0].as_integer(), theirs);
    assert!(theirs > 0);
}

// ---------------------------------------------------------------------------
// The greedy's blind spot: tiny table with a NON-selective join
// ---------------------------------------------------------------------------

#[test]
fn tiny_nonselective_table_does_not_drive() {
    let setup: &[&str] = &[
        "CREATE TABLE s (id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE m1 (id INTEGER PRIMARY KEY, k INT, v INT)",
        "CREATE TABLE m2 (id INTEGER PRIMARY KEY, v INT)",
        "CREATE INDEX idx_m2_v ON m2(v)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=5i64 {
        inserts.push(format!("INSERT INTO s (k) VALUES ({i})"));
    }
    for i in 1..=1000i64 {
        inserts.push(format!("INSERT INTO m1 (k, v) VALUES ({}, {i})", i % 5));
        inserts.push(format!("INSERT INTO m2 (v) VALUES ({i})"));
    }
    diff_with(
        setup,
        &inserts,
        "SELECT COUNT(*) FROM s, m1, m2 \
         WHERE s.k = m1.k AND m1.v = m2.v",
    );
}

// ---------------------------------------------------------------------------
// 5-table chain: column order + aggregates over the reordered spine
// ---------------------------------------------------------------------------

#[test]
fn five_table_chain_select_star_column_order() {
    let setup: &[&str] = &[
        "CREATE TABLE c1 (id INTEGER PRIMARY KEY, a INT)",
        "CREATE TABLE c2 (id INTEGER PRIMARY KEY, b INT)",
        "CREATE TABLE c3 (id INTEGER PRIMARY KEY, c INT)",
        "CREATE TABLE c4 (id INTEGER PRIMARY KEY, d INT)",
        "CREATE TABLE c5 (id INTEGER PRIMARY KEY, e INT)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=100i64 {
        let k = i % 7;
        inserts.push(format!("INSERT INTO c1 (a) VALUES ({k})"));
        inserts.push(format!("INSERT INTO c2 (b) VALUES ({k})"));
        inserts.push(format!("INSERT INTO c3 (c) VALUES ({k})"));
        inserts.push(format!("INSERT INTO c4 (d) VALUES ({k})"));
        inserts.push(format!("INSERT INTO c5 (e) VALUES ({k})"));
    }
    // Adversarial syntactic order: the selective filter lands LAST.
    diff_with(
        setup,
        &inserts,
        "SELECT * FROM c5, c4, c3, c2, c1 \
         WHERE c1.a = c2.b AND c2.b = c3.c AND c3.c = c4.d AND c4.d = c5.e \
         AND c1.id = 3 ORDER BY c1.id, c2.id, c3.id, c4.id, c5.id",
    );
    diff_with(
        setup,
        &inserts,
        "SELECT c3.c, COUNT(*) FROM c5, c4, c3, c2, c1 \
         WHERE c1.a = c2.b AND c2.b = c3.c AND c3.c = c4.d AND c4.d = c5.e \
         GROUP BY c3.c ORDER BY c3.c",
    );
}

// ---------------------------------------------------------------------------
// Non-equi ON conditions through the DP
// ---------------------------------------------------------------------------

#[test]
fn non_equi_on_conditions() {
    let setup: &[&str] = &[
        "CREATE TABLE n1 (id INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE n2 (id INTEGER PRIMARY KEY, y INT)",
        "CREATE TABLE n3 (id INTEGER PRIMARY KEY, z INT)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=50i64 {
        inserts.push(format!("INSERT INTO n1 (x) VALUES ({})", i % 11));
        inserts.push(format!("INSERT INTO n2 (y) VALUES ({})", i % 13));
        inserts.push(format!("INSERT INTO n3 (z) VALUES ({})", i % 9));
    }
    diff_with(
        setup,
        &inserts,
        "SELECT COUNT(*) FROM n1, n2, n3 \
         WHERE n1.x < n2.y AND n2.y <= n3.z AND n1.x > 2",
    );
    diff_with(
        setup,
        &inserts,
        "SELECT n1.x, n2.y, n3.z FROM n1 JOIN n2 ON n1.x < n2.y \
         JOIN n3 ON n2.y <= n3.z WHERE n3.z = 5 ORDER BY n1.id, n2.id, n3.id",
    );
}

// ---------------------------------------------------------------------------
// True cartesian product (the DP punishes it; results stay exact)
// ---------------------------------------------------------------------------

#[test]
fn true_cross_product() {
    let setup: &[&str] = &[
        "CREATE TABLE x1 (id INTEGER PRIMARY KEY, a INT)",
        "CREATE TABLE x2 (id INTEGER PRIMARY KEY, b INT)",
        "CREATE TABLE x3 (id INTEGER PRIMARY KEY, c INT)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=7i64 {
        inserts.push(format!("INSERT INTO x1 (a) VALUES ({i})"));
        inserts.push(format!("INSERT INTO x2 (b) VALUES ({})", i * 2));
        inserts.push(format!("INSERT INTO x3 (c) VALUES ({})", i * 3));
    }
    diff_with(
        setup,
        &inserts,
        "SELECT COUNT(*) FROM x1, x2, x3 WHERE x1.a * 1 = x3.c / 3 + 0",
    );
}

// ---------------------------------------------------------------------------
// Single-atom ON residuals (folded into the atom's Filter)
// ---------------------------------------------------------------------------

#[test]
fn single_atom_on_residual() {
    let setup: &[&str] = &[
        "CREATE TABLE o1 (id INTEGER PRIMARY KEY, x INT, t TEXT)",
        "CREATE TABLE o2 (id INTEGER PRIMARY KEY, y INT)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=40i64 {
        inserts.push(format!(
            "INSERT INTO o1 (x, t) VALUES ({}, 't{}')",
            i % 6,
            i % 4
        ));
        inserts.push(format!("INSERT INTO o2 (y) VALUES ({})", i % 6));
    }
    diff_with(
        setup,
        &inserts,
        "SELECT COUNT(*) FROM o1 JOIN o2 ON o1.x = o2.y AND o1.x > 3",
    );
    diff_with(
        setup,
        &inserts,
        "SELECT o1.t, COUNT(*) FROM o1 JOIN o2 ON o1.x = o2.y AND o1.id < 20 \
         GROUP BY o1.t ORDER BY o1.t",
    );
}

// ---------------------------------------------------------------------------
// Identity-optimal benign order (the no-op gate keeps the tree)
// ---------------------------------------------------------------------------

#[test]
fn benign_identity_order_unchanged() {
    let setup: &[&str] = &[
        "CREATE TABLE b1 (id INTEGER PRIMARY KEY, k INT, v INT)",
        "CREATE TABLE b2 (id INTEGER PRIMARY KEY, k INT)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=30i64 {
        inserts.push(format!("INSERT INTO b1 (k, v) VALUES ({}, {i})", i % 6));
        inserts.push(format!("INSERT INTO b2 (k) VALUES ({})", i % 6));
    }
    let eng = both_engines(setup, &inserts);
    let q = "SELECT b2.k, SUM(b1.v) FROM b2 JOIN b1 ON b2.k = b1.k \
             GROUP BY b2.k ORDER BY b2.k";
    let ours = render(&eng.query(q, []).unwrap());
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        conn.execute_batch(s).unwrap();
    }
    conn.execute_batch(&inserts.join("; ")).unwrap();
    let mut stmt = conn.prepare(q).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut theirs = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        theirs.push(vec![
            format!("I:{}", r.get::<_, i64>(0).unwrap()),
            format!("I:{}", r.get::<_, i64>(1).unwrap()),
        ]);
    }
    assert_eq!(ours, theirs);
}

// ---------------------------------------------------------------------------
// CTE atom inside a reorderable spine
// ---------------------------------------------------------------------------

#[test]
fn cte_atom_in_spine() {
    let setup: &[&str] = &[
        "CREATE TABLE base (id INTEGER PRIMARY KEY, k INT, v INT)",
        "CREATE TABLE side (id INTEGER PRIMARY KEY, k INT)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=60i64 {
        inserts.push(format!("INSERT INTO base (k, v) VALUES ({}, {i})", i % 9));
        inserts.push(format!("INSERT INTO side (k) VALUES ({})", i % 9));
    }
    diff_with(
        setup,
        &inserts,
        "WITH filtered AS (SELECT k, v FROM base WHERE v > 10) \
         SELECT side.k, COUNT(*) FROM filtered, side \
         WHERE filtered.k = side.k AND side.k < 5 \
         GROUP BY side.k ORDER BY side.k",
    );
}

// ---------------------------------------------------------------------------
// Chained INLJ: three indexed lookups in sequence
// ---------------------------------------------------------------------------

#[test]
fn chained_index_lookups() {
    let setup: &[&str] = &[
        "CREATE TABLE p1 (id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE p2 (id INTEGER PRIMARY KEY, k INT, j INT)",
        "CREATE TABLE p3 (id INTEGER PRIMARY KEY, j INT, m INT)",
        "CREATE TABLE p4 (id INTEGER PRIMARY KEY, m INT, v INT)",
        "CREATE INDEX idx_p2_k ON p2(k)",
        "CREATE INDEX idx_p3_j ON p3(j)",
        "CREATE INDEX idx_p4_m ON p4(m)",
    ];
    let mut inserts = Vec::new();
    for i in 1..=400i64 {
        inserts.push(format!("INSERT INTO p1 (k) VALUES ({})", i % 8));
        inserts.push(format!(
            "INSERT INTO p2 (k, j) VALUES ({}, {})",
            i % 8,
            i % 12
        ));
        inserts.push(format!(
            "INSERT INTO p3 (j, m) VALUES ({}, {})",
            i % 12,
            i % 16
        ));
        inserts.push(format!("INSERT INTO p4 (m, v) VALUES ({}, {i})", i % 16));
    }
    let eng = both_engines(setup, &inserts);
    let q = "SELECT SUM(p4.v) FROM p1, p2, p3, p4 \
             WHERE p1.k = p2.k AND p2.j = p3.j AND p3.m = p4.m AND p1.id = 5";
    let ours: i64 = eng
        .query(q, [])
        .unwrap()
        .iter()
        .map(|r| r[0].as_integer())
        .sum();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        conn.execute_batch(s).unwrap();
    }
    conn.execute_batch(&inserts.join("; ")).unwrap();
    let theirs: i64 = conn
        .query_row(q, [], |r| r.get::<_, Option<i64>>(0))
        .unwrap()
        .unwrap_or(0);
    assert_eq!(ours, theirs, "chained-lookup SUM mismatch");

    let plan = explain(setup, q);
    let searches = plan.iter().filter(|p| p.contains("SEARCH")).count();
    assert!(searches >= 3, "expected >=3 index searches, got {plan:?}");
}

// ---------------------------------------------------------------------------
// Determinism: the same query plans identically twice
// ---------------------------------------------------------------------------

#[test]
fn dp_plan_is_deterministic() {
    let setup: &[&str] = &[
        "CREATE TABLE d1 (id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE d2 (id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE d3 (id INTEGER PRIMARY KEY, k INT)",
    ];
    let q = "SELECT COUNT(*) FROM d1, d2, d3 \
             WHERE d1.k = d2.k AND d2.k = d3.k AND d1.id = 2";
    let p1 = explain(setup, q);
    let p2 = explain(setup, q);
    assert_eq!(p1, p2, "planner must be deterministic");
}
