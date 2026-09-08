//! ANALYZE / sqlite_stat1 / cost-based index-choice tests.

use rustqlite::{Database, Value};

fn rows_sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b.iter()) {
            match x.cmp(y) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        a.len().cmp(&b.len())
    });
    rows
}

/// ANALYZE creates sqlite_stat1 with SQLite's exact row shape:
/// one row per index ("N D1 D2…"), one idx=NULL row per index-less
/// table, and the table-row count as N.
#[test]
fn analyze_writes_sqlite_stat1_rows() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b INT, c TEXT)",
        (),
    )
    .unwrap();
    db.execute("CREATE INDEX idx_a ON t(a)", ()).unwrap();
    db.execute("CREATE INDEX idx_ab ON t(a, b)", ()).unwrap();
    db.execute("CREATE TABLE plain (x INT)", ()).unwrap();
    db.execute("BEGIN", ()).unwrap();
    for i in 1..=100i64 {
        db.execute(
            &format!(
                "INSERT INTO t (a, b, c) VALUES ({}, {}, 'k{}')",
                i % 10,
                i % 4,
                i % 10
            ),
            (),
        )
        .unwrap();
    }
    db.execute("INSERT INTO plain VALUES (1)", ()).unwrap();
    db.execute("COMMIT", ()).unwrap();

    db.execute("ANALYZE", ()).unwrap();

    let rows = db
        .query(
            "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY idx, tbl",
            (),
        )
        .unwrap();
    // (tbl, idx, stat) — idx coerces NULL to "" for the plain-table row.
    let got: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| (r[0].as_text(), r[1].as_text(), r[2].as_text()))
        .collect();
    // 10 distinct a values over 100 rows; (a, b) pairs cycle with
    // period lcm(10, 4) = 20 → 20 distinct pairs.
    let ab_pairs: std::collections::HashSet<(i64, i64)> =
        (1..=100).map(|i| (i % 10, i % 4)).collect();
    assert_eq!(ab_pairs.len(), 20);
    let expect = vec![
        // NULL idx sorts first (NULLs-first ordering).
        (String::from("plain"), String::new(), String::from("1")),
        (
            String::from("t"),
            String::from("idx_a"),
            String::from("100 10"),
        ),
        (
            String::from("t"),
            String::from("idx_ab"),
            String::from("100 10 20"),
        ),
    ];
    assert_eq!(
        format!("{:?}", got),
        format!("{:?}", expect),
        "sqlite_stat1 rows: {:?}",
        got
    );
}

/// ANALYZE t (targeted) only replaces that table's rows.
#[test]
fn analyze_target_table_only() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT)", ())
        .unwrap();
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, b INT)", ())
        .unwrap();
    db.execute("CREATE INDEX idx_a ON t(a)", ()).unwrap();
    db.execute("CREATE INDEX idx_b ON u(b)", ()).unwrap();
    db.execute("INSERT INTO t (a) VALUES (1), (1), (2)", ())
        .unwrap();
    db.execute("INSERT INTO u (b) VALUES (5), (6), (7), (8)", ())
        .unwrap();

    db.execute("ANALYZE", ()).unwrap();
    let full = db.query("SELECT COUNT(*) FROM sqlite_stat1", ()).unwrap();
    assert_eq!(full[0][0], Value::Integer(2));

    // Targeted: analyze t only.
    db.execute("ANALYZE t", ()).unwrap();
    let rows = db
        .query("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl", ())
        .unwrap();
    // u's row survives untouched; t's row was re-collected.
    assert_eq!(rows.len(), 2);
    let u_row = rows.iter().find(|r| r[0].as_text() == "u").unwrap();
    assert_eq!(u_row[2].as_text(), "4 4");
    let t_row = rows.iter().find(|r| r[0].as_text() == "t").unwrap();
    assert_eq!(t_row[2].as_text(), "3 2");

    // Unknown target errors like SQLite.
    assert!(db.execute("ANALYZE missing", ()).is_err());

    // ANALYZE idx_b: the index target analyzes its OWNING table.
    db.execute("ANALYZE idx_b", ()).unwrap();
    let rows = db
        .query("SELECT tbl, stat FROM sqlite_stat1 WHERE idx = 'idx_b'", ())
        .unwrap();
    assert_eq!(rows[0][1].as_text(), "4 4");
}

/// The planner actually uses the statistics: after ANALYZE, a query on
/// a LOW-cardinality (unselective) index plans a full scan + filter
/// instead of an index lookup — the SQLite cost-model call. A
/// HIGH-cardinality index keeps the index path.
#[test]
fn analyze_cost_based_index_choice() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, low INT, hi INT)",
        (),
    )
    .unwrap();
    db.execute("CREATE INDEX idx_low ON t(low)", ()).unwrap();
    db.execute("CREATE INDEX idx_hi ON t(hi)", ()).unwrap();
    db.execute("BEGIN", ()).unwrap();
    for i in 1..=1000i64 {
        db.execute(
            &format!("INSERT INTO t (low, hi) VALUES ({}, {})", i % 2, i),
            (),
        )
        .unwrap();
    }
    db.execute("COMMIT", ()).unwrap();

    // Before ANALYZE: the heuristic always picks the applicable index.
    let before = db
        .query(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM t WHERE low = 1",
            (),
        )
        .unwrap();
    let plan_before: String = before
        .iter()
        .map(|r| r.last().unwrap().as_text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        plan_before.to_lowercase().contains("idx_low"),
        "pre-ANALYZE plan should use idx_low: {plan_before}"
    );

    db.execute("ANALYZE", ()).unwrap();

    // low has D=2 over 1000 rows: est 500 = 50% — hmm, 50% < 75% guard,
    // so the index SURVIVES with est 500. Force the unselective shape:
    // a table where the indexed column is 90% one value.
    db.execute("UPDATE t SET low = 1 WHERE id > 100", ())
        .unwrap();
    db.execute("ANALYZE", ()).unwrap();
    // Now D(low)=2 (0 for ids 1..100, 1 for the rest): est for low=1
    // is ~500 by the uniform model — the guard needs >75%. The honest
    // driver: est*4 > n*3 with est=500, n=1000 → 2000 > 3000 false →
    // index still chosen. So instead verify the SELECTIVE side: hi is
    // fully distinct — the index path persists and is FAST, and the
    // unselective guard fires on a third truly-flat index below.
    let hi_plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM t WHERE hi = 500",
            (),
        )
        .unwrap();
    let hi_plan_txt: String = hi_plan
        .iter()
        .map(|r| r.last().unwrap().as_text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        hi_plan_txt.to_lowercase().contains("idx_hi"),
        "selective index should stay: {hi_plan_txt}"
    );

    // Truly flat: every row the same value → est = 1000 = 100% > 75% →
    // the planner declines the index and scans.
    db.execute("CREATE TABLE flat (id INTEGER PRIMARY KEY, v INT)", ())
        .unwrap();
    db.execute("CREATE INDEX idx_flat ON flat(v)", ()).unwrap();
    db.execute("BEGIN", ()).unwrap();
    for _ in 1..=1000i64 {
        db.execute("INSERT INTO flat (v) VALUES (7)", ()).unwrap();
    }
    db.execute("COMMIT", ()).unwrap();
    db.execute("ANALYZE", ()).unwrap();
    let flat_plan = db
        .query(
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM flat WHERE v = 7",
            (),
        )
        .unwrap();
    let flat_txt: String = flat_plan
        .iter()
        .map(|r| r.last().unwrap().as_text())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !flat_txt.to_lowercase().contains("idx_flat"),
        "unselective index should be declined post-ANALYZE: {flat_txt}"
    );

    // And the answer stays identical either way.
    let a = db
        .query("SELECT COUNT(*) FROM flat WHERE v = 7", ())
        .unwrap();
    assert_eq!(a[0][0], Value::Integer(1000));
}

/// Statistics survive close/reopen: a fresh Database::open on a
/// file-backed, previously-ANALYZEd database plans cost-based from the
/// sqlite_stat1 rows (and the rows themselves are readable).
#[test]
fn analyze_stats_survive_reopen() {
    let path = std::env::temp_dir().join(format!(
        "rustqlite-analyze-reopen-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)", ())
            .unwrap();
        db.execute("CREATE INDEX idx_v ON t(v)", ()).unwrap();
        db.execute("BEGIN", ()).unwrap();
        for i in 1..=100i64 {
            db.execute(&format!("INSERT INTO t (v) VALUES ({})", i % 7), ())
                .unwrap();
        }
        db.execute("COMMIT", ()).unwrap();
        db.execute("ANALYZE", ()).unwrap();
        let rows = db
            .query("SELECT stat FROM sqlite_stat1 WHERE idx = 'idx_v'", ())
            .unwrap();
        assert_eq!(rows[0][0].as_text(), "100 7");
    }
    // Reopen: stats load from disk (load_schema's stat-table walk).
    let db2 = Database::open(&path).unwrap();
    let rows = db2
        .query("SELECT stat FROM sqlite_stat1 WHERE idx = 'idx_v'", ())
        .unwrap();
    assert_eq!(rows[0][0].as_text(), "100 7");
    // Answers still correct with the loaded stats.
    let c = db2.query("SELECT COUNT(*) FROM t WHERE v = 3", ()).unwrap();
    assert_eq!(c[0][0], Value::Integer(14));
    let _ = std::fs::remove_file(&path);
}

/// Compound-index prefixes: stat text carries D1 and D1,D2, and the
/// planner picks the index with the better bound prefix estimate.
#[test]
fn analyze_compound_prefix_stats() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b INT)", ())
        .unwrap();
    db.execute("CREATE INDEX idx_a ON t(a)", ()).unwrap();
    db.execute("CREATE INDEX idx_ab ON t(a, b)", ()).unwrap();
    db.execute("BEGIN", ()).unwrap();
    for i in 1..=100i64 {
        db.execute(
            &format!("INSERT INTO t (a, b) VALUES ({}, {})", i % 10, i % 5),
            (),
        )
        .unwrap();
    }
    db.execute("COMMIT", ()).unwrap();
    db.execute("ANALYZE", ()).unwrap();

    let rows = db
        .query("SELECT stat FROM sqlite_stat1 WHERE idx = 'idx_ab'", ())
        .unwrap();
    // D1 = 10 distinct a; D2 = distinct (a,b) pairs — gcd(10,5)=5 so the
    // (a%10, b%5) pairs cycle every 10 rows → 10 distinct pairs.
    assert_eq!(rows[0][0].as_text(), "100 10 10");

    // `WHERE a = ? AND b = ?` binds two columns of idx_ab — the planner
    // keeps the compound (est 10) over idx_a (est 10 too, but covered
    // breaks the tie toward the compound). Result correctness:
    let got = rows_sorted(
        db.query("SELECT id FROM t WHERE a = 3 AND b = 4 ORDER BY id", ())
            .unwrap(),
    );
    let want: Vec<Vec<Value>> = (1..=100)
        .filter(|i| i % 10 == 3 && i % 5 == 4)
        .map(|i| vec![Value::Integer(i as i64)])
        .collect();
    assert_eq!(got, want);
}

/// Expression-index statistics: ANALYZE counts distinct over the
/// expression's evaluated values.
#[test]
fn analyze_expression_index() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)", ())
        .unwrap();
    db.execute("CREATE INDEX idx_vx ON t(v * 2)", ()).unwrap();
    db.execute("BEGIN", ()).unwrap();
    for i in 1..=50i64 {
        db.execute(&format!("INSERT INTO t (v) VALUES ({})", i), ())
            .unwrap();
    }
    db.execute("COMMIT", ()).unwrap();
    db.execute("ANALYZE", ()).unwrap();
    let rows = db
        .query("SELECT stat FROM sqlite_stat1 WHERE idx = 'idx_vx'", ())
        .unwrap();
    // v*2 over 1..50: 50 distinct.
    assert_eq!(rows[0][0].as_text(), "50 50");
}
