//! Differential probe: sqlite_master DDL text and PRAGMA result shapes,
//! engine vs real (bundled) SQLite. Prints every divergence; silent when
//! identical. Exit code 0 = no divergences, 1 = found some.

use rustqlite::{Database, Value};

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => format!("I:{i}"),
        Value::Real(f) => format!("R:{f}"),
        Value::Text(t) => format!("T:{t}"),
        Value::Blob(b) => format!("B:{}", b.len()),
    }
}

fn engine_master(setup: &[&str]) -> Vec<Vec<String>> {
    let mut db = Database::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
    }
    db.query(
        "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master ORDER BY name",
        [],
    )
    .unwrap()
    .iter()
    .map(|r| r.iter().map(render).collect())
    .collect()
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

fn engine_pragmas(setup: &[&str], pragmas: &[&str]) -> Vec<(String, Vec<Vec<String>>)> {
    let mut db = Database::open_in_memory().unwrap();
    for s in setup {
        db.execute(s, []).unwrap();
    }
    pragmas
        .iter()
        .map(|p| {
            let rows = db
                .query(p, [])
                .map(|rs| rs.iter().map(|r| r.iter().map(render).collect()).collect())
                .unwrap_or_default();
            (p.to_string(), rows)
        })
        .collect()
}

fn sqlite_pragmas(setup: &[&str], pragmas: &[&str]) -> Vec<(String, Vec<Vec<String>>)> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for s in setup {
        conn.execute_batch(s).unwrap();
    }
    pragmas
        .iter()
        .map(|p| {
            let mut rows = Vec::new();
            let mut stmt = match conn.prepare(p) {
                Ok(s) => s,
                Err(_) => return (p.to_string(), rows),
            };
            let ncols = stmt.column_count();
            if ncols == 0 {
                return (p.to_string(), rows);
            }
            let mut q = stmt.query([]).unwrap();
            while let Some(r) = q.next().unwrap() {
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
                rows.push(row);
            }
            (p.to_string(), rows)
        })
        .collect()
}

fn main() {
    let mut found = 0;

    // ---- DDL text shapes ----
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("simple", vec!["CREATE TABLE t (a INTEGER, b TEXT)"]),
        (
            "pk-alias",
            vec!["CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"],
        ),
        (
            "pk-inline",
            vec!["CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)"],
        ),
        (
            "pk-composite",
            vec!["CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a, b))"],
        ),
        (
            "pk-desc",
            vec!["CREATE TABLE t (a INTEGER, PRIMARY KEY(a DESC))"],
        ),
        (
            "not-null-default",
            vec!["CREATE TABLE t (a INTEGER NOT NULL DEFAULT 0, b TEXT DEFAULT 'x')"],
        ),
        (
            "quoted-ident",
            vec!["CREATE TABLE \"my table\" (\"select\" INTEGER, \"from\" TEXT)"],
        ),
        (
            "bracket-ident",
            vec!["CREATE TABLE [t2] ([a b] INTEGER)"],
        ),
        (
            "backtick-ident",
            vec!["CREATE TABLE `t3` (`c d` TEXT)"],
        ),
        (
            "unique-col",
            vec!["CREATE TABLE t (a INTEGER UNIQUE, b TEXT UNIQUE)"],
        ),
        (
            "unique-table",
            vec!["CREATE TABLE t (a INTEGER, b TEXT, UNIQUE(a, b))"],
        ),
        (
            "check-col",
            vec!["CREATE TABLE t (a INTEGER CHECK (a > 0))"],
        ),
        (
            "check-table",
            vec!["CREATE TABLE t (a INTEGER, b TEXT, CHECK (a > 0 AND b != 'x'))"],
        ),
        (
            "fk-inline",
            vec![
                "CREATE TABLE p (id INTEGER PRIMARY KEY)",
                "CREATE TABLE c (pid INTEGER REFERENCES p(id))",
            ],
        ),
        (
            "fk-clause",
            vec![
                "CREATE TABLE p (id INTEGER PRIMARY KEY)",
                "CREATE TABLE c (pid INTEGER, FOREIGN KEY(pid) REFERENCES p(id))",
            ],
        ),
        (
            "fk-actions",
            vec![
                "CREATE TABLE p (id INTEGER PRIMARY KEY)",
                "CREATE TABLE c (pid INTEGER REFERENCES p(id) ON DELETE CASCADE ON UPDATE SET NULL)",
            ],
        ),
        (
            "collate-col",
            vec!["CREATE TABLE t (a TEXT COLLATE NOCASE)"],
        ),
        (
            "wo-rowid",
            vec!["CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY(a)) WITHOUT ROWID"],
        ),
        (
            "wo-rowid-desc",
            vec!["CREATE TABLE t (a INTEGER PRIMARY KEY DESC, b TEXT) WITHOUT ROWID"],
        ),
        (
            "default-expr",
            vec!["CREATE TABLE t (a INTEGER DEFAULT (1+2), b TEXT DEFAULT (lower('X')))"],
        ),
        (
            "default-cast",
            vec!["CREATE TABLE t (a TEXT DEFAULT 'x' COLLATE NOCASE)"],
        ),
        (
            "autoinc",
            vec!["CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)"],
        ),
        (
            "gen-always",
            vec!["CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2))"],
        ),
        (
            "gen-as",
            vec!["CREATE TABLE t (a INTEGER, b INTEGER AS (a+1) VIRTUAL)"],
        ),
        (
            "gen-stored",
            vec!["CREATE TABLE t (a INTEGER, b INTEGER AS (a+1) STORED)"],
        ),
        (
            "index-plain",
            vec![
                "CREATE TABLE t (a INTEGER, b TEXT)",
                "CREATE INDEX ix ON t(a)",
            ],
        ),
        (
            "index-desc-collate",
            vec![
                "CREATE TABLE t (a INTEGER, b TEXT)",
                "CREATE INDEX ix ON t(a DESC, b COLLATE NOCASE)",
            ],
        ),
        (
            "index-expr",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE INDEX ix ON t(a*2)",
            ],
        ),
        (
            "index-partial",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE INDEX ix ON t(a) WHERE a > 10",
            ],
        ),
        (
            "index-unique-partial",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE UNIQUE INDEX ix ON t(a) WHERE a IS NOT NULL",
            ],
        ),
        (
            "view",
            vec!["CREATE TABLE t (a INTEGER)", "CREATE VIEW v AS SELECT a FROM t"],
        ),
        (
            "view-complex",
            vec![
                "CREATE TABLE t (a INTEGER, b TEXT)",
                "CREATE VIEW v(a, b) AS SELECT a, b FROM t WHERE a > 1",
            ],
        ),
        (
            "trigger-simple",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = a; END",
            ],
        ),
        (
            "trigger-when",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE TRIGGER tr BEFORE UPDATE OF a ON t WHEN a > 5 BEGIN SELECT 1; END",
            ],
        ),
        (
            "trigger-foreach",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE VIEW vv AS SELECT a FROM t",
                "CREATE TRIGGER tr INSTEAD OF DELETE ON vv FOR EACH ROW BEGIN SELECT 1; END",
            ],
        ),
        (
            "drop-recreate",
            vec![
                "CREATE TABLE t (a INTEGER)",
                "CREATE INDEX ix ON t(a)",
                "DROP INDEX ix",
                "CREATE INDEX ix ON t(a DESC)",
            ],
        ),
        (
            "table-x",
            vec![
                "CREATE TABLE t1 (a UNIQUE)",
                "CREATE TABLE t2 (b INTEGER PRIMARY KEY, c UNIQUE, d UNIQUE)",
            ],
        ),
        (
            "multiple-constraints",
            vec!["CREATE TABLE t (a INTEGER NOT NULL UNIQUE DEFAULT 5 CHECK (a != 0), b TEXT, PRIMARY KEY(a, b) ) WITHOUT ROWID"],
        ),
    ];

    for (name, setup) in &cases {
        let ours = engine_master(setup);
        let theirs = sqlite_master(setup);
        if ours != theirs {
            found += 1;
            println!("== DDL DIVERGENCE [{name}] ==");
            for (o, t) in ours.iter().zip(theirs.iter()) {
                if o != t {
                    println!("  ours:   {o:?}");
                    println!("  theirs: {t:?}");
                }
            }
            if ours.len() != theirs.len() {
                println!("  row count: ours {} theirs {}", ours.len(), theirs.len());
                println!("  ours full:   {ours:#?}");
                println!("  theirs full: {theirs:#?}");
            }
        }
    }

    // ---- PRAGMA result shapes ----
    let setup = vec![
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT NOT NULL DEFAULT 'x', c REAL UNIQUE, d BLOB COLLATE NOCASE)",
        "CREATE INDEX ix ON t(b, c DESC)",
        "CREATE UNIQUE INDEX uix ON t(c) WHERE c > 0",
        "CREATE TABLE p (id INTEGER PRIMARY KEY, k TEXT)",
        "CREATE TABLE c (pid INTEGER REFERENCES p(id) ON DELETE CASCADE, k TEXT, PRIMARY KEY(pid, k)) WITHOUT ROWID",
    ];
    let pragmas = vec![
        "PRAGMA table_info(t)",
        "PRAGMA table_info(p)",
        "PRAGMA table_info(c)",
        "PRAGMA table_xinfo(t)",
        "PRAGMA index_list(t)",
        "PRAGMA index_list(p)",
        "PRAGMA index_list(c)",
        "PRAGMA index_info(ix)",
        "PRAGMA index_xinfo(ix)",
        "PRAGMA index_info(uix)",
        "PRAGMA index_xinfo(uix)",
        "PRAGMA foreign_key_list(c)",
        "PRAGMA page_size",
        "PRAGMA page_count",
        "PRAGMA max_page_count",
        "PRAGMA encoding",
        "PRAGMA journal_mode",
        "PRAGMA auto_vacuum",
        "PRAGMA user_version",
        "PRAGMA application_id",
        "PRAGMA freelist_count",
        "PRAGMA schema_version",
        "PRAGMA integrity_check",
        "PRAGMA quick_check",
        "PRAGMA foreign_keys",
        "PRAGMA busy_timeout",
        "PRAGMA cache_size",
        "PRAGMA locking_mode",
        "PRAGMA synchronous",
        "PRAGMA temp_store",
        "PRAGMA pragma_list",
        "PRAGMA compile_options",
        "PRAGMA function_list",
        "PRAGMA module_list",
        "PRAGMA collation_list",
        "PRAGMA database_list",
        "PRAGMA data_version",
    ];

    let ours = engine_pragmas(&setup, &pragmas);
    let theirs = sqlite_pragmas(&setup, &pragmas);
    for ((pn, orows), (_, trows)) in ours.iter().zip(theirs.iter()) {
        if orows != trows {
            found += 1;
            println!("== PRAGMA DIVERGENCE [{pn}] ==");
            println!("  ours:   {orows:#?}");
            println!("  theirs: {trows:#?}");
        }
    }

    if found == 0 {
        println!(
            "ALL IDENTICAL ({} DDL cases, {} pragmas)",
            cases.len(),
            pragmas.len()
        );
    }
    std::process::exit(if found > 0 { 1 } else { 0 });
}
