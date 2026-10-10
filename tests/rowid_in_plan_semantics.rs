//! Rowid IN-list semantics follow SQLite's PLAN CHOICE.
//!
//! `id IN (..., -9.2233720368547758e18)` on an INTEGER PRIMARY KEY has two
//! answers in SQLite, and which one a statement gets depends on the plan:
//! the IPK IN-loop seeks each member through OP_SeekRowid (the boundary
//! REAL -2^63 converts to no rowid — ticket #3922 — and matches nothing),
//! while a full scan or a covering index evaluates the IN with the general
//! comparison (Real(-2^63) == Integer(i64::MIN) — the i64::MIN row
//! matches). SQLite picks between them with its cost model: table size
//! from sqlite_stat1 (or the ~1M-row default), STAT4 presence, list
//! length, the covering-ness of indexes led by the alias column, and
//! whether the statement is one-pass DML.
//!
//! This sweep runs the boundary-member IN through SELECT / UPDATE /
//! DELETE shapes over tables of many sizes, analyzed and not, with and
//! without alias / covering / non-covering indexes, and requires every
//! answer to match the bundled SQLite row for row.

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
    /// ANALYZE, then an index created afterwards (no stat1 for it — the
    /// table's estimate floors at 1000 rows).
    IndexAfterAnalyze,
}

const INDEXES: &[(&str, &str)] = &[
    ("none", ""),
    ("alias", "CREATE INDEX ix_id ON t(id DESC)"),
    ("alias_unique", "CREATE UNIQUE INDEX ix_id ON t(id)"),
    ("covering_a", "CREATE INDEX ix_a ON t(a)"),
    ("wide_b", "CREATE INDEX ix_b ON t(b)"),
];

const LISTS: &[&str] = &[
    "-9.2233720368547758e18, 3",
    "x'00', 'zz', -9.2233720368547758e18",
    "-9.2233720368547758e18, 2, 3, 5",
    "'-9223372036854775809', 7, 11, 2, 3",
    "-9.2233720368547758e18, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11",
];

const SELECTS: &[&str] = &[
    "SELECT * FROM t WHERE id IN ({L}) ORDER BY id",
    "SELECT count(*) FROM t WHERE id IN ({L})",
    "SELECT id FROM t WHERE id IN ({L}) ORDER BY id",
    "SELECT a FROM t WHERE id IN ({L}) ORDER BY 1",
    "SELECT id, b FROM t WHERE id IN ({L}) ORDER BY id",
    "SELECT b FROM t WHERE id IN ({L}) ORDER BY id DESC",
    "SELECT a FROM t WHERE id IN ({L}) ORDER BY a, b",
    "SELECT a, b, id FROM t WHERE id IN ({L}) ORDER BY 1",
    "SELECT DISTINCT a FROM t WHERE id IN ({L}) ORDER BY 1",
    "SELECT DISTINCT b FROM t WHERE id IN ({L})",
    "SELECT a, count(*) FROM t WHERE id IN ({L}) GROUP BY a ORDER BY 1",
    "SELECT max(a) FROM t WHERE id IN ({L})",
    "SELECT b FROM t WHERE id IN ({L})",
];

const DMLS: &[&str] = &[
    "UPDATE t SET b = 'u' WHERE id IN ({L})",
    "UPDATE t SET a = a + 100 WHERE id IN ({L})",
    "DELETE FROM t WHERE id IN ({L})",
];

#[test]
fn rowid_in_boundary_member_follows_sqlite_plan_choice() {
    let sizes = [1usize, 2, 3, 4, 5, 6, 8, 11, 16, 20, 28, 40, 64, 100, 300];
    let mut checked = 0usize;
    let mut matched_boundary = 0usize;
    let mut skipped_boundary = 0usize;
    for &(idx_name, idx_sql) in INDEXES {
        for stats in [Stats::None, Stats::Analyzed, Stats::IndexAfterAnalyze] {
            if matches!(stats, Stats::IndexAfterAnalyze) && idx_sql.is_empty() {
                continue;
            }
            for &n in &sizes {
                let mut db = Database::open_in_memory().unwrap();
                let rc = Connection::open_in_memory().unwrap();
                both(
                    &mut db,
                    &rc,
                    "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)",
                );
                let mut ins = String::from(
                    "INSERT INTO t VALUES (-9223372036854775808, 0, 'min')",
                );
                for i in 1..n {
                    ins.push_str(&format!(", ({i}, {}, 'r{i}')", i % 7));
                }
                both(&mut db, &rc, &ins);
                match stats {
                    Stats::None => {
                        if !idx_sql.is_empty() {
                            both(&mut db, &rc, idx_sql);
                        }
                    }
                    Stats::Analyzed => {
                        if !idx_sql.is_empty() {
                            both(&mut db, &rc, idx_sql);
                        }
                        both(&mut db, &rc, "ANALYZE");
                    }
                    Stats::IndexAfterAnalyze => {
                        both(&mut db, &rc, "ANALYZE");
                        both(&mut db, &rc, idx_sql);
                    }
                }
                for list in LISTS {
                    for q in SELECTS {
                        let sql = q.replace("{L}", list);
                        let mut ours = norm_ours(
                            db.query(&sql, [])
                                .unwrap_or_else(|e| panic!("ours {sql}: {e}")),
                        );
                        let mut theirs = sqlite_rows(&rc, &sql);
                        // Without ORDER BY the row order is the plan's own
                        // business — compare as multisets.
                        if !sql.contains("ORDER BY") {
                            ours.sort();
                            theirs.sort();
                        }
                        assert_eq!(
                            ours, theirs,
                            "[index={idx_name} stats={stats:?} rows={n}] {sql}"
                        );
                        checked += 1;
                        // Which plan family answered: the i64::MIN row
                        // is reachable only through the general
                        // comparison of the boundary member.
                        if sql.starts_with("SELECT id FROM") {
                            if theirs.iter().any(|r| r[0] == "I-9223372036854775808") {
                                matched_boundary += 1;
                            } else {
                                skipped_boundary += 1;
                            }
                        }
                    }
                    for q in DMLS {
                        let sql = q.replace("{L}", list);
                        both(&mut db, &rc, "BEGIN");
                        let a = db.execute(&sql, []).map_err(|e| e.to_string());
                        let b = rc.execute(&sql, []).map_err(|e| e.to_string());
                        assert_eq!(
                            a.is_ok(),
                            b.is_ok(),
                            "[index={idx_name} stats={stats:?} rows={n}] {sql}: {a:?} vs {b:?}"
                        );
                        let dump = "SELECT * FROM t ORDER BY id";
                        let ours = norm_ours(db.query(dump, []).unwrap());
                        let theirs = sqlite_rows(&rc, dump);
                        assert_eq!(
                            ours, theirs,
                            "[index={idx_name} stats={stats:?} rows={n}] state after {sql}"
                        );
                        both(&mut db, &rc, "ROLLBACK");
                        checked += 1;
                    }
                }
            }
        }
    }
    // Both plan families must actually occur, or the sweep pins nothing.
    assert!(
        matched_boundary > 0 && skipped_boundary > 0,
        "plan mix: boundary matched {matched_boundary}x, sought-past {skipped_boundary}x \
         ({checked} statements)"
    );
}
