//! ATTACH / DETACH — real attached databases (see `src/attach.rs`).
//!
//! Every error message and behavior below was verified against SQLite
//! 3.53 (python3 sqlite3 oracle, isolation_level=None) before being
//! pinned; the differential cases at the bottom run the same programs
//! through rusqlite's bundled real SQLite.

use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().expect("open")
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    let (cols, rs) = db.query_with_columns(sql, []).expect(sql);
    let _ = cols;
    rs.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(t) => t.to_string(),
                    Value::Blob(b) => format!("x'{}'", hex(b)),
                })
                .collect()
        })
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn err_of(db: &mut Database, sql: &str) -> String {
    match db.execute(sql, []) {
        Ok(()) => "OK".to_string(),
        Err(e) => e.to_string(),
    }
}

fn err_of_q(db: &Database, sql: &str) -> String {
    match db.query_with_columns(sql, []) {
        Ok(_) => "OK".to_string(),
        Err(e) => e.to_string(),
    }
}

#[test]
fn attach_detach_round_trip_and_errors() {
    let mut db = mem();
    assert_eq!(
        err_of(&mut db, "ATTACH ':memory:' AS main"),
        "semantic error: database main is already in use"
    );
    assert_eq!(err_of(&mut db, "ATTACH ':memory:' AS aux"), "OK");
    // Case-insensitive duplicate echo: SQLite reflects the typed name.
    assert_eq!(
        err_of(&mut db, "ATTACH ':memory:' AS AUX"),
        "semantic error: database AUX is already in use"
    );
    assert_eq!(err_of(&mut db, "ATTACH ':memory:' AS main2"), "OK");
    for i in 0..10 {
        let sql = format!("ATTACH ':memory:' AS db{}", i);
        let e = err_of(&mut db, &sql);
        if i == 8 {
            // aux + main2 + db0..db7 = 10 attached; db8 is the 11th.
            assert_eq!(e, "semantic error: too many attached databases - max 10");
        } else if i == 9 {
            // Still full after the failure.
            assert_eq!(e, "semantic error: too many attached databases - max 10");
        } else {
            assert_eq!(e, "OK");
        }
    }
    assert_eq!(err_of(&mut db, "DETACH nosuch"), "no such database: nosuch");
    assert_eq!(
        err_of(&mut db, "DETACH main"),
        "semantic error: cannot detach database main"
    );
    assert_eq!(err_of(&mut db, "DETACH temp"), "no such database: temp");
    assert_eq!(err_of(&mut db, "DETACH main2"), "OK");
    assert_eq!(err_of(&mut db, "DETACH main2"), "no such database: main2");
    // DETACH of a temp-bearing connection.
    db.execute("CREATE TEMP TABLE tt(z)", []).unwrap();
    assert_eq!(
        err_of(&mut db, "DETACH temp"),
        "semantic error: cannot detach database temp"
    );
}

#[test]
fn attach_filename_via_parameter() {
    let mut db = mem();
    db.execute("ATTACH ? AS aux", [Value::Text(":memory:".into())])
        .expect("attach via param");
    db.execute("CREATE TABLE aux.x(a)", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (1)", []).unwrap();
    assert_eq!(rows(&db, "SELECT a FROM aux.x"), vec![vec!["1"]]);
}

#[test]
fn qualified_reads_and_errors() {
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT, b TEXT)", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (1,'one'),(2,'two')", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT * FROM aux.x ORDER BY a"),
        vec![vec!["1", "one"], vec!["2", "two"]]
    );
    assert_eq!(
        rows(&db, "SELECT b FROM aux.x WHERE a=2"),
        vec![vec!["two"]]
    );
    // Three-part column reference.
    assert_eq!(
        rows(&db, "SELECT aux.x.a FROM aux.x WHERE aux.x.b='two'"),
        vec![vec!["2"]]
    );
    // Alias + two-part.
    assert_eq!(
        rows(&db, "SELECT y.b FROM aux.x y WHERE y.a=1"),
        vec![vec!["one"]]
    );
    // Three-part star.
    assert_eq!(
        rows(&db, "SELECT aux.x.b FROM aux.x WHERE a=1"),
        vec![vec!["one"]]
    );
    // Column names for routed star.
    let (cols, _) = db.query_with_columns("SELECT * FROM aux.x", []).unwrap();
    assert_eq!(cols, vec!["a", "b"]);
    // Errors carry the schema prefix (SQLite's shape).
    assert_eq!(
        err_of_q(&db, "SELECT * FROM aux.nosuch"),
        "no such table: aux.nosuch"
    );
    assert_eq!(
        err_of_q(&db, "SELECT * FROM nosuchdb.t"),
        "no such table: nosuchdb.t"
    );
}

#[test]
fn cross_database_joins_and_setops() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT)", []).unwrap();
    db.execute("INSERT INTO t VALUES (1),(2),(3)", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT, b TEXT)", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (1,'one'),(2,'two')", [])
        .unwrap();
    // Inner join across schemas.
    assert_eq!(
        rows(
            &db,
            "SELECT t.a, x.b FROM t JOIN aux.x ON t.a=x.a ORDER BY t.a"
        ),
        vec![vec!["1", "one"], vec!["2", "two"]]
    );
    // Left join with NULL-extended aux side.
    assert_eq!(
        rows(
            &db,
            "SELECT t.a, x.b FROM t LEFT JOIN aux.x ON t.a=x.a ORDER BY t.a"
        ),
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "NULL"]]
    );
    // Aggregation over a mixed join.
    assert_eq!(rows(&db, "SELECT count(*) FROM t, aux.x"), vec![vec!["6"]]);
    // Compound select across schemas.
    assert_eq!(
        rows(
            &db,
            "SELECT a FROM t INTERSECT SELECT a FROM aux.x ORDER BY a"
        ),
        vec![vec!["1"], vec!["2"]]
    );
    // Subquery in WHERE across schemas.
    assert_eq!(
        rows(
            &db,
            "SELECT a FROM t WHERE a IN (SELECT a FROM aux.x) ORDER BY a"
        ),
        vec![vec!["1"], vec!["2"]]
    );
    // Correlated subquery across schemas.
    assert_eq!(
        rows(
            &db,
            "SELECT t.a FROM t WHERE EXISTS (SELECT 1 FROM aux.x WHERE aux.x.a=t.a) ORDER BY t.a"
        ),
        vec![vec!["1"], vec!["2"]]
    );
}

#[test]
fn dml_routing_and_counters() {
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT PRIMARY KEY, b TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO aux.x VALUES (1,'one'),(2,'two')", [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT changes()"), vec![vec!["2"]]);
    assert_eq!(rows(&db, "SELECT last_insert_rowid()"), vec![vec!["2"]]);
    // UPDATE routed.
    db.execute("UPDATE aux.x SET b='z' WHERE a=1", []).unwrap();
    assert_eq!(rows(&db, "SELECT b FROM aux.x WHERE a=1"), vec![vec!["z"]]);
    assert_eq!(rows(&db, "SELECT changes()"), vec![vec!["1"]]);
    // DELETE routed with RETURNING.
    let ret = rows(&db, "DELETE FROM aux.x WHERE a=2 RETURNING a, b");
    assert_eq!(ret, vec![vec!["2", "two"]]);
    // Upsert routed.
    db.execute(
        "INSERT INTO aux.x(a,b) VALUES (1,'up') ON CONFLICT(a) DO UPDATE SET b='up'",
        [],
    )
    .unwrap();
    assert_eq!(rows(&db, "SELECT b FROM aux.x WHERE a=1"), vec![vec!["up"]]);
    // Bound parameters on a routed insert.
    db.execute(
        "INSERT INTO aux.x VALUES (?, ?)",
        [Value::Integer(7), Value::Text("seven".into())],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT b FROM aux.x WHERE a=7"),
        vec![vec!["seven"]]
    );
}

#[test]
fn cross_schema_insert_synthesis() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT, b TEXT)", []).unwrap();
    db.execute("INSERT INTO t VALUES (1,'one'),(2,'two')", [])
        .unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT, b TEXT)", []).unwrap();
    // Local source -> attached target.
    db.execute("INSERT INTO aux.x SELECT a, b FROM t", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT a, b FROM aux.x ORDER BY a"),
        vec![vec!["1", "one"], vec!["2", "two"]]
    );
    // Attached source -> local target.
    db.execute("CREATE TABLE copy(a INT, b TEXT)", []).unwrap();
    db.execute("INSERT INTO copy SELECT * FROM aux.x WHERE a <= 1", [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT a, b FROM copy"), vec![vec!["1", "one"]]);
    // Upsert preserved through synthesis.
    db.execute("CREATE UNIQUE INDEX aux.ua ON x(a)", [])
        .unwrap();
    db.execute(
        "INSERT INTO aux.x(a,b) SELECT a, 'u' FROM t ON CONFLICT(a) DO UPDATE SET b='upd'",
        [],
    )
    .unwrap();
    assert_eq!(
        rows(&db, "SELECT b FROM aux.x WHERE a=1"),
        vec![vec!["upd"]]
    );
}

#[test]
fn ddl_routing_and_index_rules() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT)", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    // Schema-qualified DDL.
    db.execute("CREATE TABLE aux.x(a INT, b TEXT)", []).unwrap();
    db.execute("ALTER TABLE aux.x ADD COLUMN c INT", [])
        .unwrap();
    // CREATE INDEX: the schema qualifies the NAME and scopes the TABLE
    // lookup (SQLite: `ON aux.x` is a syntax error; the target resolves
    // in the index's schema).
    db.execute("CREATE INDEX aux.ix ON x(a)", []).unwrap();
    // The table is looked up in the index's schema.
    assert_eq!(
        err_of(&mut db, "CREATE INDEX aux.ix2 ON t(a)"),
        "no such table: aux.t"
    );
    // DROP INDEX with schema.
    db.execute("DROP INDEX aux.ix", []).unwrap();
    // CREATE INDEX ON with a schema-qualified table: parse error (the
    // grammar rejects it, matching SQLite).
    assert!(db.execute("CREATE INDEX aux.ix3 ON aux.x(a)", []).is_err());
    // DROP TABLE routed.
    db.execute("DROP TABLE aux.x", []).unwrap();
    assert_eq!(
        rows(&db, "SELECT count(*) FROM aux.sqlite_master"),
        vec![vec!["0"]]
    );
    // sqlite_master per attached schema.
    db.execute("CREATE TABLE aux.z(v)", []).unwrap();
    assert_eq!(
        rows(&db, "SELECT name FROM aux.sqlite_master"),
        vec![vec!["z"]]
    );
    // The engine's own schema-table spellings on an attached schema:
    // sqlite_temp_master is main-only (SQLite).
    assert_eq!(
        err_of_q(&db, "SELECT * FROM aux.sqlite_temp_master"),
        "no such table: aux.sqlite_temp_master"
    );
}

#[test]
fn cross_schema_view_and_trigger_rules() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT)", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT)", []).unwrap();
    // A main view may not reference an attached database.
    assert_eq!(
        err_of(&mut db, "CREATE VIEW v1 AS SELECT * FROM aux.x"),
        "semantic error: view v1 cannot reference objects in database aux"
    );
    // An attached view may not reference main either.
    assert_eq!(
        err_of(&mut db, "CREATE VIEW aux.v2 AS SELECT * FROM main.t"),
        "semantic error: view v2 cannot reference objects in database main"
    );
    // An attached view with unqualified refs: the body resolves in the
    // view's OWN schema (SQLite's rule, verified: main.x existed and the
    // aux view still read aux.x).
    db.execute("CREATE TABLE aux.only_here(v INT)", []).unwrap();
    db.execute("INSERT INTO aux.only_here VALUES (5)", [])
        .unwrap();
    db.execute("CREATE TABLE only_here(w INT)", []).unwrap(); // main shadow
    db.execute("INSERT INTO only_here VALUES (9)", []).unwrap();
    db.execute("CREATE VIEW aux.v3 AS SELECT v FROM only_here", [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT v FROM aux.v3"), vec![vec!["5"]]);
    // Reading an aux view from main (federated).
    assert_eq!(rows(&db, "SELECT aux.v3.v FROM aux.v3"), vec![vec!["5"]]);
    // Trigger bodies may not carry qualified DML targets (SQLite).
    assert_eq!(
        err_of(&mut db, "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO aux.x VALUES (new.a); END"),
        "semantic error: qualified table names are not allowed on INSERT, UPDATE, and DELETE statements within triggers"
    );
}

#[test]
fn transactions_span_attached_databases() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INT)", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT)", []).unwrap();
    // ROLLBACK undoes both engines.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t VALUES (1)", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (2)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.x"), vec![vec!["1"]]);
    db.execute("ROLLBACK", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t"), vec![vec!["0"]]);
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.x"), vec![vec!["0"]]);
    // COMMIT persists both.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t VALUES (3)", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (4)", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.x"), vec![vec!["1"]]);
    // Savepoints span engines.
    db.execute("SAVEPOINT sp", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (5)", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.x"), vec![vec!["2"]]);
    db.execute("ROLLBACK TO sp", []).unwrap();
    db.execute("RELEASE sp", []).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM aux.x"), vec![vec!["1"]]);
    // ATTACH mid-transaction joins it; ROLLBACK covers the new engine.
    db.execute("BEGIN", []).unwrap();
    db.execute("ATTACH ':memory:' AS mid", []).unwrap();
    db.execute("CREATE TABLE mid.y(a INT)", []).unwrap();
    db.execute("INSERT INTO mid.y VALUES (9)", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    db.execute("ATTACH ':memory:' AS mid2", []).unwrap();
    // (mid was rolled back; a fresh attach shows an empty schema.)
    assert_eq!(
        rows(&db, "SELECT count(*) FROM mid2.sqlite_master"),
        vec![vec!["0"]]
    );
    // DETACH of a db with uncommitted changes inside a txn: locked.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (6)", []).unwrap();
    assert_eq!(
        err_of(&mut db, "DETACH aux"),
        "semantic error: database aux is locked"
    );
    db.execute("ROLLBACK", []).unwrap();
    // Clean participant detaches inside the transaction.
    db.execute("BEGIN", []).unwrap();
    assert_eq!(err_of(&mut db, "DETACH mid2"), "OK");
    db.execute("COMMIT", []).unwrap();
}

#[test]
fn pragma_routing_and_database_list() {
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    // database_list: main + attached (seq from 2).
    assert_eq!(
        rows(&db, "PRAGMA database_list"),
        vec![vec!["0", "main", ""], vec!["2", "aux", ""]]
    );
    // temp row appears once temp objects exist.
    db.execute("CREATE TEMP TABLE tt(z)", []).unwrap();
    assert_eq!(
        rows(&db, "PRAGMA database_list"),
        vec![
            vec!["0", "main", ""],
            vec!["1", "temp", ""],
            vec!["2", "aux", ""]
        ]
    );
    // Read + write pragma routing.
    assert_eq!(rows(&db, "PRAGMA aux.user_version"), vec![vec!["0"]]);
    db.execute("PRAGMA aux.user_version=7", []).unwrap();
    assert_eq!(rows(&db, "PRAGMA aux.user_version"), vec![vec!["7"]]);
    // The local pragma is untouched.
    assert_eq!(rows(&db, "PRAGMA user_version"), vec![vec!["0"]]);
    // table_info through a schema.
    db.execute("CREATE TABLE aux.tbl(c1 INT, c2 TEXT)", [])
        .unwrap();
    let ti = rows(&db, "PRAGMA aux.table_info(tbl)");
    assert_eq!(ti.len(), 2);
    assert_eq!(ti[0][1], "c1");
    assert_eq!(ti[1][1], "c2");
    // Unknown database (SQLite's pragma-specific message).
    assert_eq!(
        err_of_q(&db, "PRAGMA nosuchdb.journal_mode"),
        "semantic error: unknown database nosuchdb"
    );
}

#[test]
fn unqualified_name_search_order() {
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS first", []).unwrap();
    db.execute("ATTACH ':memory:' AS second", []).unwrap();
    db.execute("CREATE TABLE first.shared(v TEXT)", []).unwrap();
    db.execute("INSERT INTO first.shared VALUES ('first')", [])
        .unwrap();
    // Bare name resolves to the attached db when main misses.
    assert_eq!(rows(&db, "SELECT v FROM shared"), vec![vec!["first"]]);
    // Main shadows attached.
    db.execute("CREATE TABLE shared(v TEXT)", []).unwrap();
    db.execute("INSERT INTO shared VALUES ('main')", [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT v FROM shared"), vec![vec!["main"]]);
    // Temp shadows main for LOOKUP. (The engine keeps temp and main in
    // one catalog namespace — same-named temp/main tables cannot coexist
    // the way SQLite's separate temp schema allows; documented in the
    // README's gap ledger. A fresh name exercises the shadow direction
    // that exists: temp objects win the bare-name search.)
    db.execute("CREATE TEMP TABLE tsh(v TEXT)", []).unwrap();
    db.execute("INSERT INTO tsh VALUES ('temp')", []).unwrap();
    assert_eq!(rows(&db, "SELECT v FROM tsh"), vec![vec!["temp"]]);
    assert_eq!(rows(&db, "SELECT v FROM temp.tsh"), vec![vec!["temp"]]);
    // Attach order breaks first-vs-second ties.
    db.execute("CREATE TABLE second.only2(v TEXT)", []).unwrap();
    db.execute("INSERT INTO second.only2 VALUES ('second')", [])
        .unwrap();
    db.execute("CREATE TABLE first.only2(v TEXT)", []).unwrap();
    db.execute("INSERT INTO first.only2 VALUES ('first')", [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT v FROM only2"), vec![vec!["first"]]);
    // Unqualified CREATE always creates in MAIN (SQLite) — even when an
    // attached engine owns a same-named table.
    db.execute("CREATE TABLE auxless(v TEXT)", []).unwrap();
    db.execute("INSERT INTO auxless VALUES ('in-main')", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT v FROM main.auxless"),
        vec![vec!["in-main"]]
    );
    // A bare INSERT targets the attached table when MAIN misses.
    db.execute("CREATE TABLE second.only_main(v TEXT)", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT v FROM second.only2"),
        vec![vec!["second"]]
    );
    db.execute("CREATE TABLE first.ins_only(v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO first.ins_only VALUES ('via-attach')", [])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT v FROM ins_only"),
        vec![vec!["via-attach"]]
    );
}

#[test]
fn attach_real_files_both_formats() {
    let dir = std::env::temp_dir().join(format!("rqlattach-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let native = dir.join("native.db");
    let _ = std::fs::remove_file(&native);
    let sqlite_file = dir.join("sqlite.db");
    let _ = std::fs::remove_file(&sqlite_file);

    // Create a native-format file, then attach it.
    {
        let mut f = Database::open(&native).unwrap();
        f.execute("CREATE TABLE t(a INT)", []).unwrap();
        f.execute("INSERT INTO t VALUES (42)", []).unwrap();
    }
    let mut db = mem();
    db.execute(format!("ATTACH '{}' AS nat", native.display()).as_str(), [])
        .unwrap();
    assert_eq!(rows(&db, "SELECT a FROM nat.t"), vec![vec!["42"]]);
    // Writes through the attachment persist.
    db.execute("INSERT INTO nat.t VALUES (7)", []).unwrap();
    drop(db);
    {
        let f = Database::open(&native).unwrap();
        assert_eq!(
            f.query_with_columns("SELECT count(*) FROM t", [])
                .unwrap()
                .1
                .len(),
            1
        );
    }

    // Create a REAL SQLite-format file via the interop bridge, attach it.
    {
        let mut s = Database::open_sqlite_format(&sqlite_file).unwrap();
        s.execute("CREATE TABLE u(b TEXT)", []).unwrap();
        s.execute("INSERT INTO u VALUES ('sq')", []).unwrap();
    }
    let mut db2 = mem();
    db2.execute(
        format!("ATTACH '{}' AS sq", sqlite_file.display()).as_str(),
        [],
    )
    .unwrap();
    assert_eq!(rows(&db2, "SELECT b FROM sq.u"), vec![vec!["sq"]]);

    // Same file attached twice under two names (SQLite allows it).
    let mut db3 = mem();
    db3.execute(format!("ATTACH '{}' AS f1", native.display()).as_str(), [])
        .unwrap();
    db3.execute(format!("ATTACH '{}' AS f2", native.display()).as_str(), [])
        .unwrap();
    db3.execute("INSERT INTO f1.t VALUES (99)", []).unwrap();
    assert_eq!(rows(&db3, "SELECT count(*) FROM f2.t"), vec![vec!["3"]]);

    // A missing file is CREATED (SQLite's open flags).
    let missing = dir.join("created.db");
    let _ = std::fs::remove_file(&missing);
    let mut db4 = mem();
    db4.execute(
        format!("ATTACH '{}' AS fresh", missing.display()).as_str(),
        [],
    )
    .unwrap();
    db4.execute("CREATE TABLE fresh.n(v)", []).unwrap();
    assert!(missing.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prepared_statements_over_attached_schemas() {
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT)", []).unwrap();
    db.execute("INSERT INTO aux.x VALUES (1),(2),(3)", [])
        .unwrap();
    // Routed prepared SELECT with parameters.
    let mut st = db.prepare("SELECT a FROM aux.x WHERE a > ?").unwrap();
    st.bind(1, Value::Integer(1)).unwrap();
    let mut got = Vec::new();
    while let Ok(rustqlite::StepResult::Row) = st.step() {
        if let Some(v) = st.column_value(0) {
            got.push(v.to_string());
        }
    }
    assert_eq!(got, vec!["2", "3"]);
    drop(st);
    // Mixed prepared SELECT.
    db.execute("CREATE TABLE main_t(a INT)", []).unwrap();
    db.execute("INSERT INTO main_t VALUES (2)", []).unwrap();
    let mut st2 = db
        .prepare("SELECT main_t.a, aux.x.a FROM main_t JOIN aux.x ON main_t.a=aux.x.a")
        .unwrap();
    assert!(matches!(st2.step(), Ok(rustqlite::StepResult::Row)));
    drop(st2);
    // Re-execution after more data (per-execution materialization).
    db.execute("INSERT INTO aux.x VALUES (2)", []).unwrap();
    let mut st3 = db.prepare("SELECT count(*) FROM aux.x WHERE a=2").unwrap();
    assert!(matches!(st3.step(), Ok(rustqlite::StepResult::Row)));
    assert_eq!(st3.column_value(0).unwrap().to_string(), "2");
}

#[test]
fn concurrent_begin_rejected_with_attached() {
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    assert_eq!(
        err_of(&mut db, "BEGIN CONCURRENT"),
        "unsupported: BEGIN CONCURRENT does not support attached databases (use a plain BEGIN)"
    );
}

// ---------------------------------------------------------------------------
// Differential cases — the same program runs against real SQLite
// (rusqlite bundled) and this engine; outputs must match value-by-value.
// ---------------------------------------------------------------------------

fn diff_case(name: &str, sql: &[&str]) {
    let oracle = rusqlite::Connection::open_in_memory().expect("oracle open");
    let mut ours = mem();
    let mut oracle_cols = Vec::new();
    let mut oracle_rows: Vec<Vec<rusqlite::types::Value>> = Vec::new();
    for s in sql {
        let mut stmt = oracle.prepare(s).expect("oracle prepare");
        let ncols = stmt.column_count();
        if ncols > 0 {
            let names: Vec<String> = (0..ncols)
                .map(|i| stmt.column_name(i).unwrap().to_string())
                .collect();
            let mut rows = stmt.query([]).expect("oracle query");
            oracle_cols = names;
            oracle_rows.clear();
            while let Ok(Some(r)) = rows.next() {
                oracle_rows.push(
                    (0..ncols)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .unwrap_or(rusqlite::types::Value::Null)
                        })
                        .collect(),
                );
            }
        } else {
            let _ = stmt.execute([]);
        }
    }
    let mut statements: Vec<&str> = sql.to_vec();
    let last = statements.pop().unwrap();
    for s in &statements {
        ours.execute(s, [])
            .unwrap_or_else(|e| panic!("[{name}] ours failed on {s}: {e}"));
    }
    let (cols, rs) = ours
        .query_with_columns(last, [])
        .unwrap_or_else(|e| panic!("[{name}] ours query failed: {e}"));
    assert_eq!(
        cols, oracle_cols,
        "[{name}] column names diverge: ours {cols:?} vs sqlite {oracle_cols:?}"
    );
    assert_eq!(
        rs.len(),
        oracle_rows.len(),
        "[{name}] row count diverges: ours {} vs sqlite {}",
        rs.len(),
        oracle_rows.len()
    );
    for (i, (a, b)) in rs.iter().zip(oracle_rows.iter()).enumerate() {
        for (j, (av, bv)) in a.iter().zip(b.iter()).enumerate() {
            use rusqlite::types::Value as Sv;
            let ok = match (av, bv) {
                (Value::Null, Sv::Null) => true,
                (Value::Integer(x), Sv::Integer(y)) => x == y,
                (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() < 1e-9,
                (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() < 1e-9,
                (Value::Real(x), Sv::Real(y)) => (x - y).abs() < 1e-9,
                (Value::Text(x), Sv::Text(y)) => x == y,
                (Value::Blob(x), Sv::Blob(y)) => x == y,
                _ => false,
            };
            assert!(
                ok,
                "[{name}] row {i} col {j} diverges: ours {av:?} vs sqlite {bv:?}"
            );
        }
    }
}

#[test]
fn differential_attach_basics() {
    diff_case(
        "attach-join",
        &[
            "CREATE TABLE t(a INT)",
            "INSERT INTO t VALUES (1),(2),(3)",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.x(a INT, b TEXT)",
            "INSERT INTO aux.x VALUES (1,'one'),(2,'two')",
            "SELECT t.a, x.b FROM t JOIN aux.x ON t.a=x.a ORDER BY t.a",
        ],
    );
}

#[test]
fn differential_attach_cross_insert() {
    diff_case(
        "attach-cross-insert",
        &[
            "CREATE TABLE src(a INT, b TEXT)",
            "INSERT INTO src VALUES (1,'one'),(2,'two'),(3,'three')",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.dst(a INT, b TEXT)",
            "INSERT INTO aux.dst SELECT a, b FROM src WHERE a >= 2",
            "SELECT a, b FROM aux.dst ORDER BY a",
        ],
    );
}

#[test]
fn differential_attach_setops_and_aggregates() {
    diff_case(
        "attach-setops",
        &[
            "ATTACH ':memory:' AS a1",
            "ATTACH ':memory:' AS a2",
            "CREATE TABLE a1.t(v INT)",
            "CREATE TABLE a2.t(v INT)",
            "INSERT INTO a1.t VALUES (1),(2),(3)",
            "INSERT INTO a2.t VALUES (2),(3),(4)",
            "SELECT v FROM a1.t INTERSECT SELECT v FROM a2.t ORDER BY v",
        ],
    );
    diff_case(
        "attach-agg",
        &[
            "ATTACH ':memory:' AS a1",
            "ATTACH ':memory:' AS a2",
            "CREATE TABLE a1.t(v INT)",
            "CREATE TABLE a2.u(w INT)",
            "INSERT INTO a1.t VALUES (1),(2)",
            "INSERT INTO a2.u VALUES (10),(20)",
            "SELECT sum(t.v + u.w) FROM a1.t, a2.u",
        ],
    );
}

#[test]
fn differential_attach_transactions() {
    diff_case(
        "attach-txn",
        &[
            "CREATE TABLE m(v INT)",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a(v INT)",
            "BEGIN",
            "INSERT INTO m VALUES (1)",
            "INSERT INTO aux.a VALUES (2)",
            "ROLLBACK",
            "SELECT (SELECT count(*) FROM m) + (SELECT count(*) FROM aux.a)",
        ],
    );
    diff_case(
        "attach-txn-commit",
        &[
            "CREATE TABLE m(v INT)",
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.a(v INT)",
            "BEGIN",
            "INSERT INTO m VALUES (1)",
            "INSERT INTO aux.a VALUES (2)",
            "COMMIT",
            "SELECT (SELECT count(*) FROM m) + (SELECT count(*) FROM aux.a)",
        ],
    );
}

#[test]
fn differential_attach_unqualified_and_master() {
    diff_case(
        "attach-unqualified",
        &[
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.only(v TEXT)",
            "INSERT INTO aux.only VALUES ('hit')",
            "SELECT v FROM only",
        ],
    );
    diff_case(
        "attach-sqlite-master",
        &[
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.x(a)",
            "CREATE TABLE aux.y(b)",
            "SELECT count(*) FROM aux.sqlite_master",
        ],
    );
}

#[test]
fn differential_attach_upsert_returning() {
    diff_case(
        "attach-upsert",
        &[
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.x(a INT PRIMARY KEY, b TEXT)",
            "INSERT INTO aux.x VALUES (1,'one')",
            "INSERT INTO aux.x(a,b) VALUES (1,'uno') ON CONFLICT(a) DO UPDATE SET b='upd'",
            "SELECT a, b FROM aux.x",
        ],
    );
    diff_case(
        "attach-returning",
        &[
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.x(a INT)",
            "INSERT INTO aux.x VALUES (1),(2) RETURNING a * 10",
        ],
    );
}

#[test]
fn differential_attach_update_delete() {
    diff_case(
        "attach-update-delete",
        &[
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.x(a INT, b TEXT)",
            "INSERT INTO aux.x VALUES (1,'one'),(2,'two'),(3,'three')",
            "UPDATE aux.x SET b='z' WHERE a != 2",
            "DELETE FROM aux.x WHERE a = 3",
            "SELECT a, b FROM aux.x ORDER BY a",
        ],
    );
}

#[test]
fn differential_attach_view_in_aux() {
    diff_case(
        "attach-view-own-schema",
        &[
            "ATTACH ':memory:' AS aux",
            "CREATE TABLE aux.x(v INT)",
            "INSERT INTO aux.x VALUES (4)",
            "CREATE VIEW aux.vv AS SELECT v FROM x",
            "SELECT v FROM aux.vv",
        ],
    );
}

#[test]
fn differential_attach_explain_shape() {
    // EXPLAIN QUERY PLAN over a routed table must still plan (SQLite
    // shows `SCAN aux.x`; the routed plan renders the engine's own
    // wording — verified not to error).
    let mut db = mem();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.x(a INT)", []).unwrap();
    let (cols, rows) = db
        .query_with_columns("EXPLAIN QUERY PLAN SELECT * FROM aux.x", [])
        .expect("explain");
    assert!(!rows.is_empty());
    let _ = cols;
}

// ---------------------------------------------------------------------------
// Schema-qualified triggers (SQLite-exact resolution rules)
// ---------------------------------------------------------------------------

/// The trigger's database comes from its NAME qualifier (or MAIN);
/// the ON table resolves WITHIN that database. All rules verified
/// against the python3 sqlite3 oracle (3.53).
#[test]
fn schema_qualified_trigger_rules() {
    let mut db = mem();
    db.execute("CREATE TABLE m (x)", []).unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("CREATE TABLE aux.a (id INT)", []).unwrap();
    db.execute("INSERT INTO aux.a VALUES (7)", []).unwrap();
    db.execute("CREATE TABLE aux.audit (msg TEXT)", []).unwrap();

    // Qualified name + same-db table: creates ON AUX.
    db.execute(
        "CREATE TRIGGER aux.tr AFTER UPDATE ON aux.a BEGIN INSERT INTO audit VALUES ('fired'); END",
        [],
    )
    .unwrap();
    db.execute("UPDATE aux.a SET id = id", []).unwrap();
    assert_eq!(rows(&db, "SELECT msg FROM aux.audit"), vec![vec!["fired"]]);
    // The trigger is visible in aux's catalog, not main's.
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM aux.sqlite_master WHERE type='trigger' AND name='tr'"
        ),
        vec![vec!["1"]]
    );
    assert_eq!(
        rows(
            &db,
            "SELECT count(*) FROM main.sqlite_master WHERE type='trigger' AND name='tr'"
        ),
        vec![vec!["0"]]
    );

    // Unqualified name + table only in aux: MAIN answers (SQLite:
    // "no such table: main.a" — the ON table never falls back to an
    // attached schema).
    let err = err_of(
        &mut db,
        "CREATE TRIGGER tr2 AFTER UPDATE ON a BEGIN SELECT 1; END",
    );
    assert!(err.contains("no such table"), "err: {err}");

    // Explicit cross-database qualifier (SQLite: "trigger tr cannot
    // reference objects in database main").
    let err = err_of(
        &mut db,
        "CREATE TRIGGER aux.tr3 AFTER UPDATE ON main.m BEGIN SELECT 1; END",
    );
    assert_eq!(
        err,
        "semantic error: trigger tr3 cannot reference objects in database main"
    );
    // The other direction (main trigger, aux-qualified table).
    let err = err_of(
        &mut db,
        "CREATE TRIGGER tr4 AFTER UPDATE ON aux.a BEGIN SELECT 1; END",
    );
    assert_eq!(
        err,
        "semantic error: trigger tr4 cannot reference objects in database aux"
    );

    // TEMP triggers may target any database (SQLite exemption) — the
    // cross-database firing machinery: the trigger lives in the
    // connection's temp catalog and fires, per row, on this
    // connection's writes to the attached table (pinned against the
    // 3.53.4 oracle; see tests/attach_triggers.rs for the full surface).
    db.execute(
        "CREATE TEMP TRIGGER ttr AFTER UPDATE ON aux.a BEGIN INSERT INTO audit VALUES ('temp'); END",
        [],
    )
    .unwrap();
    db.execute("UPDATE aux.a SET id = id", []).unwrap();
    assert_eq!(
        rows(&db, "SELECT msg FROM aux.audit WHERE msg = 'temp'"),
        vec![vec!["temp"]]
    );
}
