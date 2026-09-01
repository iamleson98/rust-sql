//! Differential tests for `sqlite_master` — the schema catalog every
//! migration tool, ORM introspector, and sqlx reads. rustqlite vs SQLite
//! (rusqlite) must agree on type/name/tbl_name/rootpage and the `sql`
//! DDL text byte-for-byte for every object kind.

use rustqlite::Database;

/// Render both engines' sqlite_master into comparable rows.
fn diff_master(setup: &[&str], query: &str) {
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
        "\nsqlite_master mismatch on {query}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
    );
}

fn render(cols: &[String], rows: &[rustqlite::Row]) -> Vec<Vec<String>> {
    let _ = cols;
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    rustqlite::Value::Null => "NULL".to_string(),
                    rustqlite::Value::Integer(i) => format!("I:{i}"),
                    rustqlite::Value::Real(f) => format!("R:{f}"),
                    rustqlite::Value::Text(t) => format!("T:{}", t.as_str()),
                    rustqlite::Value::Blob(b) => format!("B:{}", b.len()),
                })
                .collect()
        })
        .collect()
}

#[test]
fn master_table_row_shape() {
    diff_master(
        &["CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"],
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE type='table'",
    );
}

#[test]
fn master_index_rows() {
    diff_master(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE, w INT)",
            "CREATE INDEX iw ON t(w)",
        ],
        // rootpage differs by page allocation order — compare names + sql.
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE type='index' ORDER BY name",
    );
}

#[test]
fn master_view_row() {
    diff_master(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE VIEW v AS SELECT id FROM t",
        ],
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE type='view'",
    );
}

#[test]
fn master_trigger_row() {
    diff_master(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE TRIGGER trg AFTER INSERT ON t BEGIN UPDATE t SET v = v; END",
        ],
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE type='trigger'",
    );
}

#[test]
fn master_autoindex_name_and_sql() {
    // UNIQUE constraints materialize sqlite_autoindex_<table>_N entries.
    diff_master(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, a TEXT, b TEXT, UNIQUE(a, b))",
        ],
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE type='index' ORDER BY name",
    );
}

#[test]
fn master_full_column_order() {
    // Column ORDER matters: (type, name, tbl_name, rootpage, sql).
    diff_master(
        &["CREATE TABLE t (id INTEGER PRIMARY KEY)"],
        "SELECT type, name, tbl_name, sql FROM sqlite_master",
    );
}

#[test]
fn master_migration_pattern() {
    // The sqlx / sea-orm migrator probe: does the bookkeeping table exist?
    diff_master(
        &[],
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    );
    diff_master(
        &[
            "CREATE TABLE IF NOT EXISTS _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL)",
        ],
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    );
}

#[test]
fn master_tables_in_creation_order() {
    diff_master(
        &[
            "CREATE TABLE a (id INTEGER PRIMARY KEY)",
            "CREATE TABLE b (id INTEGER PRIMARY KEY)",
            "CREATE TABLE c (id INTEGER PRIMARY KEY)",
        ],
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY rowid",
    );
}

#[test]
fn master_filter_by_name_and_type() {
    diff_master(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE INDEX iv ON t(v)",
        ],
        "SELECT name FROM sqlite_master WHERE tbl_name = 't' AND type = 'index'",
    );
}

#[test]
fn create_if_not_exists_is_idempotent() {
    // Every migrator relies on this; the second CREATE must be a no-op
    // (both engines) and leave exactly one catalog row.
    diff_master(
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)",
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)",
        ],
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='t'",
    );
}
