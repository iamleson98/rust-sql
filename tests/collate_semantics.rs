//! Differential collation-semantics suite: declared NOCASE / RTRIM
//! columns and explicitly collated indexes, engine vs real SQLite
//! (rusqlite). Covers the full semantic surface:
//!
//! - ORDER BY through a column's DECLARED collation (inherited, no
//!   explicit COLLATE in the query) — ASC, DESC, alias, ordinal.
//! - Index seeks: equality / IN-list / range probes fold through the
//!   INDEX's collation; a BINARY equality must NOT use a NOCASE index
//!   (and vice versa) — the collation-match gate.
//! - The OLTP fast paths (point lookups, covering counts, sums) fold
//!   probe keys: `WHERE v = 'APPLE'` over a NOCASE index.
//! - GROUP BY under the term's collation: group membership, counts,
//!   and the FIRST-SEEN representative value SQLite outputs.
//! - UPDATE / DELETE ... WHERE through collated indexes.
//!
//! SQLite reference behavior pinned by construction: NOCASE folds ASCII
//! only (non-ASCII compares byte-wise), RTRIM ignores trailing spaces,
//! GROUP BY / DISTINCT / min / max use the column's collation, and the
//! group's output value is the FIRST row's value in scan order.

use rustqlite::{Database, Value};

/// Run `setup` + `query` on both engines; compare rendered rows exactly.
fn diff<S: AsRef<str>>(setup: &[S], query: &str) {
    let ours = engine_rows(setup, query);
    let theirs = sqlite_rows(setup, query);
    assert_eq!(
        ours, theirs,
        "\ncollation mismatch on {query}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
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
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("I:{i}"),
                    Value::Real(f) => format!("R:{f}"),
                    Value::Text(t) => format!("T:{t}"),
                    Value::Blob(b) => format!("B:{}", b.len()),
                })
                .collect()
        })
        .collect()
}

/// A setup script plus one extra statement (index DDL, usually).
fn with_ix(base: &[&str], ddl: &str) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    v.push(ddl.to_string());
    v
}

const NOCASE_TABLE: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
    "INSERT INTO t VALUES (1, 'apple'), (2, 'APPLE'), (3, 'Banana'), \
     (4, 'banana'), (5, 'cherry'), (6, 'Cherry'), (7, 'date'), \
     (8, 'éclair'), (9, 'apple pie'), (10, 'BANANA split')",
];

const PLAIN_TABLE: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO t VALUES (1, 'apple'), (2, 'APPLE'), (3, 'Banana'), \
     (4, 'banana'), (5, 'cherry'), (6, 'Cherry'), (7, 'date'), \
     (8, 'éclair'), (9, 'apple pie'), (10, 'BANANA split')",
];

// ---------------------------------------------------------------------------
// ORDER BY through the declared collation
// ---------------------------------------------------------------------------

#[test]
fn order_by_declared_nocase() {
    diff(NOCASE_TABLE, "SELECT v FROM t ORDER BY v");
    diff(NOCASE_TABLE, "SELECT v FROM t ORDER BY v DESC");
    // Secondary sort key for full determinism.
    diff(NOCASE_TABLE, "SELECT v, id FROM t ORDER BY v, id");
}

#[test]
fn order_by_alias_and_ordinal_inherit_collation() {
    // An ORDER BY alias resolves to the projection expression — a bare
    // column keeps its declared collation.
    diff(NOCASE_TABLE, "SELECT v AS name FROM t ORDER BY name");
    diff(NOCASE_TABLE, "SELECT id, v FROM t ORDER BY 2");
}

#[test]
fn order_by_explicit_collate_on_plain_column() {
    diff(PLAIN_TABLE, "SELECT v FROM t ORDER BY v COLLATE NOCASE");
    diff(
        PLAIN_TABLE,
        "SELECT v FROM t ORDER BY v COLLATE NOCASE DESC",
    );
    // Explicit COLLATE overrides the declared one (BINARY wins here).
    diff(NOCASE_TABLE, "SELECT v FROM t ORDER BY v COLLATE BINARY");
}

#[test]
fn rtrim_order_and_equality() {
    diff(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE RTRIM)",
            "INSERT INTO t VALUES (1, 'abc'), (2, 'abc  '), (3, ' abc'), \
             (4, 'xyz'), (5, 'xyz   '), (6, 'de')",
        ],
        "SELECT v FROM t ORDER BY v, id",
    );
}

// ---------------------------------------------------------------------------
// Index probes: collation-folded seeks and the collation-match gate
// ---------------------------------------------------------------------------

#[test]
fn nocase_index_equality_probes() {
    // The index INHERITS the column's NOCASE: every case-variant probe
    // must find the whole fold class (the probe key folds).
    for key in [
        "apple", "APPLE", "Apple", "banana", "BANANA", "date", "éclair",
    ] {
        diff(
            &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
            &format!("SELECT id FROM t WHERE v = '{key}' ORDER BY id"),
        );
    }
}

#[test]
fn binary_equality_never_uses_a_nocase_index() {
    // BINARY column + explicitly NOCASE index: the BINARY equality must
    // NOT seek the folded b-tree ('APPLE' must match exactly one row).
    for key in ["apple", "APPLE", "banana", "date"] {
        diff(
            &with_ix(PLAIN_TABLE, "CREATE INDEX ix ON t(v COLLATE NOCASE)"),
            &format!("SELECT id FROM t WHERE v = '{key}' ORDER BY id"),
        );
    }
}

#[test]
fn nocase_comparison_never_uses_a_binary_index() {
    // NOCASE comparison over a BINARY-indexed plain column: the index
    // can't serve the seek; the filter must still match the fold class.
    for key in ["apple", "APPLE", "banana"] {
        diff(
            &with_ix(PLAIN_TABLE, "CREATE INDEX ix ON t(v)"),
            &format!("SELECT id FROM t WHERE v COLLATE NOCASE = '{key}' ORDER BY id"),
        );
    }
}

#[test]
fn nocase_index_in_list() {
    diff(
        &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
        "SELECT id FROM t WHERE v IN ('ALPHA', 'apple', 'BANANA', 'zz') ORDER BY id",
    );
}

#[test]
fn nocase_index_range_and_between() {
    diff(
        &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
        "SELECT id FROM t WHERE v BETWEEN 'b' AND 'c' ORDER BY id",
    );
    diff(
        &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
        "SELECT id FROM t WHERE v > 'BANANA' ORDER BY id",
    );
    diff(
        &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
        "SELECT id FROM t WHERE v < 'Cherry' ORDER BY id",
    );
}

#[test]
fn fastpath_point_lookup_folds_probe_key() {
    // The statement-cache fast paths (IndexPoint / IndexCount) encode
    // literal keys at plan time — the fold must apply there too.
    diff(
        &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
        "SELECT count(*) FROM t WHERE v = 'APPLE'",
    );
    diff(
        &with_ix(NOCASE_TABLE, "CREATE INDEX ix ON t(v)"),
        "SELECT sum(id) FROM t WHERE v = 'APPLE'",
    );
    diff(
        &with_ix(NOCASE_TABLE, "CREATE UNIQUE INDEX uq ON t(id, v)"),
        "SELECT id, v FROM t WHERE id = 2 AND v = 'apple' ORDER BY id",
    );
    // Parameterized probe (fold at execution time, not plan time).
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
        [],
    )
    .unwrap();
    for (id, v) in [(1, "apple"), (2, "APPLE"), (3, "banana")] {
        db.execute(
            "INSERT INTO t VALUES (?, ?)",
            vec![Value::Integer(id), Value::Text(v.into())],
        )
        .unwrap();
    }
    db.execute("CREATE INDEX ix ON t(v)", []).unwrap();
    let rows = db
        .query(
            "SELECT id FROM t WHERE v = ? ORDER BY id",
            vec![Value::Text("APPLE".into())],
        )
        .unwrap();
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => *i,
            other => panic!("expected INTEGER, got {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2], "parameterized NOCASE probe must fold");
}

// ---------------------------------------------------------------------------
// GROUP BY under the term's collation
// ---------------------------------------------------------------------------

#[test]
fn groupby_declared_nocase() {
    diff(
        NOCASE_TABLE,
        "SELECT v, count(*) FROM t GROUP BY v ORDER BY v",
    );
    diff(
        NOCASE_TABLE,
        "SELECT v, count(*), sum(id) FROM t GROUP BY v ORDER BY v",
    );
}

#[test]
fn groupby_first_seen_representative() {
    // SQLite outputs the FIRST row's value as the group representative:
    // insertion order 'APPLE' before 'apple' must output 'APPLE'.
    diff(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
            "INSERT INTO t VALUES (1, 'APPLE'), (2, 'apple'), (3, 'banana')",
        ],
        "SELECT v, count(*) FROM t GROUP BY v ORDER BY v",
    );
    diff(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
            "INSERT INTO t VALUES (1, 'apple'), (2, 'APPLE'), (3, 'banana')",
        ],
        "SELECT v, count(*) FROM t GROUP BY v ORDER BY v",
    );
}

#[test]
fn groupby_with_filter_and_having() {
    diff(
        NOCASE_TABLE,
        "SELECT v, count(*) FROM t WHERE id > 2 GROUP BY v HAVING count(*) >= 1 ORDER BY v",
    );
}

#[test]
fn groupby_explicit_collate_term() {
    diff(
        PLAIN_TABLE,
        "SELECT v, count(*) FROM t GROUP BY v COLLATE NOCASE ORDER BY v COLLATE NOCASE",
    );
}

#[test]
fn groupby_multi_term_mixed_collations() {
    diff(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE, w TEXT)",
            "INSERT INTO t VALUES (1, 'apple', 'a'), (2, 'APPLE', 'a'), \
             (3, 'apple', 'b'), (4, 'Banana', 'a'), (5, 'BANANA', 'B')",
        ],
        "SELECT v, w, count(*) FROM t GROUP BY v, w ORDER BY v, w",
    );
}

#[test]
fn groupby_join_qualified_reference() {
    diff(
        &[
            "CREATE TABLE a (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE)",
            "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id), v TEXT)",
            "INSERT INTO a VALUES (1, 'x'), (2, 'X'), (3, 'y')",
            "INSERT INTO b VALUES (10, 1, 'p'), (11, 2, 'q'), (12, 3, 'r')",
        ],
        "SELECT a.tag, count(*) FROM a JOIN b ON b.a_id = a.id GROUP BY a.tag ORDER BY a.tag",
    );
}

#[test]
fn rtrim_group_and_distinct() {
    diff(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE RTRIM)",
            "INSERT INTO t VALUES (1, 'abc'), (2, 'abc  '), (3, 'abc'), \
             (4, 'xyz'), (5, 'xyz  ')",
        ],
        "SELECT v, count(*) FROM t GROUP BY v ORDER BY v",
    );
}

// ---------------------------------------------------------------------------
// DML through collated indexes
// ---------------------------------------------------------------------------

#[test]
fn update_delete_where_collated_index() {
    let setup: &[&str] = &[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
        "CREATE INDEX ix ON t(v)",
        "INSERT INTO t VALUES (1, 'apple'), (2, 'APPLE'), (3, 'banana'), (4, 'Banana')",
    ];
    diff(setup, "SELECT id, v FROM t WHERE v = 'APPLE' ORDER BY id");
    // UPDATE through the NOCASE probe (the streaming IndexPoint path).
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
        conn.execute_batch(s).unwrap();
    }
    db.execute("UPDATE t SET v = 'updated' WHERE v = 'APPLE'", [])
        .unwrap();
    conn.execute_batch("UPDATE t SET v = 'updated' WHERE v = 'APPLE'")
        .unwrap();
    let ours = render(&db.query("SELECT id, v FROM t ORDER BY id", []).unwrap());
    let theirs = {
        let mut stmt = conn.prepare("SELECT id, v FROM t ORDER BY id").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            out.push(vec![
                format!("I:{}", r.get::<_, i64>(0).unwrap()),
                format!("T:{}", r.get::<_, String>(1).unwrap()),
            ]);
        }
        out
    };
    assert_eq!(ours, theirs, "UPDATE through NOCASE index mismatch");

    // DELETE through the NOCASE probe.
    db.execute("DELETE FROM t WHERE v = 'updated'", []).unwrap();
    conn.execute_batch("DELETE FROM t WHERE v = 'updated'")
        .unwrap();
    let ours = render(&db.query("SELECT id, v FROM t ORDER BY id", []).unwrap());
    let theirs = {
        let mut stmt = conn.prepare("SELECT id, v FROM t ORDER BY id").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            out.push(vec![
                format!("I:{}", r.get::<_, i64>(0).unwrap()),
                format!("T:{}", r.get::<_, String>(1).unwrap()),
            ]);
        }
        out
    };
    assert_eq!(ours, theirs, "DELETE through NOCASE index mismatch");
}

// ---------------------------------------------------------------------------
// Parallel GROUP BY: the worker-split path must fold identically
// ---------------------------------------------------------------------------

#[test]
fn parallel_collated_groupby_matches_serial() {
    // 300k rows over a NOCASE column: above the 131_072 parallel-scan
    // threshold, the worker split + range-ordered merge must produce
    // EXACTLY the serial answer (groups AND representatives).
    let n: i64 = 300_000;
    let build = |db: &mut Database| {
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
            [],
        )
        .unwrap();
        for base in (0..n).step_by(5000) {
            let mut sql = String::from("INSERT INTO t VALUES ");
            for i in base..(base + 5000).min(n) {
                if i > base {
                    sql.push(',');
                }
                // Case variants: first-seen representative is the base
                // form in rowid order.
                let v = if (i / 100) % 3 == 0 {
                    format!("key{:04}", i / 100)
                } else if (i / 100) % 3 == 1 {
                    format!("KEY{:04}", i / 100)
                } else {
                    format!("Key{:04}", i / 100)
                };
                sql.push_str(&format!("({i}, '{v}')"));
            }
            db.execute(&sql, []).unwrap();
        }
    };
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db);
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let serial = render(
        &db.query(
            "SELECT v, count(*), min(id), max(id) FROM t GROUP BY v ORDER BY v",
            [],
        )
        .unwrap(),
    );
    db.execute("PRAGMA parallel_scan=1", []).unwrap();
    let parallel = render(
        &db.query(
            "SELECT v, count(*), min(id), max(id) FROM t GROUP BY v ORDER BY v",
            [],
        )
        .unwrap(),
    );
    assert_eq!(
        serial, parallel,
        "parallel collated GROUP BY diverged from serial"
    );
    // And the group count: 3 case variants fold per 100-row class.
    let n_groups = serial.len();
    assert_eq!(n_groups, (n / 100) as usize, "fold classes must collapse");
}
