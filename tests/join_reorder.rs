//! Differential join-reordering suite: the planner's INNER-join spine
//! reordering (greedy, cardinality-driven) vs real SQLite (rusqlite).
//! Every query runs on both engines; rows must match EXACTLY (values,
//! order where deterministic, and column order for SELECT *).
//!
//! Covers:
//! - 3- and 4-table inner-join chains in ADVERSARIAL syntactic order
//!   (the small filtered table written LAST — the case that motivated
//!   the reorder: the syntactic tree computes the big×big intermediate
//!   first).
//! - Implicit-join syntax (`FROM a, b WHERE a.x = b.x`) — the WHERE
//!   conjuncts fuse into join conditions (hash join) instead of a
//!   cross product + post-filter.
//! - Column ORDER preservation after reordering (SELECT *).
//! - USING / multi-key equi (AND-chain) / mixed equi+residual ON.
//! - LEFT JOIN boundaries: the pinned subtree's inner sub-spines
//!   reorder, the outer join itself never re-associates.
//! - Subquery atoms (reorder declines — but results stay exact).
//! - RowidLookup-driven reordering (`WHERE big.id = ?`).
//! - Aggregates and ORDER BY over reordered spines.
//! - Many-to-many multiplicity preservation.
//! - ANALYZE-driven estimates (sqlite_stat1) vs the blind defaults.

use rustqlite::{Database, Value};

/// Run `setup` + `query` on both engines; compare rendered rows exactly.
fn diff<S: AsRef<str>>(setup: &[S], query: &str) {
    let ours = engine_rows(setup, query);
    let theirs = sqlite_rows(setup, query);
    assert_eq!(
        ours, theirs,
        "\njoin-reorder mismatch on {query}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
    );
}

fn engine_rows<S: AsRef<str>>(setup: &[S], query: &str) -> Vec<Vec<String>> {
    let mut db = Database::open_in_memory().unwrap();
    for s in setup {
        db.execute(s.as_ref(), []).unwrap();
    }
    render(&db.query(query, []).expect("rustqlite query failed"))
}

fn sqlite_rows<S: AsRef<str>>(setup: &[S], query: &str) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        conn.execute_batch(s.as_ref()).unwrap();
    }
    let mut stmt = conn.prepare(query).unwrap();
    let ncols = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
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
        out.push(row);
    }
    out
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

#[test]
fn reorder_three_table_adversarial_order() {
    // small is written LAST; its point filter makes it the driving table.
    // The syntactic plan hashes big1×big2 (up to 1600 rows) before small's
    // 5 rows filter anything; the reorder drives small→big1→big2.
    let setup: Vec<String> = vec![
        "CREATE TABLE big1 (id INTEGER PRIMARY KEY, k INTEGER, v TEXT)".into(),
        "CREATE TABLE big2 (id INTEGER PRIMARY KEY, k INTEGER, w TEXT)".into(),
        "CREATE TABLE small (id INTEGER PRIMARY KEY, k INTEGER, s TEXT)".into(),
        "CREATE INDEX i_b1 ON big1(k)".into(),
        "CREATE INDEX i_b2 ON big2(k)".into(),
    ];
    let mut eng = Database::open_in_memory().unwrap();
    for s in &setup {
        eng.execute(s, []).unwrap();
    }
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in &setup {
        conn.execute_batch(s).unwrap();
    }
    eng.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=400i64 {
        let (a, b) = (format!("b1{}", i % 7), format!("b2{}", i % 9));
        let (a2, b2) = (a.clone(), b.clone());
        eng.execute(
            "INSERT INTO big1 (k, v) VALUES (?, ?)",
            [Value::Integer(i % 40), Value::Text(a.into())],
        )
        .unwrap();
        eng.execute(
            "INSERT INTO big2 (k, w) VALUES (?, ?)",
            [Value::Integer(i % 40), Value::Text(b.into())],
        )
        .unwrap();
        if i <= 5 {
            eng.execute(
                "INSERT INTO small (k, s) VALUES (?, ?)",
                [Value::Integer(i), Value::Text(format!("s{i}").into())],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO small (k, s) VALUES (?, ?)",
                (i, format!("s{i}")),
            )
            .unwrap();
        }
        conn.execute("INSERT INTO big1 (k, v) VALUES (?, ?)", (i % 40, a2))
            .unwrap();
        conn.execute("INSERT INTO big2 (k, w) VALUES (?, ?)", (i % 40, b2))
            .unwrap();
    }
    eng.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();

    // Point-filtered small table drives the join.
    let q = "SELECT small.s, big1.v, big2.w FROM big1 JOIN big2 ON big1.k = big2.k \
             JOIN small ON small.k = big1.k WHERE small.id = 3";
    let ours = render(&eng.query(q, []).unwrap());
    let mut stmt = conn.prepare(q).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut theirs: Vec<Vec<String>> = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::new();
        for i in 0..3 {
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
    let mut ours_sorted = ours.clone();
    ours_sorted.sort();
    let mut theirs_sorted = theirs.clone();
    theirs_sorted.sort();
    assert_eq!(
        ours_sorted, theirs_sorted,
        "3-table adversarial join mismatch"
    );

    // Plan-shape: small's rowid lookup must come FIRST (the reorder drove
    // the smallest, point-filtered table).
    let plan = explain(&setup, q);
    assert!(
        plan[0].contains("SEARCH small"),
        "expected small to drive the plan, got {plan:?}"
    );
}

/// Rendered EXPLAIN QUERY PLAN detail strings (the planner's own view of
/// the plan — used to pin that the reorder actually happened).
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

#[test]
fn reorder_four_table_chain_mixed_filters() {
    let setup: &[&str] = &[
        "CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER, ta TEXT)",
        "CREATE TABLE b (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, tb TEXT)",
        "CREATE TABLE c (id INTEGER PRIMARY KEY, y INTEGER, z INTEGER, tc TEXT)",
        "CREATE TABLE d (id INTEGER PRIMARY KEY, z INTEGER, td TEXT)",
        "CREATE INDEX ib ON b(x)",
        "CREATE INDEX ic ON c(y)",
        "CREATE INDEX idz ON d(z)",
    ];
    let mut eng = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        eng.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    eng.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=200i64 {
        for (tbl, x, y, z, t) in [
            ("a", i % 13, 0, 0, format!("a{}", i % 5)),
            ("b", i % 13, i % 11, 0, format!("b{}", i % 5)),
            ("c", 0, i % 11, i % 9, format!("c{}", i % 5)),
            ("d", 0, 0, i % 9, format!("d{}", i % 5)),
        ] {
            let sql = if tbl == "a" {
                format!("INSERT INTO a (x, ta) VALUES ({x}, '{t}')")
            } else if tbl == "b" {
                format!("INSERT INTO b (x, y, tb) VALUES ({x}, {y}, '{t}')")
            } else if tbl == "c" {
                format!("INSERT INTO c (y, z, tc) VALUES ({y}, {z}, '{t}')")
            } else {
                format!("INSERT INTO d (z, td) VALUES ({z}, '{t}')")
            };
            eng.execute(&sql, []).unwrap();
            conn.execute_batch(&sql).unwrap();
        }
    }
    eng.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();

    let queries = [
        "SELECT a.ta, b.tb, c.tc, d.td FROM a JOIN b ON a.x = b.x \
         JOIN c ON b.y = c.y JOIN d ON c.z = d.z",
        "SELECT COUNT(*), SUM(a.x) FROM a JOIN b ON a.x = b.x \
         JOIN c ON b.y = c.y JOIN d ON c.z = d.z WHERE d.id = 7",
        "SELECT c.tc, COUNT(*) FROM a JOIN b ON a.x = b.x \
         JOIN c ON b.y = c.y JOIN d ON c.z = d.z WHERE c.z > 4 GROUP BY c.tc ORDER BY 2, 1",
        "SELECT d.td FROM a, b, c, d WHERE a.x = b.x AND b.y = c.y AND c.z = d.z \
         AND b.id BETWEEN 10 AND 20 ORDER BY d.td",
    ];
    for q in queries {
        let ours = render(&eng.query(q, []).unwrap());
        let theirs: Vec<Vec<String>> = {
            let mut stmt = conn.prepare(q).unwrap();
            let ncols = stmt.column_count();
            let mut rows = stmt.query([]).unwrap();
            let mut out = Vec::new();
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
                out.push(row);
            }
            out
        };
        let mut ours_sorted = ours.clone();
        ours_sorted.sort();
        let mut theirs_sorted = theirs.clone();
        theirs_sorted.sort();
        assert_eq!(ours_sorted, theirs_sorted, "4-table chain mismatch on {q}");
    }
}

#[test]
fn reorder_column_order_preserved() {
    // SELECT * must return columns in FROM-clause order after any reorder.
    let setup: &[&str] = &[
        "CREATE TABLE t1 (a INTEGER PRIMARY KEY, x INTEGER)",
        "CREATE TABLE t2 (b INTEGER PRIMARY KEY, x INTEGER)",
        "CREATE TABLE t3 (c INTEGER PRIMARY KEY, x INTEGER)",
        "INSERT INTO t1 VALUES (1, 10), (2, 20)",
        "INSERT INTO t2 VALUES (1, 10), (2, 30)",
        "INSERT INTO t3 VALUES (2, 20), (3, 40)",
    ];
    diff(
        setup,
        "SELECT * FROM t1 JOIN t2 ON t1.x = t2.x JOIN t3 ON t2.x = t3.x ORDER BY 1",
    );
    diff(
        setup,
        "SELECT * FROM t3, t2, t1 WHERE t1.x = t2.x AND t2.x = t3.x ORDER BY 1",
    );
    // Projection referencing every table in mixed order.
    diff(
        setup,
        "SELECT t3.c, t1.a, t2.x FROM t1 JOIN t2 ON t1.x = t2.x \
         JOIN t3 ON t2.x = t3.x ORDER BY 2, 1",
    );
}

#[test]
fn reorder_using_and_multi_key_joins() {
    let setup: &[&str] = &[
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT, dept INT)",
        "CREATE TABLE o (oid INTEGER PRIMARY KEY, id INT, amt INT, dept INT)",
        "CREATE TABLE p (pid INTEGER PRIMARY KEY, dept INT, tag TEXT)",
        "INSERT INTO u VALUES (1, 'ann', 10), (2, 'bob', 20), (3, 'cal', 10)",
        "INSERT INTO o VALUES (1, 1, 100, 10), (2, 1, 200, 10), (3, 2, 300, 20)",
        "INSERT INTO p VALUES (1, 10, 'x'), (2, 20, 'y')",
    ];
    // USING chains reassociate: each `id = id` operand must bind to its
    // OWN side, not both to the first same-named column.
    diff(
        setup,
        "SELECT u.name, o.amt FROM u JOIN o USING (id) JOIN p USING (dept) ORDER BY 1, 2",
    );
    // Multi-key equi AND-chain (hash algorithm, not the old nested loop).
    diff(
        setup,
        "SELECT u.name, o.amt FROM u JOIN o ON u.id = o.id AND u.dept = o.dept ORDER BY 1, 2",
    );
    // Mixed equi + residual ON.
    diff(
        setup,
        "SELECT u.name FROM u JOIN o ON u.id = o.id AND o.amt > 150 ORDER BY 1",
    );
    // Three-table multi-key chain.
    diff(
        setup,
        "SELECT u.name, p.tag FROM u JOIN o ON u.id = o.id AND o.dept = u.dept \
         JOIN p ON o.dept = p.dept ORDER BY 1, 2",
    );
}

#[test]
fn reorder_left_join_boundary_pins() {
    let setup: &[&str] = &[
        "CREATE TABLE d1 (a INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE d2 (b INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE d3 (c INTEGER PRIMARY KEY, x INT)",
        "INSERT INTO d1 VALUES (1, 1), (2, 2), (3, 3)",
        "INSERT INTO d2 VALUES (1, 1), (2, 2)",
        "INSERT INTO d3 VALUES (2, 2), (4, 4)",
    ];
    // The LEFT JOIN boundary must survive: the inner sub-spine (d1 ⋈ d3)
    // may reorder under it, but NULL-extension semantics hold.
    diff(
        setup,
        "SELECT d1.a, d3.c, d2.b FROM d1 JOIN d3 ON d1.x = d3.x \
         LEFT JOIN d2 ON d2.x = d1.x ORDER BY 1, 2, 3",
    );
    // LEFT JOIN on the outside of a 3-atom inner spine.
    diff(
        setup,
        "SELECT d1.a, d2.b, d3.c FROM d1 JOIN d2 ON d1.x = d2.x \
         JOIN d3 ON d2.x = d3.x ORDER BY 1, 2, 3",
    );
    diff(
        setup,
        "SELECT d2.b, d1.a, d3.c FROM d2 LEFT JOIN (d1 JOIN d3 ON d1.x = d3.x) \
         ON d1.x = d2.x ORDER BY 1, 2, 3",
    );
    // WHERE on the null-extended side stays ABOVE the outer join.
    diff(
        setup,
        "SELECT d1.a, d2.b FROM d1 LEFT JOIN d2 ON d1.x = d2.x WHERE d2.b IS NULL ORDER BY 1",
    );
}

#[test]
fn reorder_subquery_atoms_decline_but_stay_correct() {
    let setup: &[&str] = &[
        "CREATE TABLE s1 (a INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE s2 (b INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE s3 (c INTEGER PRIMARY KEY, x INT)",
        "INSERT INTO s1 VALUES (1, 1), (2, 2), (3, 3)",
        "INSERT INTO s2 VALUES (1, 1), (2, 2)",
        "INSERT INTO s3 VALUES (2, 2), (3, 3)",
    ];
    // A subquery atom makes the spine's names non-static: the reorder
    // declines and the syntactic plan answers.
    diff(
        setup,
        "SELECT t.a, s2.b, s3.c FROM (SELECT a, x FROM s1 WHERE x > 1) t \
         JOIN s2 ON t.x = s2.x JOIN s3 ON s2.x = s3.x ORDER BY 1, 2, 3",
    );
    // Correlated subquery in the WHERE stays at the top of the spine.
    diff(
        setup,
        "SELECT s1.a FROM s1 JOIN s2 ON s1.x = s2.x JOIN s3 ON s2.x = s3.x \
         WHERE (SELECT COUNT(*) FROM s2 WHERE s2.x = s1.x) >= 1 ORDER BY 1",
    );
}

#[test]
fn reorder_self_join_aliases() {
    let setup: &[&str] = &[
        "CREATE TABLE emp (id INTEGER PRIMARY KEY, name TEXT, mgr INT)",
        "INSERT INTO emp VALUES (1, 'ceo', NULL), (2, 'ann', 1), (3, 'bob', 1), (4, 'cal', 2)",
    ];
    diff(
        setup,
        "SELECT e.name, m.name FROM emp e JOIN emp m ON e.mgr = m.id ORDER BY 1",
    );
    // 3-atom self-join chain.
    diff(
        setup,
        "SELECT e.name, m.name, g.name FROM emp e JOIN emp m ON e.mgr = m.id \
         JOIN emp g ON m.mgr = g.id ORDER BY 1, 2",
    );
}

#[test]
fn reorder_rowid_lookup_drives() {
    let setup: &[&str] = &[
        "CREATE TABLE r1 (id INTEGER PRIMARY KEY, x INT, t TEXT)",
        "CREATE TABLE r2 (id INTEGER PRIMARY KEY, x INT, u TEXT)",
        "CREATE TABLE r3 (id INTEGER PRIMARY KEY, x INT, v TEXT)",
    ];
    let mut eng = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        eng.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    eng.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=300i64 {
        let x = i % 17;
        let sqls = [
            format!("INSERT INTO r1 (x, t) VALUES ({x}, 't{}')", i % 6),
            format!("INSERT INTO r2 (x, u) VALUES ({x}, 'u{}')", i % 6),
            format!("INSERT INTO r3 (x, v) VALUES ({x}, 'v{}')", i % 6),
        ];
        for s in &sqls {
            eng.execute(s, []).unwrap();
            conn.execute_batch(s).unwrap();
        }
    }
    eng.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();

    let q = "SELECT r1.t, r2.u, r3.v FROM r1 JOIN r2 ON r1.x = r2.x \
             JOIN r3 ON r2.x = r3.x WHERE r1.id = 42";
    let ours = render(&eng.query(q, []).unwrap());
    let mut stmt = conn.prepare(q).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut theirs = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::new();
        for i in 0..3 {
            row.push(match r.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Text(t) => format!("T:{}", String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(v) => format!("I:{v}"),
                rusqlite::types::ValueRef::Real(v) => format!("R:{v}"),
                rusqlite::types::ValueRef::Blob(b) => format!("B:{}", b.len()),
            });
        }
        theirs.push(row);
    }
    let mut ours_sorted = ours.clone();
    ours_sorted.sort();
    let mut theirs_sorted = theirs.clone();
    theirs_sorted.sort();
    assert_eq!(ours_sorted, theirs_sorted, "rowid-driven join mismatch");
    // The point-filtered table must drive.
    let plan = explain(setup, q);
    assert!(
        plan[0].contains("SEARCH r1"),
        "expected r1 to drive (rowid lookup), got {plan:?}"
    );
}

#[test]
fn reorder_aggregates_and_order_by() {
    let setup: &[&str] = &[
        "CREATE TABLE g1 (id INTEGER PRIMARY KEY, k INT, val INT)",
        "CREATE TABLE g2 (id INTEGER PRIMARY KEY, k INT, val INT)",
        "CREATE TABLE g3 (id INTEGER PRIMARY KEY, k INT, val INT)",
    ];
    let mut eng = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        eng.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    eng.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=150i64 {
        for (t, k) in [("g1", i % 5), ("g2", i % 7), ("g3", i % 3)] {
            let sql = format!("INSERT INTO {t} (k, val) VALUES ({k}, {i})");
            eng.execute(&sql, []).unwrap();
            conn.execute_batch(&sql).unwrap();
        }
    }
    eng.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();

    for q in [
        "SELECT g1.k, COUNT(*), SUM(g3.val) FROM g1 JOIN g2 ON g1.k = g2.k \
         JOIN g3 ON g2.k = g3.k GROUP BY g1.k ORDER BY 1",
        "SELECT g3.k, MIN(g1.val), MAX(g2.val) FROM g1 JOIN g2 ON g1.k = g2.k \
         JOIN g3 ON g2.k = g3.k WHERE g3.id < 10 GROUP BY g3.k HAVING COUNT(*) > 2 ORDER BY 1",
        "SELECT g1.val, g2.val, g3.val FROM g1 JOIN g2 ON g1.k = g2.k \
         JOIN g3 ON g2.k = g3.k WHERE g1.id = 3 ORDER BY 3 DESC, 1",
    ] {
        let ours = render(&eng.query(q, []).unwrap());
        let mut stmt = conn.prepare(q).unwrap();
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
        let mut ours_sorted = ours.clone();
        ours_sorted.sort();
        let mut theirs_sorted = theirs.clone();
        theirs_sorted.sort();
        assert_eq!(ours_sorted, theirs_sorted, "aggregate mismatch on {q}");
    }
}

#[test]
fn reorder_multiplicity_and_empties() {
    let setup: &[&str] = &[
        "CREATE TABLE m1 (id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE m2 (id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE m3 (id INTEGER PRIMARY KEY, k INT)",
        // many-to-many: k=1 appears 3×, 2×, 1× across the tables.
        "INSERT INTO m1 VALUES (1, 1), (2, 1), (3, 1), (4, 2), (5, 9)",
        "INSERT INTO m2 VALUES (1, 1), (2, 2), (3, 2)",
        "INSERT INTO m3 VALUES (1, 1)",
    ];
    // 3×2×1 = 6 rows for k=1; 1×2 rows for k=2.
    diff(
        setup,
        "SELECT COUNT(*) FROM m1 JOIN m2 ON m1.k = m2.k JOIN m3 ON m2.k = m3.k",
    );
    diff(
        setup,
        "SELECT m1.id, m2.id, m3.id FROM m1 JOIN m2 ON m1.k = m2.k \
         JOIN m3 ON m2.k = m3.k ORDER BY 1, 2, 3",
    );
    // Empty relations.
    diff(
        setup,
        "SELECT * FROM m1 JOIN m2 ON m1.k = m2.k JOIN m3 ON m2.k = m3.k \
         WHERE m1.k = 999",
    );
}

#[test]
fn reorder_analyze_estimates() {
    let setup: Vec<String> = vec![
        "CREATE TABLE a1 (id INTEGER PRIMARY KEY, k INT, t TEXT)".into(),
        "CREATE TABLE a2 (id INTEGER PRIMARY KEY, k INT, u TEXT)".into(),
        "CREATE TABLE a3 (id INTEGER PRIMARY KEY, k INT, v TEXT)".into(),
    ];
    let mut eng = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in &setup {
        eng.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    eng.execute("BEGIN", []).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    // a1: 500 rows, a2: 50 rows, a3: 5 rows — stat1 should drive a3-first.
    for i in 1..=500i64 {
        let sql = format!("INSERT INTO a1 (k, t) VALUES ({}, 't')", i % 30);
        eng.execute(&sql, []).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    for i in 1..=50i64 {
        let sql = format!("INSERT INTO a2 (k, u) VALUES ({}, 'u')", i % 30);
        eng.execute(&sql, []).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    for i in 1..=5i64 {
        let sql = format!("INSERT INTO a3 (k, v) VALUES ({}, 'v')", i % 30);
        eng.execute(&sql, []).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    eng.execute("COMMIT", []).unwrap();
    conn.execute_batch("COMMIT").unwrap();
    eng.execute("ANALYZE", []).unwrap();
    conn.execute_batch("ANALYZE").unwrap();

    let q = "SELECT a1.t, a2.u, a3.v FROM a1 JOIN a2 ON a1.k = a2.k \
             JOIN a3 ON a2.k = a3.k";
    let ours = render(&eng.query(q, []).unwrap());
    let mut stmt = conn.prepare(q).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut theirs = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::new();
        for i in 0..3 {
            row.push(match r.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Text(t) => format!("T:{}", String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(v) => format!("I:{v}"),
                rusqlite::types::ValueRef::Real(v) => format!("R:{v}"),
                rusqlite::types::ValueRef::Blob(b) => format!("B:{}", b.len()),
            });
        }
        theirs.push(row);
    }
    let mut ours_sorted = ours.clone();
    ours_sorted.sort();
    let mut theirs_sorted = theirs.clone();
    theirs_sorted.sort();
    assert_eq!(
        ours_sorted, theirs_sorted,
        "ANALYZE-driven reorder mismatch"
    );
}

#[test]
fn implicit_join_where_fuses_to_hash() {
    // The classic implicit-join shape must not plan as a cross product +
    // post-filter anymore: `FROM a, b WHERE a.x = b.x` fuses the WHERE
    // conjunct into the join condition.
    let setup: &[&str] = &[
        "CREATE TABLE f1 (id INTEGER PRIMARY KEY, x INT, t TEXT)",
        "CREATE TABLE f2 (id INTEGER PRIMARY KEY, x INT, u TEXT)",
        "INSERT INTO f1 VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 1, 'c')",
        "INSERT INTO f2 VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 9, 'z')",
    ];
    diff(
        setup,
        "SELECT f1.t, f2.u FROM f1, f2 WHERE f1.x = f2.x ORDER BY 1, 2",
    );
    diff(setup, "SELECT COUNT(*) FROM f1, f2 WHERE f1.x = f2.x");
    // Mixed: one fusable conjunct + one single-table filter.
    diff(
        setup,
        "SELECT f1.t, f2.u FROM f1, f2 WHERE f1.x = f2.x AND f2.id > 1 ORDER BY 1, 2",
    );
    // Residual-only (no equi) keeps working.
    diff(
        setup,
        "SELECT f1.t, f2.u FROM f1, f2 WHERE f1.x < f2.x ORDER BY 1, 2",
    );
    // Three-table implicit chain.
    let setup3: &[&str] = &[
        "CREATE TABLE h1 (id INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE h2 (id INTEGER PRIMARY KEY, x INT)",
        "CREATE TABLE h3 (id INTEGER PRIMARY KEY, x INT)",
        "INSERT INTO h1 VALUES (1, 1), (2, 2)",
        "INSERT INTO h2 VALUES (1, 1), (2, 2), (3, 3)",
        "INSERT INTO h3 VALUES (1, 2), (2, 3)",
    ];
    diff(
        setup3,
        "SELECT h1.id, h2.id, h3.id FROM h1, h2, h3 \
         WHERE h1.x = h2.x AND h2.x = h3.x ORDER BY 1, 2, 3",
    );
}

#[test]
fn reorder_cte_atoms() {
    let setup: &[&str] = &[
        "CREATE TABLE c0 (id INTEGER PRIMARY KEY, x INT)",
        "INSERT INTO c0 VALUES (1, 1), (2, 2), (3, 3)",
    ];
    // CTE atoms are statically named (CteRows carries its columns) and
    // participate in the reorder pool.
    diff(
        setup,
        "WITH cte AS (SELECT x, id FROM c0) \
         SELECT c0.id, cte.id FROM c0 JOIN cte ON c0.x = cte.x ORDER BY 1, 2",
    );
    diff(
        setup,
        "WITH a AS (SELECT x, id FROM c0), b AS (SELECT x, id FROM c0) \
         SELECT a.id, b.id, c0.id FROM a JOIN b ON a.x = b.x JOIN c0 ON b.x = c0.x ORDER BY 1, 2, 3",
    );
}
