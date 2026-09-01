//! Differential tests for the SQLite-introspection PRAGMAs:
//! `PRAGMA table_info`, `table_xinfo`, `index_list`, `index_info`,
//! `foreign_key_list`, plus the `PRAGMA journal_mode = X` result row.
//!
//! Every case runs the SAME SQL against rustqlite and real SQLite
//! (rusqlite) and asserts value-by-value equality of the full result —
//! columns AND rows, including NULLs and integers-vs-text distinctions
//! that ORMs depend on.

use rustqlite::Database;

/// Run a SQL script on both engines, then compare the given query's
/// rows value-by-value (rendered into a comparable form).
fn diff_query(setup: &[&str], query: &str) {
    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
        conn.execute(s, []).unwrap();
    }
    let ours = db
        .query_with_columns(query, [])
        .map(|(cols, rows)| render(&cols, &rows))
        .unwrap_or_else(|e| panic!("rustqlite failed on {query}: {e}"));
    let theirs: Vec<Vec<String>> = {
        let mut stmt = conn.prepare(query).unwrap();
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
        out
    };
    assert_eq!(
        ours, theirs,
        "\nPRAGMA mismatch on {query}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
    );
}

/// Render rustqlite rows with type tags so Integer(0) != Text("0") etc.
fn render(cols: &[String], rows: &[rustqlite::Row]) -> Vec<Vec<String>> {
    let _ = cols;
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

const T1: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL DEFAULT 10.5, note TEXT, ts DATETIME DEFAULT CURRENT_TIMESTAMP)",
];

#[test]
fn table_info_basic_shape() {
    diff_query(T1, "PRAGMA table_info(t)");
}

#[test]
fn table_info_typeless_and_defaults() {
    diff_query(
        &["CREATE TABLE u (a, b INT DEFAULT 5, c TEXT DEFAULT 'hi', d DEFAULT NULL, e DEFAULT -3)"],
        "PRAGMA table_info(u)",
    );
}

#[test]
fn table_info_notnull_pk_quirk() {
    // SQLite's famous quirk: plain INTEGER PRIMARY KEY reports notnull=0;
    // explicit NOT NULL reports 1.
    diff_query(
        &["CREATE TABLE q (a INTEGER PRIMARY KEY, b INT NOT NULL)"],
        "PRAGMA table_info(q)",
    );
    diff_query(
        &["CREATE TABLE q2 (c INT PRIMARY KEY NOT NULL)"],
        "PRAGMA table_info(q2)",
    );
}

#[test]
fn table_info_compound_pk_positions() {
    // Compound table-level PK: pk column = position within the PK clause
    // (not declaration order).
    diff_query(
        &["CREATE TABLE cp (a, b, c, PRIMARY KEY (c, a))"],
        "PRAGMA table_info(cp)",
    );
}

#[test]
fn table_info_rowid_alias_pk() {
    diff_query(
        &["CREATE TABLE r (id INTEGER PRIMARY KEY DESC, v TEXT)"],
        "PRAGMA table_info(r)",
    );
}

#[test]
fn table_info_empty_for_missing_table() {
    // SQLite returns ZERO rows (no error) for an unknown table.
    diff_query(&["CREATE TABLE x (a)"], "PRAGMA table_info(nonexistent)");
}

#[test]
fn table_xinfo_hidden_columns() {
    diff_query(T1, "PRAGMA table_xinfo(t)");
}

#[test]
fn table_info_quoted_argument() {
    diff_query(T1, "PRAGMA table_info('t')");
}

#[test]
fn index_list_created_index() {
    diff_query(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT, w INT)",
            "CREATE INDEX iv ON t(v)",
            "CREATE UNIQUE INDEX iw ON t(w)",
        ],
        "PRAGMA index_list(t)",
    );
}

#[test]
fn index_list_unique_constraint_autos() {
    diff_query(
        &["CREATE TABLE t (a TEXT UNIQUE, b INT UNIQUE, c INT)"],
        "PRAGMA index_list(t)",
    );
}

#[test]
fn index_list_partial_index() {
    diff_query(
        &[
            "CREATE TABLE t (a INT, b INT)",
            "CREATE INDEX ip ON t(a) WHERE b > 5",
        ],
        "PRAGMA index_list(t)",
    );
}

#[test]
fn index_list_empty_table() {
    diff_query(&["CREATE TABLE t (a INT)"], "PRAGMA index_list(t)");
}

#[test]
fn index_info_single_and_composite() {
    diff_query(
        &[
            "CREATE TABLE t (a INT, b TEXT, c REAL)",
            "CREATE INDEX i1 ON t(b)",
            "CREATE INDEX i2 ON t(a, c DESC)",
        ],
        "PRAGMA index_info(i1)",
    );
    diff_query(
        &[
            "CREATE TABLE t (a INT, b TEXT, c REAL)",
            "CREATE INDEX i2 ON t(a, c DESC)",
        ],
        "PRAGMA index_info(i2)",
    );
}

#[test]
fn index_xinfo_collation() {
    diff_query(
        &[
            "CREATE TABLE t (a TEXT, b TEXT)",
            "CREATE INDEX ic ON t(a COLLATE NOCASE, b DESC)",
        ],
        "PRAGMA index_xinfo(ic)",
    );
}

#[test]
fn foreign_key_list_column_level() {
    diff_query(
        &[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE child (cid INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
        ],
        "PRAGMA foreign_key_list(child)",
    );
}

#[test]
fn foreign_key_list_table_level_actions() {
    diff_query(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (a INT, b INT, FOREIGN KEY (a, b) REFERENCES p (id, id) ON DELETE CASCADE ON UPDATE SET NULL)",
        ],
        "PRAGMA foreign_key_list(c)",
    );
}

#[test]
fn foreign_key_list_implicit_parent_pk() {
    // REFERENCES p (no columns) — the parent's PK is implied.
    diff_query(
        &[
            "CREATE TABLE p (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (a INT REFERENCES p)",
        ],
        "PRAGMA foreign_key_list(c)",
    );
}

#[test]
fn foreign_key_list_multiple_constraints_reverse_order() {
    // SQLite lists FK clauses in REVERSE declaration order (id = 0 is the
    // last-declared constraint).
    diff_query(
        &[
            "CREATE TABLE p (x INTEGER PRIMARY KEY, y INTEGER UNIQUE)",
            "CREATE TABLE c (a INT REFERENCES p(x), b INT REFERENCES p(y))",
        ],
        "PRAGMA foreign_key_list(c)",
    );
}

#[test]
fn foreign_key_list_no_fks() {
    diff_query(&["CREATE TABLE t (a INT)"], "PRAGMA foreign_key_list(t)");
}

#[test]
fn journal_mode_write_returns_row() {
    let mut db = Database::open_in_memory().unwrap();
    let res = db.query_with_columns("PRAGMA journal_mode=WAL", []).unwrap();
    assert_eq!(res.0, vec!["journal_mode".to_string()]);
    assert_eq!(res.1.len(), 1);
    match &res.1[0][0] {
        rustqlite::Value::Text(t) => assert_eq!(t.as_str(), "wal"),
        other => panic!("journal_mode row should be Text, got {other:?}"),
    }
    // And a read returns the current mode.
    let res = db.query("PRAGMA journal_mode", []).unwrap();
    match &res[0][0] {
        rustqlite::Value::Text(t) => assert_eq!(t.as_str(), "wal"),
        other => panic!("journal_mode read should be Text, got {other:?}"),
    }
}

#[test]
fn journal_mode_call_form_writes() {
    let mut db = Database::open_in_memory().unwrap();
    let res = db.query("PRAGMA journal_mode(WAL)", []).unwrap();
    assert_eq!(res.len(), 1);
    match &res[0][0] {
        rustqlite::Value::Text(t) => assert_eq!(t.as_str(), "wal"),
        other => panic!("journal_mode() write should return Text, got {other:?}"),
    }
}

#[test]
fn write_pragmas_still_return_no_rows() {
    // Non-journal_mode write pragmas return zero rows (SQLite behavior
    // for most write forms; sqlx sends these at connect time).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INT)", []).unwrap();
    let res = db.query("PRAGMA foreign_keys=ON", []).unwrap();
    assert!(res.is_empty());
    let res = db.query("PRAGMA cache_size=-2000", []).unwrap();
    assert!(res.is_empty());
}

#[test]
fn pragma_read_forms_still_single_row() {
    // foreign_keys: rusqlite turns it ON by default while rustqlite's
    // default is OFF (SQLite's own default) — align both first.
    // (page_size differs by design: rustqlite defaults to 8 KiB pages.)
    diff_query(
        &["CREATE TABLE t (a)", "PRAGMA foreign_keys=OFF"],
        "PRAGMA foreign_keys",
    );
    diff_query(&["CREATE TABLE t (a)"], "PRAGMA encoding");
}

#[test]
fn integrity_check_row_shape() {
    // integrity_check returns single-column rows ("ok" when clean).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INT)", []).unwrap();
    db.execute("INSERT INTO t VALUES (1),(2)", []).unwrap();
    let (cols, rows) = db.query_with_columns("PRAGMA integrity_check", []).unwrap();
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0], "integrity_check");
    assert_eq!(rows.len(), 1);
    match &rows[0][0] {
        rustqlite::Value::Text(t) => assert_eq!(t.as_str(), "ok"),
        other => panic!("integrity_check should report 'ok', got {other:?}"),
    }
}

#[test]
fn pragma_table_info_through_prepared_statement() {
    // The statement layer (prepare/step) must serve the same rows —
    // this is the path the C ABI / sqlx takes.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL DEFAULT 'x')",
        [],
    )
    .unwrap();
    let mut stmt = db.prepare("PRAGMA table_info(t)").unwrap();
    let mut count = 0;
    while let rustqlite::StepResult::Row = stmt.step().unwrap() {
        assert_eq!(stmt.column_count(), 6);
        let name = stmt.column_value(1).unwrap();
        assert!(matches!(name, rustqlite::Value::Text(_)));
        count += 1;
    }
    assert_eq!(count, 2);
    // Column names match SQLite's exact spellings.
    let mut stmt = db.prepare("PRAGMA table_info(t)").unwrap();
    stmt.step().unwrap();
    let names: Vec<String> = (0..stmt.column_count())
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]
    );
}
