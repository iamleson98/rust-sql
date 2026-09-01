//! Differential tests for `UPDATE ... FROM` (SQLite 3.33+) and
//! collations (NOCASE / RTRIM): rustqlite vs SQLite (rusqlite) must
//! agree on all observable outcomes — row contents, change counts,
//! constraint errors, index lookups, ORDER BY results.

use rustqlite::Database;

/// Does the statement produce rows (RETURNING / SELECT)? rusqlite's
/// `execute` rejects row-returning statements ("did you mean to call
/// query?") — those must go through the query path on BOTH engines.
fn is_row_returning(s: &str) -> bool {
    let upper = s.to_ascii_uppercase();
    if upper.contains("RETURNING") {
        return true;
    }
    let t = s.trim_start();
    t.len() >= 6 && t[..6].eq_ignore_ascii_case("SELECT")
}

/// Run identical SQL programs on both engines; after each statement in
/// `check_at` positions, compare the full table contents.
fn diff_program(setup: &[&str], program: &[&str], check: &str) {
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for group in [setup, program] {
        for s in group {
            if is_row_returning(s) {
                // Row-returning DML: compare the RETURNING rows too.
                let ours = db.query(s, []).map(|r| render_plain(&r));
                let theirs: Result<Vec<Vec<String>>, rusqlite::Error> = {
                    let mut stmt = conn.prepare(s).unwrap();
                    let ncols = stmt.column_count();
                    let mut rows = stmt.query([]).unwrap();
                    let mut out = Vec::new();
                    while let Some(r) = rows.next().unwrap() {
                        let mut row = Vec::with_capacity(ncols);
                        for i in 0..ncols {
                            let v = match r.get_ref(i).unwrap() {
                                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                                rusqlite::types::ValueRef::Integer(v) => format!("I:{v}"),
                                rusqlite::types::ValueRef::Real(v) => format!("R:{v}"),
                                rusqlite::types::ValueRef::Text(t) => format!("T:{}", String::from_utf8_lossy(t)),
                                rusqlite::types::ValueRef::Blob(b) => format!("B:{}", b.len()),
                            };
                            row.push(v);
                        }
                        out.push(row);
                    }
                    Ok(out)
                };
                match (ours, theirs) {
                    (Ok(a), Ok(b)) => assert_eq!(
                        a, b,
                        "\nRETURNING row mismatch on {s}\n  rustqlite: {a:#?}\n  sqlite:   {b:#?}"
                    ),
                    (Err(e), Err(te)) => {
                        let ours_msg = e.to_string();
                        let theirs_msg = te.to_string();
                        assert!(
                            ours_msg.starts_with(&theirs_msg[..theirs_msg.len().min(40)])
                                || theirs_msg.starts_with(&ours_msg[..ours_msg.len().min(40)]),
                            "error mismatch on {s}\n  rustqlite: {ours_msg}\n  sqlite:   {theirs_msg}"
                        );
                    }
                    (Err(e), Ok(_)) => panic!("rustqlite failed where SQLite succeeded on {s}: {e}"),
                    (Ok(_), Err(te)) => panic!("SQLite failed where rustqlite succeeded on {s}: {te}"),
                }
                continue;
            }
            let ours = db.execute(s, []).map(|_| ());
            let theirs = conn.execute(s, []).map(|_| ());
            match (ours, theirs) {
                (Ok(()), Ok(())) => {}
                (Err(e), Err(te)) => {
                    // Both rejected: the error MESSAGE must agree on the
                    // constraint shape (SQLite-exact prefixes).
                    let ours_msg = e.to_string();
                    let theirs_msg = te.to_string();
                    assert!(
                        ours_msg.starts_with(&theirs_msg[..theirs_msg.len().min(40)])
                            || theirs_msg.starts_with(&ours_msg[..ours_msg.len().min(40)]),
                        "error mismatch on {s}\n  rustqlite: {ours_msg}\n  sqlite:   {theirs_msg}"
                    );
                }
                (Err(e), Ok(())) => panic!("rustqlite failed where SQLite succeeded on {s}: {e}"),
                (Ok(()), Err(te)) => panic!("SQLite failed where rustqlite succeeded on {s}: {te}"),
            }
        }
    }
    let ours = render_rows(&mut db, check);
    let theirs = render_conn(&conn, check);
    assert_eq!(
        ours, theirs,
        "\nstate mismatch after program\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
    );
}

/// Render plain rows (type-tagged) for RETURNING comparison.
fn render_plain(rows: &[Vec<rustqlite::Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    rustqlite::Value::Null => "NULL".to_string(),
                    rustqlite::Value::Integer(i) => format!("I:{i}"),
                    rustqlite::Value::Real(f) => format!("R:{f}"),
                    rustqlite::Value::Text(t) => format!("T:{t}"),
                    rustqlite::Value::Blob(b) => format!("B:{}", b.len()),
                })
                .collect()
        })
        .collect()
}

/// Compare only the outcome class (Ok / Err message prefix) of one
/// statement, without table comparison (for error-only checks).
fn diff_error(setup: &[&str], stmt: &str) {
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    let ours = db.execute(stmt, []).err().map(|e| e.to_string());
    let theirs = conn.execute(stmt, []).err().map(|e| e.to_string());
    match (ours, theirs) {
        (None, None) => {}
        (Some(o), Some(t)) => {
            let n = o.len().min(t.len()).min(30);
            assert!(
                o.starts_with(&t[..n]) || t.starts_with(&o[..n]),
                "error mismatch on {stmt}\n  rustqlite: {o}\n  sqlite:   {t}"
            );
        }
        (Some(o), None) => panic!("rustqlite errored where SQLite succeeded on {stmt}: {o}"),
        (None, Some(t)) => panic!("SQLite errored where rustqlite succeeded on {stmt}: {t}"),
    }
}

fn render_rows(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    let rows = db.query(sql, []).unwrap();
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    rustqlite::Value::Null => "NULL".into(),
                    rustqlite::Value::Integer(i) => format!("I:{i}"),
                    rustqlite::Value::Real(f) => format!("R:{f}"),
                    rustqlite::Value::Text(t) => format!("T:{}", t.as_str()),
                    rustqlite::Value::Blob(b) => format!("B:{}", b.len()),
                })
                .collect()
        })
        .collect()
}

fn render_conn(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::with_capacity(n);
        for i in 0..n {
            let v = match r.get_ref(i).unwrap() {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(v) => format!("I:{v}"),
                rusqlite::types::ValueRef::Real(v) => format!("R:{v}"),
                rusqlite::types::ValueRef::Text(t) => format!("T:{}", String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(b) => format!("B:{}", b.len()),
            };
            row.push(v);
        }
        out.push(row);
    }
    out
}

// ===========================================================================
// UPDATE ... FROM
// ===========================================================================

const UF_SETUP: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
    "CREATE TABLE src (id INTEGER PRIMARY KEY, v TEXT)",
    "INSERT INTO t VALUES (1, 'old'), (2, 'keep'), (3, 'stay')",
    "INSERT INTO src VALUES (1, 'new')",
];

#[test]
fn update_from_basic_join() {
    diff_program(
        UF_SETUP,
        &["UPDATE t SET v = src.v FROM src WHERE t.id = src.id"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_multiple_matches_last_wins() {
    // Two matching FROM rows: SQLite updates once ("one arbitrary row";
    // in practice the LAST). The engines must agree on the final value.
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE TABLE src (t_id INT, v TEXT)",
            "INSERT INTO t VALUES (1, 'old')",
            "INSERT INTO src VALUES (1, 'first'), (1, 'second')",
        ],
        &["UPDATE t SET v = src.v FROM src WHERE t.id = src.t_id"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_no_match_leaves_rows() {
    diff_program(
        UF_SETUP,
        &["UPDATE t SET v = src.v FROM src WHERE t.id = src.id + 100"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_subquery_source() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE TABLE base (id INT, v TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            "INSERT INTO base VALUES (1, 'x'), (2, 'y')",
        ],
        &["UPDATE t SET v = s.v FROM (SELECT id, v FROM base WHERE id > 0) AS s WHERE t.id = s.id"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_join_in_from() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE TABLE a (id INT, val TEXT)",
            "CREATE TABLE b (id INT, tag TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            "INSERT INTO a VALUES (1, 'va'), (2, 'vb')",
            "INSERT INTO b VALUES (1, 't1'), (2, 't2')",
        ],
        &["UPDATE t SET v = a.val FROM a JOIN b ON a.id = b.id WHERE t.id = a.id"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_multiple_set_columns() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, w INT)",
            "CREATE TABLE src (id INT, v TEXT, w INT)",
            "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)",
            "INSERT INTO src VALUES (1, 'z', 99), (3, 'q', 77)",
        ],
        &["UPDATE t SET v = src.v, w = src.w FROM src WHERE t.id = src.id"],
        "SELECT id, v, w FROM t ORDER BY id",
    );
}

#[test]
fn update_from_with_returning() {
    diff_program(
        UF_SETUP,
        &["UPDATE t SET v = src.v FROM src WHERE t.id = src.id RETURNING id, v"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_arithmetic_from_side() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INT)",
            "CREATE TABLE s (id INT, n INT)",
            "INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)",
            "INSERT INTO s VALUES (1, 1), (2, 2)",
        ],
        &["UPDATE t SET n = t.n + s.n FROM s WHERE t.id = s.id"],
        "SELECT id, n FROM t ORDER BY id",
    );
}

#[test]
fn update_from_self_reference() {
    // The target table can appear on the FROM side too (SQLite allows it).
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, up INT)",
            "INSERT INTO t VALUES (1, 'a', NULL), (2, 'b', NULL)",
        ],
        &["UPDATE t SET up = s.id FROM t AS s WHERE t.id = s.id + 1"],
        "SELECT id, v, up FROM t ORDER BY id",
    );
}

#[test]
fn update_from_inside_transaction() {
    diff_program(
        UF_SETUP,
        &[
            "BEGIN",
            "UPDATE t SET v = src.v FROM src WHERE t.id = src.id",
            "COMMIT",
        ],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_from_index_maintenance() {
    // SET a column that has a UNIQUE/normal index — the index must stay
    // consistent after the FROM-join update.
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            "CREATE TABLE s (id INT, v TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            "INSERT INTO s VALUES (1, 'x'), (2, 'y')",
        ],
        &["UPDATE t SET v = s.v FROM s WHERE t.id = s.id"],
        "SELECT id, v FROM t ORDER BY v",
    );
}

#[test]
fn update_from_changes_count() {
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for e in UF_SETUP {
        db.execute(e, []).unwrap();
        conn.execute(e, []).unwrap();
    }
    db.execute("UPDATE t SET v = src.v FROM src WHERE t.id = src.id", [])
        .unwrap();
    let ours: i64 = db.query("SELECT changes()", []).unwrap()[0][0].as_integer();
    conn.execute("UPDATE t SET v = src.v FROM src WHERE t.id = src.id", [])
        .unwrap();
    let theirs: i64 = conn.query_row("SELECT changes()", [], |r| r.get(0)).unwrap();
    assert_eq!(ours, theirs, "changes() after UPDATE FROM must match");
}

// ===========================================================================
// Collations
// ===========================================================================

#[test]
fn nocase_explicit_in_where() {
    for sql in [
        "SELECT id FROM t WHERE v = 'OLD' COLLATE NOCASE",
        "SELECT id FROM t WHERE v COLLATE NOCASE = 'OLD'",
        "SELECT id FROM t WHERE 'OLD' = v COLLATE NOCASE",
        "SELECT id FROM t WHERE v <> 'OLD' COLLATE NOCASE",
        "SELECT id FROM t WHERE v < 'OLD' COLLATE NOCASE",
    ] {
        diff_program(
            &[
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
                "INSERT INTO t VALUES (1, 'old'), (2, 'keep')",
            ],
            &[],
            sql,
        );
    }
}

#[test]
fn nocase_column_declared_in_where() {
    diff_program(
        &[
            "CREATE TABLE u (name TEXT COLLATE NOCASE, n INT)",
            "INSERT INTO u VALUES ('Alice', 1)",
        ],
        &[],
        "SELECT n FROM u WHERE name = 'ALICE'",
    );
    diff_program(
        &[
            "CREATE TABLE u (name TEXT COLLATE NOCASE, n INT)",
            "INSERT INTO u VALUES ('Alice', 1)",
        ],
        &[],
        "SELECT n FROM u WHERE name = 'alice'",
    );
}

#[test]
fn nocase_in_list() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
            "INSERT INTO t VALUES (1, 'Alpha'), (2, 'Beta'), (3, 'Gamma')",
        ],
        &[],
        "SELECT id FROM t WHERE v IN ('ALPHA', 'beta')",
    );
}

#[test]
fn nocase_between() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE)",
            "INSERT INTO t VALUES (1, 'apple'), (2, 'Banana'), (3, 'cherry')",
        ],
        &[],
        "SELECT id FROM t WHERE v BETWEEN 'b' AND 'c'",
    );
}

#[test]
fn rtrim_column_in_where() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT COLLATE RTRIM)",
            "INSERT INTO t VALUES (1, 'abc'), (2, 'abc   '), (3, 'xyz')",
        ],
        &[],
        "SELECT id FROM t WHERE v = 'abc'",
    );
}

#[test]
fn unique_with_declared_nocase() {
    // `email TEXT UNIQUE COLLATE NOCASE` — uniqueness is case-insensitive.
    diff_error(
        &[
            "CREATE TABLE w (email TEXT UNIQUE COLLATE NOCASE)",
            "INSERT INTO w VALUES ('alice@example.com')",
        ],
        "INSERT INTO w VALUES ('ALICE@EXAMPLE.COM')",
    );
}

#[test]
fn unique_table_constraint_nocase() {
    // Table-level UNIQUE over a NOCASE column.
    diff_error(
        &[
            "CREATE TABLE z (email TEXT COLLATE NOCASE, n INT, UNIQUE (email))",
            "INSERT INTO z VALUES ('alice@example.com', 1)",
        ],
        "INSERT INTO z VALUES ('ALIce@example.com', 2)",
    );
}

#[test]
fn unique_nocase_allows_different() {
    // Different values must still insert fine.
    diff_program(
        &[
            "CREATE TABLE w (email TEXT UNIQUE COLLATE NOCASE)",
            "INSERT INTO w VALUES ('alice@example.com')",
        ],
        &["INSERT INTO w VALUES ('bob@example.com')"],
        "SELECT email FROM w ORDER BY email",
    );
}

#[test]
fn nocase_index_seeks() {
    // CREATE INDEX with explicit COLLATE + equality probes through the
    // index (the fast path).
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'GAMMA')",
            "CREATE INDEX si ON s(tag COLLATE NOCASE)",
        ],
        &[],
        "SELECT id FROM s WHERE tag = 'ALPHA' COLLATE NOCASE",
    );
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'GAMMA')",
            "CREATE INDEX si ON s(tag COLLATE NOCASE)",
        ],
        &[],
        "SELECT id FROM s WHERE tag = 'beta' COLLATE NOCASE",
    );
}

#[test]
fn nocase_index_in_list() {
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'GAMMA')",
            "CREATE INDEX si ON s(tag COLLATE NOCASE)",
        ],
        &[],
        "SELECT id FROM s WHERE tag IN ('ALPHA', 'BETA', 'nope') COLLATE NOCASE",
    );
}

#[test]
fn nocase_index_range() {
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'GAMMA'), (4, 'delta')",
            "CREATE INDEX si ON s(tag COLLATE NOCASE)",
        ],
        &[],
        "SELECT id FROM s WHERE tag > 'beta' COLLATE NOCASE ORDER BY id",
    );
}

#[test]
fn index_inherits_column_collation() {
    // A plain CREATE INDEX on a NOCASE column inherits the collation.
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta')",
            "CREATE INDEX si ON s(tag)",
        ],
        &[],
        "SELECT id FROM s WHERE tag = 'BETA'",
    );
}

#[test]
fn nocase_order_by() {
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'GAMMA')",
        ],
        &[],
        "SELECT tag FROM s ORDER BY tag COLLATE NOCASE",
    );
}

#[test]
fn rtrim_unique() {
    diff_error(
        &[
            "CREATE TABLE r (v TEXT UNIQUE COLLATE RTRIM)",
            "INSERT INTO r VALUES ('abc')",
        ],
        "INSERT INTO r VALUES ('abc   ')",
    );
}

#[test]
fn nocase_update_index_consistency() {
    // UPDATE a NOCASE-indexed column through the index maintenance path.
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE UNIQUE)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta')",
        ],
        &["UPDATE s SET tag = 'ALPHA' WHERE id = 2"],
        "SELECT id, tag FROM s ORDER BY id",
    );
}

#[test]
fn nocase_delete_with_index() {
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'GAMMA')",
            "CREATE INDEX si ON s(tag COLLATE NOCASE)",
        ],
        &["DELETE FROM s WHERE tag = 'BETA'"],
        "SELECT id, tag FROM s ORDER BY id",
    );
}

#[test]
fn collated_index_survives_reopen() {
    // The implicit UNIQUE auto-index with COLLATE round-trips through the
    // schema SQL on reopen.
    let dir = std::env::temp_dir().join(format!(
        "rustqlite_collate_reopen_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE w (email TEXT UNIQUE COLLATE NOCASE)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO w VALUES ('alice@example.com')", [])
            .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let r = db.execute("INSERT INTO w VALUES ('ALICE@EXAMPLE.COM')", []);
        assert!(
            r.is_err(),
            "UNIQUE + COLLATE NOCASE must survive reopen (schema round-trip)"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// UPDATE unique-index enforcement, conflict algorithms, rowid moves
// ===========================================================================

#[test]
fn update_unique_violation_basic() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
        ],
        &["UPDATE t SET v = 'a' WHERE id = 2"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_unique_composite_key() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b INT, UNIQUE(a, b))",
            "INSERT INTO t VALUES (1, 1, 1), (2, 2, 2), (3, 3, 3)",
        ],
        &["UPDATE t SET a = 1, b = 1 WHERE id = 2"],
        "SELECT id, a, b FROM t ORDER BY id",
    );
}

#[test]
fn update_unique_vacated_by_earlier_row() {
    // Sequential application: row 1 vacates key 1 before row 2 claims it.
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT UNIQUE)",
            "INSERT INTO t VALUES (1, 1), (2, 2)",
        ],
        &["UPDATE t SET v = v - 1"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_unique_swap_conflicts() {
    // Swapping keys collides under sequential application — SQLite errors.
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT UNIQUE)",
            "INSERT INTO t VALUES (1, 1), (2, 2)",
        ],
        &["UPDATE t SET v = 3 - v"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_unique_nulls_always_allowed() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT UNIQUE)",
            "INSERT INTO t VALUES (1, 1), (2, 2)",
        ],
        &["UPDATE t SET v = NULL"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_or_ignore_skips_conflicting_row() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        ],
        &[
            "UPDATE OR IGNORE t SET v = CASE id WHEN 2 THEN 'a' ELSE v END",
            "SELECT changes()",
        ],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_or_replace_deletes_holder() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        ],
        &["UPDATE OR REPLACE t SET v = 'a' WHERE id = 3"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_rowid_move_to_free() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        ],
        &["UPDATE t SET id = 10 WHERE id = 1"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_rowid_move_to_taken_fails() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        ],
        &["UPDATE t SET id = 2 WHERE id = 3"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_rowid_move_with_unique_index() {
    // Row moves must maintain every index entry (reinsert at the new rowid).
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            "CREATE INDEX iv ON t(v)",
        ],
        &[
            "UPDATE t SET id = 5 WHERE id = 1",
            "SELECT v FROM t WHERE v = 'a'",
            "SELECT COUNT(*) FROM t",
        ],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_rowid_move_or_ignore() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
        ],
        &["UPDATE OR IGNORE t SET id = 2 WHERE id = 1"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_rowid_null_autoassigns() {
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
        ],
        &["UPDATE t SET id = NULL WHERE id = 1"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_multi_row_unique_ratchet() {
    // The classic ratchet: shifting every value down by one in scan order
    // succeeds vacated-key-by-vacated-key (SQLite sequential semantics).
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INT UNIQUE)",
            "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
        ],
        &["UPDATE t SET v = v - 10"],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_nocase_multi_row_conflict() {
    diff_program(
        &[
            "CREATE TABLE s (id INTEGER PRIMARY KEY, tag TEXT COLLATE NOCASE UNIQUE)",
            "INSERT INTO s VALUES (1, 'Alpha'), (2, 'beta'), (3, 'Gamma')",
        ],
        &["UPDATE s SET tag = 'BETA' WHERE id = 1"],
        "SELECT id, tag FROM s ORDER BY id",
    );
}

#[test]
fn update_unique_index_sees_move() {
    // After the update, an index seek must find the moved value.
    diff_program(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
        ],
        &[
            "UPDATE t SET v = 'zed' WHERE id = 1",
            "SELECT id FROM t WHERE v = 'zed'",
            "SELECT COUNT(*) FROM t WHERE v = 'a'",
        ],
        "SELECT id, v FROM t ORDER BY id",
    );
}

#[test]
fn update_fk_message_shape() {
    // Runtime FK violations use SQLite's bare message on both engines.
    // NOTE: rusqlite's bundled SQLite builds with FKs ON by default;
    // upstream sqlite3 and rustqlite default OFF — align explicitly.
    diff_error(
        &[
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INT REFERENCES p(id))",
            "INSERT INTO p VALUES (1)",
            "INSERT INTO c VALUES (1, 1)",
        ],
        "UPDATE c SET pid = 99 WHERE id = 1",
    );
}

#[test]
fn update_notnull_message_shape() {
    diff_error(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
            "INSERT INTO t VALUES (1, 'a')",
        ],
        "UPDATE t SET v = NULL WHERE id = 1",
    );
}
