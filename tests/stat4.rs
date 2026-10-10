//! sqlite_stat4 tests: ANALYZE's histogram — schema parity with SQLite,
//! the row format (neq/nlt/ndlt/sample), sample discipline (<= 24,
//! monotone nlt), planner use (sample-refined equality + interpolated
//! range estimates), reopen persistence, and DROP cleanup.

use rustqlite::Database;

fn q1(db: &mut Database, sql: &str) -> String {
    match db.query(sql, []) {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.first())
            .map(|v| v.as_text())
            .unwrap_or_else(|| "<none>".to_string()),
        Err(e) => format!("<err {}>", e),
    }
}

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

fn detail(db: &mut Database, sql: &str) -> String {
    match db.query(sql, []) {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.get(3))
            .map(|v| v.as_text())
            .unwrap_or_else(|| "<none>".to_string()),
        Err(e) => format!("<err {}>", e),
    }
}

#[test]
fn stat4_schema_parity() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t(a INT, b INT);
         CREATE INDEX ia ON t(a);
         INSERT INTO t VALUES (1, 1), (2, 2), (3, 3);
         ANALYZE;",
        [],
    )
    .unwrap();
    // SQLite's exact DDL text.
    let sql = q1(
        &mut db,
        "SELECT sql FROM sqlite_master WHERE name='sqlite_stat4'",
    );
    assert_eq!(
        sql,
        "CREATE TABLE sqlite_stat4(tbl,idx,neq,nlt,ndlt,sample)"
    );
    // Columns: (tbl, idx, neq, nlt, ndlt, sample).
    let names = db
        .query("PRAGMA table_info(sqlite_stat4)", [])
        .unwrap()
        .iter()
        .map(|r| r[1].as_text())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["tbl", "idx", "neq", "nlt", "ndlt", "sample"]);
    // One sample per tiny index (3 rows, interval 1 → up to 3 samples).
    let n = q1(&mut db, "SELECT count(*) FROM sqlite_stat4 WHERE idx='ia'");
    assert!(n.parse::<i64>().unwrap() >= 1 && n.parse::<i64>().unwrap() <= 24);
    // Row format: neq/nlt/ndlt space-joined ints, sample a BLOB.
    let rows = db
        .query(
            "SELECT typeof(neq), typeof(nlt), typeof(ndlt), typeof(sample) FROM sqlite_stat4",
            [],
        )
        .unwrap();
    assert!(!rows.is_empty());
    for r in &rows {
        assert_eq!(r[0].as_text(), "text");
        assert_eq!(r[1].as_text(), "text");
        assert_eq!(r[2].as_text(), "text");
        assert_eq!(r[3].as_text(), "blob");
    }
}

#[test]
fn stat4_sample_discipline() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t(a INT, b INT);
         CREATE INDEX iab ON t(a, b);
         CREATE INDEX ia ON t(a);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 1000)
         INSERT INTO t SELECT i % 10, i % 100 FROM s;
         ANALYZE;",
        [],
    )
    .unwrap();
    // <= 24 samples per index.
    for idx in ["ia", "iab"] {
        let n: i64 = q1(
            &mut db,
            &format!("SELECT count(*) FROM sqlite_stat4 WHERE idx='{}'", idx),
        )
        .parse()
        .unwrap();
        assert!((1..=24).contains(&n), "{}: {} samples", idx, n);
    }
    // nlt is monotonically increasing within an index, per prefix.
    let rows = db
        .query(
            "SELECT nlt FROM sqlite_stat4 WHERE idx='iab' ORDER BY rowid",
            [],
        )
        .unwrap();
    let mut prev_last: i64 = -1;
    for r in &rows {
        let nlt: Vec<i64> = r[0]
            .as_text()
            .split_whitespace()
            .map(|x| x.parse().unwrap())
            .collect();
        // (a, b, rowid) — the rowid pseudo-column is the last prefix,
        // exactly like SQLite's stat4 rows.
        assert_eq!(nlt.len(), 3);
        assert!(nlt[0] <= nlt[1]);
        assert!(nlt[1] <= nlt[2]);
        assert!(
            nlt[2] > prev_last,
            "nlt not monotone: {} after {}",
            nlt[2],
            prev_last
        );
        prev_last = nlt[2];
    }
    // The last sample's full-key nlt is within one row of the total.
    let last = db
        .query(
            "SELECT nlt FROM sqlite_stat4 WHERE idx='iab' ORDER BY rowid DESC LIMIT 1",
            [],
        )
        .unwrap();
    let nlt_last: i64 = last[0][0]
        .as_text()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap();
    assert!((975..=1000).contains(&nlt_last), "last nlt: {}", nlt_last);
    // The sample record decodes: 3 values (a, b, rowid).
    let sample = db
        .query(
            "SELECT length(sample) FROM sqlite_stat4 WHERE idx='iab' LIMIT 1",
            [],
        )
        .unwrap();
    let len: i64 = sample[0][0].as_text().parse().unwrap();
    assert!((4..=32).contains(&len), "sample len {}", len);
}

#[test]
fn stat4_stat1_unchanged() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t(a INT);
         CREATE INDEX ia ON t(a);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 100)
         INSERT INTO t SELECT i % 10 FROM s;
         ANALYZE;",
        [],
    )
    .unwrap();
    let stat = q1(&mut db, "SELECT stat FROM sqlite_stat1 WHERE idx='ia'");
    assert_eq!(stat, "100 10");
}

/// Skew differential: a highly skewed first column where stat1's average
/// misleads and the histogram (an exact sample match) drives the plan.
#[test]
fn stat4_planner_uses_histogram() {
    // 900 rows a=1, 50 rows a=2, ~1 row each for a=3..53.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t(a INT, b INT);
         CREATE INDEX ia ON t(a);
         CREATE INDEX ib ON t(b);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 1000)
         INSERT INTO t SELECT CASE WHEN i <= 900 THEN 1 WHEN i <= 950 THEN 2 ELSE i - 947 END, i FROM s;
         ANALYZE;",
        [],
    )
    .unwrap();
    let oracle = lite();
    oracle
        .execute_batch(
            "CREATE TABLE t(a INT, b INT);
             CREATE INDEX ia ON t(a);
             CREATE INDEX ib ON t(b);
             WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 1000)
             INSERT INTO t SELECT CASE WHEN i <= 900 THEN 1 WHEN i <= 950 THEN 2 ELSE i - 947 END, i FROM s;
             ANALYZE;",
        )
        .unwrap();
    // Plans agree with the oracle across the skew spectrum.
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 1",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 2",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 40",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b = 500",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a > 2",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a BETWEEN 2 AND 40",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b > 900",
    ] {
        let mine = detail(&mut db, sql);
        let theirs: String = oracle
            .query_row(sql, [], |r| r.get::<&str, String>("detail"))
            .unwrap_or_default();
        // Normalize SQLite's parameter markers vs ours when both are
        // index searches with the same access shape: compare the
        // index name + access kind prefix.
        assert_eq!(
            normalize_plan(&mine),
            normalize_plan(&theirs),
            "plan divergence on {}: ours='{}' theirs='{}'",
            sql,
            mine,
            theirs
        );
    }
}

/// Both engines render `SEARCH t USING INDEX ia (a=?)`-shaped details;
/// normalize minor operator-printing differences (e.g. `b>? AND b<?)`
/// vs spacing) by collapsing whitespace and comparing the index name and
/// verb.
fn normalize_plan(d: &str) -> String {
    let collapsed: String = d.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

#[test]
fn stat4_reopen_persistence() {
    let path = std::env::temp_dir().join("stat4_persist_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE t(a INT);
             CREATE INDEX ia ON t(a);
             WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 500)
             INSERT INTO t SELECT i % 7 FROM s;
             ANALYZE;",
            [],
        )
        .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let n = q1(&mut db, "SELECT count(*) FROM sqlite_stat4");
        assert!(n.parse::<i64>().unwrap() >= 1);
        // The histogram still informs planning after reopen.
        let p = detail(&mut db, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 3");
        assert!(p.contains("ia"), "plan: {}", p);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stat4_drop_cleanup() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t(a INT, b INT);
         CREATE INDEX ia ON t(a);
         CREATE INDEX ib ON t(b);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 200)
         INSERT INTO t SELECT i % 5, i FROM s;
         ANALYZE;",
        [],
    )
    .unwrap();
    // Both indexes have stats.
    let n = q1(&mut db, "SELECT count(DISTINCT idx) FROM sqlite_stat4");
    assert_eq!(n, "2");
    // DROP INDEX removes its stat1 + stat4 rows.
    db.execute("DROP INDEX ia", []).unwrap();
    let n4 = q1(&mut db, "SELECT count(*) FROM sqlite_stat4 WHERE idx='ia'");
    assert_eq!(n4, "0");
    let n1 = q1(&mut db, "SELECT count(*) FROM sqlite_stat1 WHERE idx='ia'");
    assert_eq!(n1, "0");
    let keep = q1(&mut db, "SELECT count(DISTINCT idx) FROM sqlite_stat4");
    assert_eq!(keep, "1");
    // DROP TABLE removes everything for the table.
    db.execute("DROP TABLE t", []).unwrap();
    let n = q1(&mut db, "SELECT count(*) FROM sqlite_stat4");
    assert_eq!(n, "0");
    let n = q1(&mut db, "SELECT count(*) FROM sqlite_stat1");
    assert_eq!(n, "0");
}

/// Re-ANALYZE refreshes samples after data changes (no stale residue).
#[test]
fn stat4_reanalyze_refreshes() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t(a INT);
         CREATE INDEX ia ON t(a);
         INSERT INTO t VALUES (1), (2), (3);
         ANALYZE;",
        [],
    )
    .unwrap();
    let before = q1(&mut db, "SELECT count(*) FROM sqlite_stat4");
    db.execute(
        "DELETE FROM t;
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 100)
         INSERT INTO t SELECT i % 4 FROM s;
         ANALYZE;",
        [],
    )
    .unwrap();
    let after = q1(&mut db, "SELECT count(*) FROM sqlite_stat4");
    assert!(after.parse::<i64>().unwrap() >= before.parse::<i64>().unwrap());
    // nlt totals track the new row count (100 rows, monotone to ~100).
    let last = db
        .query(
            "SELECT nlt FROM sqlite_stat4 ORDER BY rowid DESC LIMIT 1",
            [],
        )
        .unwrap();
    let nlt_last: i64 = last[0][0]
        .as_text()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!((75..=100).contains(&nlt_last), "last nlt: {}", nlt_last);
    // The rowid pseudo-prefix is present (2 entries for a 1-column index).
    let two = db
        .query("SELECT neq FROM sqlite_stat4 WHERE idx='ia' LIMIT 1", [])
        .unwrap();
    let n: usize = two[0][0].as_text().split_whitespace().count();
    assert_eq!(n, 2);
}

/// Targeted ANALYZE (one table / one index) replaces only that table's
/// samples — SQLite's contract.
#[test]
fn stat4_targeted_analyze() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t1(a INT);
         CREATE TABLE t2(b INT);
         CREATE INDEX i1 ON t1(a);
         CREATE INDEX i2 ON t2(b);
         INSERT INTO t1 VALUES (1), (1), (2);
         INSERT INTO t2 VALUES (5), (6), (7);
         ANALYZE;",
        [],
    )
    .unwrap();
    let both = q1(&mut db, "SELECT count(DISTINCT idx) FROM sqlite_stat4");
    assert_eq!(both, "2");
    // Targeted: only t2's rows are replaced.
    db.execute("ANALYZE t2", []).unwrap();
    let idxs = db
        .query("SELECT DISTINCT idx FROM sqlite_stat4", [])
        .unwrap();
    assert_eq!(idxs.len(), 2);
    // By index name: analyzes the owning table.
    db.execute("ANALYZE i1", []).unwrap();
    let n = q1(&mut db, "SELECT count(DISTINCT tbl) FROM sqlite_stat4");
    assert_eq!(n, "2");
}

/// Every sqlite_stat4 row (and sqlite_stat1 row) ANALYZE writes equals
/// the bundled SQLite's, byte for byte: the same samples chosen by
/// analyze.c's sampler (periodic samples, per-prefix repetition
/// candidates, eviction, hash tiebreaks) with the same neq/nlt/ndlt
/// counters and sample records — over index shapes (multi-column, DESC,
/// UNIQUE nullable / NOT NULL, partial, expression, NOCASE, on the
/// INTEGER PRIMARY KEY, WITHOUT ROWID) and table sizes around the
/// periodic-interval and 24-sample boundaries.
#[test]
fn stat4_rows_match_sqlite_exactly() {
    fn rows_lite(c: &rusqlite::Connection, sql: &str) -> Vec<String> {
        let mut st = c.prepare(sql).unwrap();
        let n = st.column_count();
        let mut out = Vec::new();
        let mut rows = st.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let cells: Vec<String> = (0..n)
                .map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap()))
                .collect();
            out.push(cells.join("|"));
        }
        out
    }
    fn rows_ours(db: &mut Database, sql: &str) -> Vec<String> {
        db.query(sql, [])
            .unwrap()
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| match v {
                        rustqlite::Value::Null => "Null".to_string(),
                        rustqlite::Value::Integer(i) => format!("Integer({i})"),
                        rustqlite::Value::Real(f) => format!("Real({f:?})"),
                        rustqlite::Value::Text(t) => format!("Text({:?})", t.as_str()),
                        rustqlite::Value::Blob(b) => format!("Blob({b:?})"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    }
    const SCHEMA: &[&str] = &[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL, d INTEGER NOT NULL)",
        "CREATE INDEX ia ON t(a)",
        "CREATE INDEX iab ON t(a, b)",
        "CREATE INDEX iba ON t(b DESC, a)",
        "CREATE INDEX ibn ON t(b COLLATE NOCASE)",
        "CREATE UNIQUE INDEX ud ON t(d)",
        "CREATE UNIQUE INDEX uc ON t(c)",
        "CREATE UNIQUE INDEX uda ON t(d, a)",
        "CREATE INDEX ip ON t(a) WHERE a > 3",
        "CREATE INDEX ix ON t(a % 3, c)",
        "CREATE UNIQUE INDEX uid ON t(id)",
        "CREATE TABLE w (k1 INTEGER, k2 TEXT, v INTEGER, PRIMARY KEY (k1, k2)) WITHOUT ROWID",
        "CREATE INDEX wv ON w(v)",
    ];
    let mut checked = 0usize;
    for &n in &[
        0usize, 1, 2, 3, 8, 9, 10, 17, 24, 25, 26, 60, 100, 217, 500, 1500,
    ] {
        for skew in [2u64, 7, 1000] {
            let mut db = Database::open_in_memory().unwrap();
            let c = lite();
            for s in SCHEMA {
                db.execute(s, []).unwrap();
                c.execute_batch(s).unwrap();
            }
            let mut x: u64 = 0x9e3779b97f4a7c15 ^ (n as u64) ^ (skew << 20);
            let mut next = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let mut vals = Vec::new();
            let mut wvals = Vec::new();
            for i in 0..n {
                // Skewed `a` (a few heavy keys), case-varying `b`, unique
                // `c` with NULLs, unique NOT NULL `d`.
                let r = next();
                let a = if r % 4 == 0 {
                    (r >> 8) % skew
                } else {
                    (r >> 8) % 3
                };
                let b = ["x", "X", "y", "Y", "zz", "a"][(r >> 20) as usize % 6];
                let c_val = if r % 5 == 0 {
                    "NULL".to_string()
                } else {
                    format!("{}.5", i)
                };
                vals.push(format!("({a}, '{b}', {c_val}, {})", n - i));
                wvals.push(format!("({}, 'k{}', {})", r % 11, i, (r >> 30) % skew));
            }
            for chunk in vals.chunks(200) {
                let sql = format!("INSERT INTO t (a, b, c, d) VALUES {}", chunk.join(", "));
                db.execute(&sql, []).unwrap();
                c.execute_batch(&sql).unwrap();
            }
            for chunk in wvals.chunks(200) {
                let sql = format!("INSERT INTO w VALUES {}", chunk.join(", "));
                db.execute(&sql, []).unwrap();
                c.execute_batch(&sql).unwrap();
            }
            db.execute("ANALYZE", []).unwrap();
            c.execute_batch("ANALYZE").unwrap();
            for q in [
                "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY 1, 2",
                "SELECT tbl, idx, neq, nlt, ndlt, hex(sample) FROM sqlite_stat4 ORDER BY 1, 2, 4",
            ] {
                assert_eq!(
                    rows_ours(&mut db, q),
                    rows_lite(&c, q),
                    "[n={n} skew={skew}] {q}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0);
}
