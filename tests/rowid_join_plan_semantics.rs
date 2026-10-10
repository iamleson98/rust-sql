//! Rowid-alias equi-JOINS follow SQLite's plan choice.
//!
//! `A JOIN B ON A.id = B.x` (A.id an INTEGER PRIMARY KEY) has two answers
//! in SQLite when B.x holds the boundary REAL -9.2233720368547758e18 and A
//! holds the rowid i64::MIN: a plan that SEEKS A's rowid with B's value
//! (OP_SeekRowid, ticket #3922) finds nothing, while a plan that scans A
//! — or probes an index / automatic index on B.x — compares with `=` and
//! matches. Which plan SQLite runs depends on stat1 sizes, indexes, and
//! whether a scan order serves the ORDER BY (stateful-fuzz seed 8008: an
//! analyzed 7-row table joined to a 1000-row estimate under ORDER BY
//! a.rowid, b.rowid LIMIT 200 scans both). This sweep runs boundary-value
//! joins over table sizes, stats states, index shapes on both sides, key
//! affinities, ORDER BY / LIMIT / GROUP BY / filter shapes and FROM order,
//! and requires every answer to equal the bundled SQLite's.

use rusqlite::types::Value as Sv;
use rusqlite::Connection;
use rustqlite::{Database, Value};

fn norm_ours(rows: Vec<Vec<Value>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("I{i}"),
                    Value::Real(f) => format!("R{f:?}"),
                    Value::Text(t) => format!("T{}", t.as_str()),
                    Value::Blob(b) => format!("B{b:?}"),
                })
                .collect()
        })
        .collect()
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = rc
        .prepare(sql)
        .unwrap_or_else(|e| panic!("sqlite prepare {sql}: {e}"));
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        out.push(
            (0..n)
                .map(|i| match row.get::<_, Sv>(i).unwrap() {
                    Sv::Null => "NULL".to_string(),
                    Sv::Integer(i) => format!("I{i}"),
                    Sv::Real(f) => format!("R{f:?}"),
                    Sv::Text(t) => format!("T{t}"),
                    Sv::Blob(b) => format!("B{b:?}"),
                })
                .collect(),
        );
    }
    out
}

fn both(db: &mut Database, rc: &Connection, sql: &str) {
    db.execute(sql, [])
        .unwrap_or_else(|e| panic!("ours {sql}: {e}"));
    rc.execute_batch(sql)
        .unwrap_or_else(|e| panic!("sqlite {sql}: {e}"));
}

#[derive(Clone, Copy, Debug)]
enum Stats {
    None,
    Analyzed,
    /// ANALYZE, then the indexes (no stat1 for them — the tables' row
    /// estimates floor at 1000).
    IndexesAfter,
}

/// (name, index DDL on a / b)
const SHAPES: &[(&str, &[&str])] = &[
    ("none", &[]),
    ("b_x", &["CREATE INDEX bx ON b(x)"]),
    ("b_x_unique", &["CREATE UNIQUE INDEX bx ON b(x)"]),
    ("b_y", &["CREATE INDEX by_ ON b(y)"]),
    ("a_w", &["CREATE INDEX aw ON a(w)"]),
    ("a_id", &["CREATE INDEX aid ON a(id DESC)"]),
    (
        "a_w+b_x",
        &["CREATE INDEX aw ON a(w)", "CREATE INDEX bx ON b(x)"],
    ),
    // Partial indexes the join equality implies (`x IS NOT NULL`) — and
    // one it does not.
    (
        "b_x_partial",
        &["CREATE INDEX bx ON b(x) WHERE x IS NOT NULL"],
    ),
    (
        "a_id_partial",
        &["CREATE UNIQUE INDEX aid ON a(id) WHERE id IS NOT NULL"],
    ),
    ("b_y_partial", &["CREATE INDEX by_ ON b(y) WHERE y > 'y1'"]),
];

const QUERIES: &[&str] = &[
    "SELECT a.rowid, b.rowid FROM a JOIN b ON a.id = b.x ORDER BY 1, 2 LIMIT 200",
    "SELECT a.rowid, b.rowid FROM b JOIN a ON b.x = a.id ORDER BY 1, 2 LIMIT 200",
    "SELECT a.rowid, b.rowid FROM a JOIN b ON a.id = b.x ORDER BY 2, 1",
    "SELECT b.rowid, a.rowid FROM b, a WHERE a.id = b.x ORDER BY 1, 2",
    "SELECT count(*) FROM a JOIN b ON a.id = b.x",
    "SELECT a.v, b.y, a.rowid, b.rowid FROM a JOIN b ON a.id = b.x ORDER BY a.v, b.y, 3, 4",
    "SELECT a.id, b.z FROM a JOIN b ON a.id = b.x",
    "SELECT a.rowid, b.rowid FROM a JOIN b ON a.id = b.x WHERE b.z > 1 ORDER BY 1, 2",
    "SELECT a.rowid, b.rowid FROM a JOIN b ON a.id = b.x WHERE a.v > 'a' ORDER BY 2, 1 LIMIT 3",
    "SELECT b.z, count(*) FROM a JOIN b ON a.id = b.x GROUP BY b.z ORDER BY 1",
];

#[test]
fn rowid_join_boundary_key_follows_sqlite_plan_choice() {
    let sizes = [1usize, 2, 3, 5, 8, 13, 21, 40, 100, 300];
    let mut checked = 0usize;
    let mut matched = 0usize;
    let mut missed = 0usize;
    for key_type in ["REAL", "INTEGER", "TEXT"] {
        for &(shape, ddl) in SHAPES {
            for stats in [Stats::None, Stats::Analyzed, Stats::IndexesAfter] {
                if matches!(stats, Stats::IndexesAfter) && ddl.is_empty() {
                    continue;
                }
                for (si, &na) in sizes.iter().enumerate() {
                    // Pair each A size with a few B sizes (small, equal,
                    // large) rather than the full grid.
                    for &nb in &[
                        sizes[(si + 3) % sizes.len()],
                        na,
                        sizes[(si + 7) % sizes.len()],
                    ] {
                        let mut db = Database::open_in_memory().unwrap();
                        let rc = Connection::open_in_memory().unwrap();
                        both(
                            &mut db,
                            &rc,
                            "CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT, w INTEGER)",
                        );
                        both(
                            &mut db,
                            &rc,
                            &format!("CREATE TABLE b (x {key_type}, y TEXT, z INTEGER)"),
                        );
                        let mut ins =
                            String::from("INSERT INTO a VALUES (-9223372036854775808, 'min', 0)");
                        for i in 1..na {
                            ins.push_str(&format!(", ({i}, 'v{}', {})", i % 5, i % 3));
                        }
                        both(&mut db, &rc, &ins);
                        let mut ins = String::from("INSERT INTO b VALUES ");
                        for j in 0..nb {
                            if j > 0 {
                                ins.push_str(", ");
                            }
                            // Every third key is the boundary REAL; the rest
                            // hit (or miss) ordinary rowids. Under a UNIQUE
                            // index the keys must be distinct: one boundary
                            // key, then 1, 2, 3, ...
                            let unique = ddl.iter().any(|d| d.contains("UNIQUE"));
                            let x = if j == 0 || (!unique && j % 3 == 0) {
                                "-9.2233720368547758e18".to_string()
                            } else if unique {
                                format!("{j}")
                            } else {
                                format!("{}", (j * 7) % (na + 2))
                            };
                            ins.push_str(&format!("({x}, 'y{}', {})", j % 4, j % 3));
                        }
                        both(&mut db, &rc, &ins);
                        match stats {
                            Stats::None => {
                                for d in ddl {
                                    both(&mut db, &rc, d);
                                }
                            }
                            Stats::Analyzed => {
                                for d in ddl {
                                    both(&mut db, &rc, d);
                                }
                                both(&mut db, &rc, "ANALYZE");
                            }
                            Stats::IndexesAfter => {
                                both(&mut db, &rc, "ANALYZE");
                                for d in ddl {
                                    both(&mut db, &rc, d);
                                }
                            }
                        }
                        for q in QUERIES {
                            let mut ours = norm_ours(
                                db.query(q, []).unwrap_or_else(|e| panic!("ours {q}: {e}")),
                            );
                            let mut theirs = sqlite_rows(&rc, q);
                            if !q.contains("ORDER BY") {
                                ours.sort();
                                theirs.sort();
                            }
                            assert_eq!(
                                ours, theirs,
                                "[key={key_type} shape={shape} stats={stats:?} a={na} b={nb}] {q}"
                            );
                            checked += 1;
                            if q.starts_with("SELECT a.rowid, b.rowid FROM a JOIN b ON a.id = b.x ORDER BY 1, 2 LIMIT")
                                && key_type == "REAL"
                            {
                                if theirs.iter().any(|r| r[0] == "I-9223372036854775808") {
                                    matched += 1;
                                } else {
                                    missed += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // Both plan families must occur, or the sweep pins nothing.
    assert!(
        matched > 0 && missed > 0,
        "plan mix: boundary matched {matched}x, sought past {missed}x ({checked} statements)"
    );
}
