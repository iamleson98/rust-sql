//! Probe: non-BINARY collation writes to SQLite-format files.
//!
//! For each collation shape (explicit index COLLATE, inherited column
//! collation, UNIQUE autoindex, WITHOUT ROWID PK, DESC combo), build a
//! table with adversarial mixed-case data in the engine, dump to a
//! SQLite-format file, and let REAL SQLite judge:
//!   1. PRAGMA integrity_check (verifies index b-tree order per collation)
//!   2. equality lookups that binary-search the index
//!   3. ORDER BY parity vs the engine's answer
//!   4. the file's own index scan order (forces the index path)

use rustqlite::{Database, Value};

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsql_collw_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(rustqlite::storage::sqlitefmt::reader::wal_path_of(&p));
    p
}

fn texts(db: &Database, sql: &str) -> Vec<String> {
    db.query(sql, ())
        .unwrap()
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.as_str().to_string(),
            o => panic!("expected TEXT got {o:?}"),
        })
        .collect()
}

fn sqlite_texts(con: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = con.prepare(sql).unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows
}

struct Case {
    name: &'static str,
    ddl: &'static str,
    insert: &'static str,
    /// engine-side ORDER BY query
    order_sql: &'static str,
    /// sqlite-side: force the index order (covering scan, no sort)
    index_scan_sql: &'static str,
}

const DATA: &str = "INSERT INTO t(v) VALUES ('apple'),('Banana'),('cherry'),('APPLE'),
    ('banana'),('Cherry'),('date'),('Date'),('éclair'),('Éclair'),
    ('apple pie'),('BANANA split')";

const CASES: &[Case] = &[
    Case {
        name: "explicit_index_nocase",
        ddl: "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
              CREATE INDEX ix ON t(v COLLATE NOCASE);",
        insert: DATA,
        order_sql: "SELECT v FROM t ORDER BY v COLLATE NOCASE",
        index_scan_sql: "SELECT v FROM t ORDER BY v COLLATE NOCASE", // engine answer
    },
    Case {
        name: "inherited_column_nocase",
        ddl: "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE);
              CREATE INDEX ix ON t(v);",
        insert: DATA,
        order_sql: "SELECT v FROM t ORDER BY v",
        index_scan_sql: "SELECT v FROM t ORDER BY v", // engine answer
    },
    Case {
        name: "unique_autoindex_nocase",
        ddl: "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT COLLATE NOCASE UNIQUE);",
        insert: "INSERT INTO t(v) VALUES ('apple'),('Banana'),('cherry'),('date'),('éclair'),
            ('apple pie'),('BANANA split')",
        order_sql: "SELECT v FROM t ORDER BY v",
        index_scan_sql: "SELECT v FROM t ORDER BY v",
    },
    Case {
        name: "without_rowid_pk_nocase",
        ddl: "CREATE TABLE t(v TEXT COLLATE NOCASE PRIMARY KEY, x INT) WITHOUT ROWID;",
        insert: "INSERT INTO t(v, x) VALUES ('apple',1),('Banana',2),('cherry',3),('date',4),
            ('éclair',5),('Apple pie',6),('BANANA split',7)",
        order_sql: "SELECT v FROM t ORDER BY v",
        index_scan_sql: "SELECT v FROM t ORDER BY v",
    },
    Case {
        name: "desc_nocase",
        ddl: "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
              CREATE INDEX ix ON t(v COLLATE NOCASE DESC);",
        insert: DATA,
        order_sql: "SELECT v FROM t ORDER BY v COLLATE NOCASE DESC",
        index_scan_sql: "SELECT v FROM t ORDER BY v COLLATE NOCASE DESC",
    },
];

fn main() {
    for c in CASES {
        println!("=== {} ===", c.name);
        let path = temp_path(c.name);
        // Build engine-side file: create engine db at a native path then
        // force sqlite-format output? Simplest: create with real SQLite to
        // set the file format, then open in engine, drop table, recreate.
        {
            let con = rusqlite::Connection::open(&path).unwrap();
            let _ = con.execute_batch("CREATE TABLE boot(x); DROP TABLE boot;");
        }
        let mut db = Database::open(&path).unwrap();
        for stmt in c.ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            db.execute(stmt, ()).unwrap();
        }
        for stmt in c.insert.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            db.execute(stmt, ()).unwrap();
        }
        let engine_order = texts(&db, c.order_sql);
        let lookups: Vec<(String, i64)> = [
            "apple", "APPLE", "Date", "date", "éclair", "Éclair", "banana",
        ]
        .iter()
        .map(|k| {
            let sql = format!("SELECT count(*) FROM t WHERE v = '{}'", k);
            let n = db.query(&sql, ()).unwrap();
            let cnt = match &n[0][0] {
                Value::Integer(i) => *i,
                o => panic!("{o:?}"),
            };
            (k.to_string(), cnt)
        })
        .collect();
        drop(db); // dump in SQLite format
        let _ = std::fs::copy(&path, format!("/tmp/keep_{}.sqlite", c.name));

        let con = rusqlite::Connection::open(&path).unwrap();
        let ic: String = con
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        println!("  integrity_check: {}", ic);
        if ic != "ok" {
            println!("  !! INTEGRITY FAILURE");
        }
        for (k, cnt) in &lookups {
            let sq: i64 = con
                .query_row(
                    &format!("SELECT count(*) FROM t WHERE v = '{}'", k),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if sq != *cnt {
                println!("  !! lookup mismatch: {} engine={} sqlite={}", k, cnt, sq);
            }
        }
        // index-scan order per real SQLite
        let sq_scan: Vec<String> = sqlite_texts(&con, c.index_scan_sql);
        if sq_scan != engine_order {
            println!("  !! ORDER mismatch:");
            println!("     engine: {:?}", engine_order);
            println!("     sqlite: {:?}", sq_scan);
        } else {
            println!("  order: OK ({} rows)", engine_order.len());
        }
        let _ = std::fs::remove_file(&path);
    }
    println!("probe done");
}
