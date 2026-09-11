//! Differential schema-surface parity: sqlite_master rows, PRAGMA result
//! shapes, AUTOINCREMENT/sqlite_sequence semantics, and identifier
//! quoting — engine vs real (bundled) SQLite. Every case here was a live
//! divergence found by `examples/probe_master_ddl.rs` and closed:
//!
//! - rootpage numbering: SQLite's 1-based file convention (page 1 is the
//!   schema b-tree; the first user object is page 2). The engine stores
//!   internal 0-based ids and translates at the master-row boundary.
//! - WITHOUT ROWID PK: no autoindex row in sqlite_master (the table
//!   b-tree IS the PK), but `PRAGMA index_list` reports it with origin
//!   'pk' under SQLite's `sqlite_autoindex_<t>_<N>` numbering (N runs
//!   after every other autoindex), and `PRAGMA table_info` shows the PK
//!   columns as notnull=1.
//! - AUTOINCREMENT: a REAL `sqlite_sequence(name,seq)` table (visible in
//!   sqlite_master, queryable, user-editable), high-water rowid
//!   allocation (deleted top rowids never reused), explicit-rowid bumps,
//!   SQLite's two validation errors, and drop/remake behavior.
//! - Reserved `sqlite_%` object names.
//! - `[bracket]` / `` `backtick` `` identifier quoting in DDL.
//! - Read-form pragmas: schema_version (cookie), busy_timeout,
//!   cache_size (raw setting), max_page_count, data_version,
//!   journal_mode=memory on `:memory:`, collation_list, database_list,
//!   pragma_list, compile_options, function_list, module_list.

use rustqlite::{Database, Value};

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => format!("I:{i}"),
        Value::Real(f) => format!("R:{f}"),
        Value::Text(t) => format!("T:{}", t.as_str()),
        Value::Blob(b) => format!("B:{}", b.len()),
    }
}

fn render_rows(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(render).collect())
        .collect()
}

/// Build expected rows from &str literals.
fn srows(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|c| c.to_string()).collect())
        .collect()
}

fn engine_master(setup: &[&str]) -> Vec<Vec<String>> {
    let mut db = Database::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
    }
    render_rows(
        &db.query(
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master ORDER BY name",
            [],
        )
        .unwrap(),
    )
}

fn sqlite_master(setup: &[&str]) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        conn.execute_batch(s).unwrap();
    }
    let mut out = Vec::new();
    let mut stmt = conn
        .prepare("SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master ORDER BY name")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::new();
        for i in 0..5 {
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

fn diff_master(name: &str, setup: Vec<&str>) {
    let ours = engine_master(&setup);
    let theirs = sqlite_master(&setup);
    assert_eq!(
        ours, theirs,
        "\nsqlite_master mismatch [{name}]\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
    );
}

// ---------------------------------------------------------------------------
// sqlite_master rows
// ---------------------------------------------------------------------------

#[test]
fn master_rootpage_convention() {
    // Page 1 is the schema b-tree: the first user table is rootpage 2
    // and every subsequent object continues from there.
    diff_master("plain", vec!["CREATE TABLE t (a INTEGER, b TEXT)"]);
    diff_master(
        "table+index",
        vec!["CREATE TABLE t (a INTEGER)", "CREATE INDEX ix ON t(a)"],
    );
    diff_master("table+autoindex", vec!["CREATE TABLE t (a INTEGER UNIQUE)"]);
    diff_master(
        "two-tables-autoindex",
        vec![
            "CREATE TABLE t1 (a UNIQUE)",
            "CREATE TABLE t2 (b INTEGER PRIMARY KEY, c UNIQUE, d UNIQUE)",
        ],
    );
    diff_master(
        "composite-pk",
        vec!["CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a, b))"],
    );
    diff_master(
        "view-and-trigger",
        vec![
            "CREATE TABLE t (a INTEGER)",
            "CREATE VIEW v AS SELECT a FROM t",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END",
        ],
    );
}

#[test]
fn master_without_rowid_pk_has_no_autoindex_row() {
    // SQLite: the WR table b-tree IS the PK index — no autoindex row in
    // sqlite_master, whatever the PK shape.
    diff_master(
        "wr-single-pk",
        vec!["CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a)) WITHOUT ROWID"],
    );
    diff_master(
        "wr-composite-pk",
        vec!["CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID"],
    );
    diff_master(
        "wr-composite-pk-plus-unique",
        vec!["CREATE TABLE t (a INTEGER UNIQUE, b TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID"],
    );
    diff_master(
        "wr-multi-unique",
        vec!["CREATE TABLE t (a INTEGER UNIQUE, b TEXT UNIQUE, PRIMARY KEY(a,b)) WITHOUT ROWID"],
    );
}

#[test]
fn master_autoincrement_sequence_row() {
    // AUTOINCREMENT materializes a real sqlite_sequence table (master
    // row + rootpage in allocation order: table, then sequence).
    diff_master(
        "autoinc",
        vec!["CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)"],
    );
    diff_master(
        "autoinc-plus-index",
        vec![
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            "CREATE INDEX ix ON t(v)",
        ],
    );
}

#[test]
fn master_quoted_identifiers() {
    // All three of SQLite's identifier quoting families round-trip in
    // the stored DDL text and the name column.
    diff_master(
        "double-quoted",
        vec!["CREATE TABLE \"my table\" (\"select\" INTEGER, \"from\" TEXT)"],
    );
    diff_master("bracket", vec!["CREATE TABLE [t2] ([a b] INTEGER)"]);
    diff_master("backtick", vec!["CREATE TABLE `t3` (`c d` TEXT)"]);
}

#[test]
fn bracket_and_backtick_quoting_parse_and_bind() {
    // Beyond the master rows: objects created with bracket/backtick
    // names are queryable through the same quoting (SQLite treats them
    // as ordinary identifiers).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE [t] ([a b] INTEGER, `c` TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO [t] ([a b], `c`) VALUES (1, 'x')", [])
        .unwrap();
    let rows = db.query("SELECT [a b], `c` FROM [t]", []).unwrap();
    assert_eq!(render_rows(&rows), srows(&[&["I:1", "T:x"]]));

    // Mixed quoting refers to the same object.
    let rows = db
        .query("SELECT \"a b\" FROM `t` WHERE [c] = 'x'", [])
        .unwrap();
    assert_eq!(render_rows(&rows), srows(&[&["I:1"]]));
}

#[test]
fn reserved_sqlite_names_rejected() {
    let mut db = Database::open_in_memory().unwrap();
    let err = db
        .execute("CREATE TABLE sqlite_foo (x)", [])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("object name reserved for internal use"),
        "got: {err}"
    );
    let err = db
        .execute("CREATE TABLE sqlite_sequence(name,seq)", [])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("object name reserved for internal use"),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// AUTOINCREMENT / sqlite_sequence semantics
// ---------------------------------------------------------------------------

#[test]
fn autoincrement_high_water_survives_delete() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES(NULL, 'a'), (NULL, 'b')", [])
        .unwrap();
    db.execute("DELETE FROM t WHERE id = 2", []).unwrap();
    // SQLite: the deleted top rowid is NEVER reused — the next insert
    // takes 3 (sequence high-water), not 2.
    db.execute("INSERT INTO t VALUES(NULL, 'c')", []).unwrap();
    let ids = render_rows(&db.query("SELECT id FROM t ORDER BY id", []).unwrap());
    assert_eq!(ids, srows(&[&["I:1"], &["I:3"]]));

    // sqlite_sequence tracks the high-water.
    let seq = render_rows(
        &db.query("SELECT name, seq FROM sqlite_sequence", [])
            .unwrap(),
    );
    assert_eq!(seq, srows(&[&["T:t", "I:3"]]));
}

#[test]
fn autoincrement_explicit_rowid_raises_sequence() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES(100)", []).unwrap();
    let seq = render_rows(&db.query("SELECT seq FROM sqlite_sequence", []).unwrap());
    assert_eq!(seq, srows(&[&["I:100"]]));
    db.execute("INSERT INTO t VALUES(NULL)", []).unwrap();
    let rows = render_rows(&db.query("SELECT id FROM t", []).unwrap());
    assert_eq!(rows, srows(&[&["I:100"], &["I:101"]]));
}

#[test]
fn autoincrement_user_rearm_via_update() {
    // SQLite lets users edit sqlite_sequence directly to re-arm the
    // sequence (documented usage).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES(5)", []).unwrap();
    db.execute("UPDATE sqlite_sequence SET seq = 1000 WHERE name = 't'", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES(NULL)", []).unwrap();
    let rows = render_rows(
        &db.query("SELECT id FROM t ORDER BY id DESC LIMIT 1", [])
            .unwrap(),
    );
    assert_eq!(rows, srows(&[&["I:1001"]]));
}

#[test]
fn autoincrement_two_tables_share_sequence_table() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES(10)", []).unwrap();
    db.execute("INSERT INTO u VALUES(20)", []).unwrap();
    let seq = render_rows(
        &db.query("SELECT name, seq FROM sqlite_sequence ORDER BY name", [])
            .unwrap(),
    );
    assert_eq!(seq, srows(&[&["T:t", "I:10"], &["T:u", "I:20"]]));
}

#[test]
fn autoincrement_drop_removes_sequence_row_keeps_table() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES(1)", []).unwrap();
    db.execute("INSERT INTO u VALUES(1)", []).unwrap();
    db.execute("DROP TABLE u", []).unwrap();
    // SQLite: the sequence TABLE survives; the dropped table's ROW is
    // removed.
    let names = render_rows(&db.query("SELECT name FROM sqlite_sequence", []).unwrap());
    assert_eq!(names, srows(&[&["T:t"]]));

    // A recreated autoincrement table starts fresh (no stale row).
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("INSERT INTO u VALUES(NULL)", []).unwrap();
    let id = render_rows(&db.query("SELECT id FROM u", []).unwrap());
    assert_eq!(id, srows(&[&["I:1"]]));
}

#[test]
fn autoincrement_validation_errors() {
    let mut db = Database::open_in_memory().unwrap();
    let err = db
        .execute(
            "CREATE TABLE t (a TEXT PRIMARY KEY AUTOINCREMENT, b TEXT)",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY"),
        "got: {err}"
    );
    let err = db
        .execute(
            "CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT, b TEXT) WITHOUT ROWID",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("AUTOINCREMENT not allowed on WITHOUT ROWID tables"),
        "got: {err}"
    );
}

#[test]
fn autoincrement_reopen_persistence() {
    let path = std::env::temp_dir().join("schema_parity_seq.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
            .unwrap();
        db.execute("INSERT INTO s VALUES(50)", []).unwrap();
        db.execute("DELETE FROM s WHERE id = 50", []).unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        // The high-water (50) survived the close: 51, not 1.
        db.execute("INSERT INTO s VALUES(NULL)", []).unwrap();
        let rows = render_rows(&db.query("SELECT id FROM s", []).unwrap());
        assert_eq!(rows, srows(&[&["I:51"]]));
        let seq = render_rows(&db.query("SELECT seq FROM sqlite_sequence", []).unwrap());
        assert_eq!(seq, srows(&[&["I:51"]]));
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// PRAGMA result shapes
// ---------------------------------------------------------------------------

fn diff_pragmas(setup: &[&str], pragmas: &[&str], name: &str) {
    let mut ours_db = Database::open_in_memory().unwrap();
    for s in setup {
        ours_db.execute(s, []).unwrap();
    }
    let theirs_conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        theirs_conn.execute_batch(s).unwrap();
    }
    for p in pragmas {
        let ours = render_rows(&ours_db.query(p, []).unwrap());
        let mut theirs: Vec<Vec<String>> = Vec::new();
        let mut stmt = match theirs_conn.prepare(p) {
            Ok(s) => s,
            Err(e) => panic!("[{name}] {p}: sqlite prepare failed: {e}"),
        };
        let ncols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let mut row = Vec::new();
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
        assert_eq!(
            ours, theirs,
            "\nPRAGMA mismatch [{name}] {p}\n  rustqlite: {ours:#?}\n  sqlite:   {theirs:#?}"
        );
    }
}

#[test]
fn pragma_table_info_without_rowid_pk_notnull() {
    // WR-PK columns report notnull=1 (rowid-table PK columns stay 0 —
    // SQLite's legacy quirk).
    diff_pragmas(
        &[
            "CREATE TABLE c (pid INTEGER, k TEXT, PRIMARY KEY(pid, k)) WITHOUT ROWID",
            "CREATE TABLE r (a INTEGER, b TEXT, PRIMARY KEY(a, b))",
        ],
        &["PRAGMA table_info(c)", "PRAGMA table_info(r)"],
        "wr-pk-notnull",
    );
}

#[test]
fn pragma_index_list_without_rowid_naming() {
    // The WR-PK's index_list entry: SQLite's naming (N runs after every
    // other autoindex — no uniques -> _1, one -> _2, two -> _3), origin
    // 'pk', listed first.
    diff_pragmas(
        &[
            "CREATE TABLE a (x INTEGER, y TEXT, PRIMARY KEY(x)) WITHOUT ROWID",
            "CREATE TABLE b (x INTEGER UNIQUE, y TEXT, PRIMARY KEY(x, y)) WITHOUT ROWID",
            "CREATE TABLE c (x INTEGER UNIQUE, y TEXT UNIQUE, PRIMARY KEY(x, y)) WITHOUT ROWID",
        ],
        &[
            "PRAGMA index_list(a)",
            "PRAGMA index_list(b)",
            "PRAGMA index_list(c)",
        ],
        "wr-index-list",
    );
}

#[test]
fn pragma_read_forms_match() {
    // The scalar read forms whose values are engine-independent (the
    // physical ones — page_count — and connection-side settings differ
    // legitimately and are excluded; compile options/function/module
    // inventories carry the engine's own content).
    diff_pragmas(
        &[
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT NOT NULL DEFAULT 'x', c REAL UNIQUE)",
            "CREATE INDEX ix ON t(b, c DESC)",
        ],
        &[
            "PRAGMA schema_version",
            "PRAGMA cache_size",
            "PRAGMA max_page_count",
            "PRAGMA data_version",
            "PRAGMA journal_mode",
            "PRAGMA collation_list",
            "PRAGMA database_list",
            "PRAGMA pragma_list",
        ],
        "read-forms",
    );
}

#[test]
fn pragma_busy_timeout_round_trip() {
    let mut db = Database::open_in_memory().unwrap();
    let v = render_rows(&db.query("PRAGMA busy_timeout", []).unwrap());
    assert_eq!(v, srows(&[&["I:0"]]), "SQLite raw default: 0");
    db.execute("PRAGMA busy_timeout = 2500", []).unwrap();
    let v = render_rows(&db.query("PRAGMA busy_timeout", []).unwrap());
    assert_eq!(v, srows(&[&["I:2500"]]));
}

#[test]
fn pragma_cache_size_round_trip() {
    let mut db = Database::open_in_memory().unwrap();
    let v = render_rows(&db.query("PRAGMA cache_size", []).unwrap());
    assert_eq!(v, srows(&[&["I:-2000"]]), "SQLite default setting");
    db.execute("PRAGMA cache_size = -65536", []).unwrap();
    let v = render_rows(&db.query("PRAGMA cache_size", []).unwrap());
    assert_eq!(v, srows(&[&["I:-65536"]]));
    db.execute("PRAGMA cache_size = 77", []).unwrap();
    let v = render_rows(&db.query("PRAGMA cache_size", []).unwrap());
    assert_eq!(v, srows(&[&["I:77"]]));
}

#[test]
fn pragma_journal_mode_memory_database() {
    // SQLite: `:memory:` databases report (and stay) "memory" for every
    // journal_mode write.
    let db = Database::open_in_memory().unwrap();
    let v = render_rows(&db.query("PRAGMA journal_mode", []).unwrap());
    assert_eq!(v, srows(&[&["T:memory"]]));
    let v = render_rows(&db.query("PRAGMA journal_mode=WAL", []).unwrap());
    assert_eq!(v, srows(&[&["T:memory"]]));
    let v = render_rows(&db.query("PRAGMA journal_mode=DELETE", []).unwrap());
    assert_eq!(v, srows(&[&["T:memory"]]));
}

#[test]
fn pragma_function_list_shape_and_core() {
    let db = Database::open_in_memory().unwrap();
    let rows = db.query("PRAGMA function_list", []).unwrap();
    assert!(!rows.is_empty());
    // 6 columns: name, builtin, type, enc, narg, flags.
    for r in &rows {
        assert_eq!(r.len(), 6, "row shape: {r:?}");
        assert!(matches!(render(&r[1]).as_str(), "I:1"), "builtin=1");
        assert!(
            matches!(render(&r[2]).as_str(), "T:s" | "T:w" | "T:a"),
            "type in s/w/a: {:?}",
            r[2]
        );
        assert!(matches!(render(&r[3]).as_str(), "T:utf8"), "enc");
    }
    // Core scalar + window entries present with SQLite's type and arity.
    let find = |name: &str| -> Option<Vec<Value>> {
        rows.iter()
            .filter(|r| matches!(&r[0], Value::Text(t) if t.as_str() == name))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .next()
    };
    let abs = find("abs").expect("abs present");
    assert_eq!(render(&abs[2]), "T:s");
    assert_eq!(render(&abs[4]), "I:1");
    let gc = find("group_concat").expect("group_concat present");
    assert_eq!(render(&gc[2]), "T:w");
    let rank = find("rank").expect("rank present");
    assert_eq!(render(&rank[2]), "T:w");
    assert_eq!(render(&rank[4]), "I:0");
}

#[test]
fn pragma_module_and_collation_lists() {
    let db = Database::open_in_memory().unwrap();
    // Shape: single name column; the engine reports its registered
    // modules (none without plugins).
    let rows = db.query("PRAGMA module_list", []).unwrap();
    for r in &rows {
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], Value::Text(_)));
    }
    // collation_list: SQLite's base three.
    diff_pragmas(&[], &["PRAGMA collation_list"], "collation_list");
}

#[test]
fn pragma_compile_options_shape() {
    let db = Database::open_in_memory().unwrap();
    let rows = db.query("PRAGMA compile_options", []).unwrap();
    assert!(!rows.is_empty());
    for r in &rows {
        assert_eq!(r.len(), 1);
        assert!(matches!(r[0], Value::Text(_)));
    }
    // The same list backs sqlite_compileoption_get.
    let first = db.query("SELECT sqlite_compileoption_get(1)", []).unwrap();
    let opts_first = rows.first().cloned();
    assert_eq!(Some(first.into_iter().next().unwrap()), opts_first);
}

#[test]
fn pragma_schema_version_advances_on_ddl() {
    // SQLite: every schema change bumps the cookie; a fresh DB's first
    // CREATE reports 1.
    let mut db = Database::open_in_memory().unwrap();
    let before = render_rows(&db.query("PRAGMA schema_version", []).unwrap());
    assert_eq!(before, srows(&[&["I:0"]]));
    db.execute("CREATE TABLE t (a INTEGER)", []).unwrap();
    let after1 = render_rows(&db.query("PRAGMA schema_version", []).unwrap());
    assert_eq!(after1, srows(&[&["I:1"]]));
    db.execute("CREATE INDEX ix ON t(a)", []).unwrap();
    let after2 = render_rows(&db.query("PRAGMA schema_version", []).unwrap());
    assert_eq!(after2, srows(&[&["I:2"]]));
    // DML does not move it.
    db.execute("INSERT INTO t VALUES(1)", []).unwrap();
    let after3 = render_rows(&db.query("PRAGMA schema_version", []).unwrap());
    assert_eq!(after3, srows(&[&["I:2"]]));
}

// ---------------------------------------------------------------------------
// Native reopen: the master surface after a file round-trip
// ---------------------------------------------------------------------------

#[test]
fn master_rootpage_survives_reopen() {
    let path = std::env::temp_dir().join("schema_parity_reopen.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)", [])
            .unwrap();
        db.execute("CREATE TABLE u (c INTEGER UNIQUE)", []).unwrap();
        db.execute("CREATE INDEX ix ON t(b)", []).unwrap();
        db.execute("INSERT INTO t VALUES(1, 'x')", []).unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        // The stored rows decode back to the same SQLite-convention
        // rootpages AND the tables still query correctly (the internal
        // -1 round-trip held).
        let rows = render_rows(
            &db.query(
                "SELECT type, name, rootpage FROM sqlite_master ORDER BY name",
                [],
            )
            .unwrap(),
        );
        // t=2, sqlite_autoindex_u_1=4, u=3, ix=5 in creation order.
        // ORDER BY name: ix < sqlite_autoindex_u_1 < t < u.
        assert_eq!(
            rows,
            srows(&[
                &["T:index", "T:ix", "I:5"],
                &["T:index", "T:sqlite_autoindex_u_1", "I:4"],
                &["T:table", "T:t", "I:2"],
                &["T:table", "T:u", "I:3"],
            ])
        );
        let data = render_rows(&db.query("SELECT a, b FROM t", []).unwrap());
        assert_eq!(data, srows(&[&["I:1", "T:x"]]));

        // DDL after reopen keeps allocating past the old objects.
        db.execute("CREATE TABLE w (x INTEGER)", []).unwrap();
        let w = render_rows(
            &db.query("SELECT rootpage FROM sqlite_master WHERE name = 'w'", [])
                .unwrap(),
        );
        assert_eq!(w, srows(&[&["I:6"]]));
    }
    let _ = std::fs::remove_file(&path);
}
