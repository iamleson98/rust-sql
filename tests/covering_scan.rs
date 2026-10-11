//! The FULL-SCAN VARIANT choice (SQLite whereLoopAddBtree's covering
//! index full scan): when a statement's residual access path is a plain
//! scan, SQLite prices every index's covering full-index scan against
//! the table scan and takes the cheapest — and the two variants EMIT
//! ROWS IN DIFFERENT ORDERS (index order vs rowid order), which is
//! observable in unordered SELECT output. This suite pins the engine's
//! variant choice to bundled SQLite, plan (EXPLAIN) and rows both.
//!
//! Cost model (LogEst, mirrored in `sqlite_cost::best_full_scan_loop`):
//! table scan `N + 16` (`N + 14` with STAT4), covering index scan
//! `N_i + 1 + 15*szIdxRow/szTabRow`; ties keep the table scan (sPk
//! leads the probe chain); one-pass DML, unknown column use, DESC /
//! expression keys and WITHOUT ROWID tables never take the index scan.

use rustqlite::{Database, Value};

/// One dual-engine case: run the setup + query on both engines, compare
/// the full row STREAM byte-for-byte (text as hex — same values, same
/// order).
struct Case {
    name: &'static str,
    /// Statements run on both engines (setup + DML).
    setup: &'static [&'static str],
    /// The query whose output ORDER is compared.
    query: &'static str,
    /// Expected EXPLAIN QUERY PLAN detail (first row) — None skips the
    /// plan check.
    plan: Option<&'static str>,
}

fn fmt_engine(rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    for r in rows {
        if !out.is_empty() {
            out.push('|');
        }
        for (i, c) in r.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match c {
                Value::Null => out.push('~'),
                Value::Integer(v) => out.push_str(&v.to_string()),
                Value::Real(f) => out.push_str(&format!("{f:?}")),
                Value::Text(t) => out.push_str(
                    &t.as_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                ),
                Value::Blob(b) => {
                    out.push_str(&b.iter().map(|x| format!("{x:02x}")).collect::<String>())
                }
            }
        }
    }
    out
}

fn fmt_sqlite(rows: &[Vec<rusqlite::types::Value>]) -> String {
    use rusqlite::types::Value as Sv;
    let mut out = String::new();
    for r in rows {
        if !out.is_empty() {
            out.push('|');
        }
        for (i, c) in r.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match c {
                Sv::Null => out.push('~'),
                Sv::Integer(v) => out.push_str(&v.to_string()),
                Sv::Real(f) => out.push_str(&format!("{f:?}")),
                Sv::Text(t) => out.push_str(
                    &t.as_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                ),
                Sv::Blob(b) => {
                    out.push_str(&b.iter().map(|x| format!("{x:02x}")).collect::<String>())
                }
            }
        }
    }
    out
}

fn sqlite_rows(rc: &rusqlite::Connection, q: &str) -> String {
    let mut stmt = rc.prepare(q).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut out: Vec<Vec<rusqlite::types::Value>> = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::new();
        for i in 0..r.as_ref().column_count() {
            row.push(r.get(i).unwrap());
        }
        out.push(row);
    }
    fmt_sqlite(&out)
}

fn engine_rows(db: &Database, q: &str) -> String {
    fmt_engine(&db.query(q, []).unwrap())
}

fn engine_plan(db: &Database, q: &str) -> String {
    db.query(&format!("EXPLAIN QUERY PLAN {q}"), [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[3] {
            Value::Text(t) => t.to_string(),
            v => panic!("detail column: {v:?}"),
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

fn sqlite_plan(rc: &rusqlite::Connection, q: &str) -> String {
    let mut stmt = rc.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push(r.get::<_, String>(3).unwrap());
    }
    out.join(" / ")
}

fn run(cases: &[Case]) {
    for c in cases {
        let mut db = Database::open_in_memory().unwrap();
        let rc = rusqlite::Connection::open_in_memory().unwrap();
        for s in c.setup {
            db.execute(s, []).unwrap();
            rc.execute_batch(s).unwrap();
        }
        let eng = engine_rows(&db, c.query);
        let sq = sqlite_rows(&rc, c.query);
        assert_eq!(eng, sq, "rows diverge: case {}", c.name);
        if let Some(want) = c.plan {
            let eng_plan = engine_plan(&db, c.query);
            let sq_plan = sqlite_plan(&rc, c.query);
            assert_eq!(eng_plan, sq_plan, "plan diverges: case {}", c.name);
            assert_eq!(eng_plan, want, "plan wording: case {}", c.name);
        }
    }
}

/// The standard fixture: an ANALYZE'd table with single-column indexes
/// narrower than the (wide) table row.
const SETUP: &[&str] = &[
    "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT)",
    "CREATE INDEX t_a ON t(a)",
    "CREATE INDEX t_k ON t(k)",
    "CREATE INDEX t_kv ON t(k, v)",
    "INSERT INTO t VALUES
     (1, 3, 10, 'x'), (2, 1, 20, 'y'), (3, 3, 10, 'z'),
     (5, 2, 30, 'w'), (7, 1, 20, 'v'), (11, 2, 10, 'u')",
    "ANALYZE",
];

#[test]
fn covering_scan_order_matches_sqlite() {
    run(&[
        Case {
            name: "projection via t_a (a order, not rowid order)",
            setup: SETUP,
            query: "SELECT a FROM t",
            plan: Some("SCAN t USING COVERING INDEX t_a"),
        },
        Case {
            name: "residual reading an uncovered column keeps the table scan",
            setup: SETUP,
            query: "SELECT a FROM t WHERE a > 1 OR k = 10",
            plan: Some("SCAN t"),
        },
        Case {
            name: "residual over covered columns scans t_a",
            setup: SETUP,
            query: "SELECT a FROM t WHERE a > 1 OR a IS NULL",
            plan: Some("SCAN t USING COVERING INDEX t_a"),
        },
        Case {
            name: "star reads everything: table scan",
            setup: SETUP,
            query: "SELECT * FROM t",
            plan: Some("SCAN t"),
        },
        Case {
            name: "uncovered column: table scan",
            setup: SETUP,
            query: "SELECT v FROM t",
            plan: Some("SCAN t"),
        },
        Case {
            name: "GROUP BY over the covering scan (first-seen = index order)",
            setup: SETUP,
            query: "SELECT a, count(*) FROM t GROUP BY a",
            plan: None,
        },
        Case {
            name: "DISTINCT over the covering scan",
            setup: SETUP,
            query: "SELECT DISTINCT a FROM t",
            plan: None,
        },
        Case {
            name: "LIMIT over the covering scan",
            setup: SETUP,
            query: "SELECT a FROM t LIMIT 3",
            plan: Some("SCAN t USING COVERING INDEX t_a"),
        },
        Case {
            name: "count(*) result parity (fused path keeps the count)",
            setup: SETUP,
            query: "SELECT count(*) FROM t",
            plan: None,
        },
    ]);
}

#[test]
fn covering_scan_boundary_real_or_shape() {
    // The gap-c closer: a rowid-alias IN inside an OR over an analyzed
    // table — SQLite scans the covering index (expression semantics:
    // the boundary REAL -2^63 equals the i64::MIN row) in INDEX order.
    run(&[Case {
        name: "rowid IN + OR: covering scan, boundary row included",
        setup: &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT)",
            "CREATE INDEX t_a ON t(a)",
            "CREATE INDEX t_k ON t(k)",
            "CREATE INDEX t_kv ON t(k, v)",
            "CREATE INDEX t_id ON t(id)",
            "INSERT INTO t VALUES
             (1, 1, 10, 'x'), (2, 2, 10, 'y'), (3, 1, 20, 'z'),
             (-9223372036854775808, 3, 30, 'min'),
             (9223372036854775807, 1, 40, 'max'),
             (5, 2, 20, 'w')",
            "ANALYZE",
        ],
        query: "SELECT id FROM t WHERE oid IN (-9223372036854775808.0, 3) OR a = 1",
        plan: Some("SCAN t USING COVERING INDEX t_a"),
    }]);
}

#[test]
fn covering_scan_gates_match_sqlite() {
    run(&[
        Case {
            name: "DESC index scans in declared order (col DESC, rowid ASC)",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, pad TEXT)",
                "CREATE INDEX td ON t(a DESC)",
                "INSERT INTO t VALUES (1, 3, 'x'), (2, 1, 'y'), (3, 2, 'z'), (4, 3, 'w')",
                "ANALYZE",
            ],
            query: "SELECT a FROM t",
            plan: Some("SCAN t USING COVERING INDEX td"),
        },
        Case {
            name: "mixed ASC/DESC compound index scans in declared order",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, pad TEXT)",
                "CREATE INDEX tm ON t(a ASC, b DESC)",
                "INSERT INTO t VALUES (1, 1, 10, 'x'), (2, 1, 20, 'y'), (3, 2, 10, 'z'), (4, 2, 20, 'w'), (5, 0, 99, 'v')",
                "ANALYZE",
            ],
            query: "SELECT a, b FROM t",
            plan: Some("SCAN t USING COVERING INDEX tm"),
        },
        Case {
            name: "wide index loses the width gate (szIdxRow >= szTabRow)",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)",
                "CREATE INDEX tw ON t(a, id, id, id, id)",
                "INSERT INTO t VALUES (1, 3), (2, 1), (3, 2)",
                "ANALYZE",
            ],
            query: "SELECT a FROM t",
            plan: None,
        },
        // A partial index with an UNCONSTRAINED query: no WHERE term
        // implies its predicate, so it is not usable (SQLite's
        // whereUsablePartialIndex needs an implying term) — table scan.
        Case {
            name: "partial index needs an implying WHERE term",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, v TEXT)",
                "CREATE INDEX tp ON t(a) WHERE a IS NOT NULL",
                "INSERT INTO t VALUES (1, 3, 'x'), (2, 1, 'y'), (3, 2, 'z')",
                "ANALYZE",
            ],
            query: "SELECT a FROM t",
            plan: Some("SCAN t"),
        },
        // A COVERING partial index (the query reads only indexed
        // columns and the WHERE carries the partial's predicate).
        Case {
            name: "covering partial index with its WHERE term present",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, v TEXT)",
                "CREATE INDEX tp ON t(a) WHERE a IS NOT NULL",
                "INSERT INTO t VALUES (1, 3, 'x'), (2, 1, 'y'), (3, 2, 'z')",
                "ANALYZE",
            ],
            query: "SELECT a FROM t WHERE a IS NOT NULL AND a > 0",
            plan: None,
        },
        // The non-covering partial corner (an unindexed column read):
        // SQLite prices its lookup-loaded scan against the table scan
        // within the row-estimate noise — the engine keeps the table
        // scan there (ledger'd residual); rows still agree by SET.
        Case {
            name: "non-covering partial keeps the table scan (rows by set)",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, v TEXT)",
                "CREATE INDEX tp ON t(a) WHERE v = 'hot'",
                "INSERT INTO t VALUES (1, 3, 'hot'), (2, 1, 'cold'), (3, 2, 'hot')",
                "ANALYZE",
            ],
            query: "SELECT a FROM t WHERE v = 'hot' AND length(v) > 0 ORDER BY a",
            plan: None,
        },
        Case {
            name: "NOCASE index: folded order, original values",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT COLLATE NOCASE, pad TEXT)",
                "CREATE INDEX tn ON t(a)",
                "INSERT INTO t VALUES (1, 'b', 'x'), (2, 'A', 'y'), (3, 'C', 'z'), (4, 'a', 'w')",
                "ANALYZE",
            ],
            query: "SELECT a FROM t",
            plan: None,
        },
        Case {
            name: "INDEXED BY freezes the variant (walk the hinted index)",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT)",
                "CREATE INDEX t_a ON t(a)",
                "CREATE INDEX t_k ON t(k)",
                "INSERT INTO t VALUES (1, 3, 10, 'x'), (2, 1, 20, 'y'), (3, 3, 10, 'z')",
                "ANALYZE",
            ],
            query: "SELECT id FROM t INDEXED BY t_k",
            plan: None,
        },
    ]);
}

#[test]
fn covering_scan_update_delete_keep_the_table_scan() {
    // One-pass DML never takes the covering-index scan (SQLite's
    // WHERE_ONEPASS_DESIRED gate): the WHERE'd DML stays on the table
    // b-tree and its semantics are unchanged.
    let mut db = Database::open_in_memory().unwrap();
    let rc = rusqlite::Connection::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, v TEXT)",
        "CREATE INDEX t_a ON t(a)",
        "INSERT INTO t VALUES (1, 3, 'x'), (2, 1, 'y'), (3, 2, 'z'), (5, 1, 'w')",
        "ANALYZE",
    ] {
        db.execute(s, []).unwrap();
        rc.execute_batch(s).unwrap();
    }
    // UPDATE by the indexed column: both engines must agree on the
    // resulting table (order of the post-update scan included).
    db.execute("UPDATE t SET v = v || '!' WHERE a = 1", [])
        .unwrap();
    rc.execute_batch("UPDATE t SET v = v || '!' WHERE a = 1")
        .unwrap();
    assert_eq!(
        engine_rows(&db, "SELECT a FROM t"),
        sqlite_rows(&rc, "SELECT a FROM t")
    );
    assert_eq!(
        engine_rows(&db, "SELECT id, v FROM t ORDER BY id"),
        sqlite_rows(&rc, "SELECT id, v FROM t ORDER BY id")
    );
    db.execute("DELETE FROM t WHERE a = 2", []).unwrap();
    rc.execute_batch("DELETE FROM t WHERE a = 2").unwrap();
    assert_eq!(
        engine_rows(&db, "SELECT a FROM t"),
        sqlite_rows(&rc, "SELECT a FROM t")
    );
    assert_eq!(
        engine_rows(&db, "SELECT count(*) FROM t"),
        sqlite_rows(&rc, "SELECT count(*) FROM t")
    );
}

#[test]
fn covering_scan_null_keys_and_empty_index() {
    // A FULL index scan includes the NULL-keyed entries (an index
    // range without a lower bound skips them — a full scan must not).
    run(&[
        Case {
            name: "NULL keys sort first in the covering scan",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, pad TEXT)",
                "CREATE INDEX ta ON t(a)",
                "INSERT INTO t VALUES (1, 3, 'x'), (2, NULL, 'y'), (3, 1, 'z'), (4, NULL, 'w')",
                "ANALYZE",
            ],
            query: "SELECT a FROM t",
            plan: Some("SCAN t USING COVERING INDEX ta"),
        },
        Case {
            name: "empty table through the covering scan",
            setup: &[
                "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, pad TEXT)",
                "CREATE INDEX ta ON t(a)",
                "ANALYZE",
            ],
            query: "SELECT a FROM t",
            plan: Some("SCAN t USING COVERING INDEX ta"),
        },
    ]);
}
