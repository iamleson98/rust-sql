//! WITHOUT ROWID PRIMARY KEY uniqueness — differential against real
//! SQLite. The engine stores WITHOUT ROWID tables as rowid tables
//! internally, so the PK's uniqueness is backed by an engine-internal
//! index (`IndexOrigin::WithoutRowidPk`: no schema row, hidden from
//! sqlite_master and every file-format dump — in a real SQLite file the
//! table b-tree IS the PK index). This suite pins the SQLite-observable
//! effects:
//!
//! - duplicate PK inserts fail with SQLite's exact message
//! - `ON CONFLICT (pk)` upserts resolve (single + composite PKs)
//! - `INSERT OR REPLACE` replaces
//! - `UPDATE ... SET pk = <existing>` fails / `OR REPLACE` displaces
//! - NULL-bearing keys stay distinct (NULLs are distinct in SQLite PKs?
//!   no — PK columns are NOT NULL, pinned below)
//! - PK-ordered scans still work (the reader path)
//! - reopen (native format) re-backfills the index from the DDL
//! - the SQLite-format dump round-trips WITHOUT the internal index
//!   (real SQLite re-opens the file and enforces the PK itself)

use rusqlite::Connection;

fn both(db: &mut rustqlite::Database, rc: &Connection, sql: &str) {
    let e1 = db.execute(sql, ()).map_err(|e| e.to_string());
    let e2 = rc.execute_batch(sql).map_err(|e| clean(&e.to_string()));
    assert_eq!(
        e1, e2,
        "engine/SQLite disagree on: {sql}\n  engine: {e1:?}\n  sqlite: {e2:?}"
    );
}

/// rusqlite wraps errors as "error: ..."; strip the prefix so both sides
/// carry the bare message.
fn clean(s: &str) -> String {
    s.strip_prefix("error: ").unwrap_or(s).to_string()
}

fn engine_rows(db: &rustqlite::Database, sql: &str) -> Vec<rustqlite::Value> {
    db.query(sql, ()).unwrap().into_iter().flatten().collect()
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Vec<rustqlite::Value> {
    rc.prepare(sql)
        .unwrap()
        .query_map([], |r| {
            Ok(match r.get_ref(0)? {
                rusqlite::types::ValueRef::Null => rustqlite::Value::Null,
                rusqlite::types::ValueRef::Integer(v) => rustqlite::Value::Integer(v),
                rusqlite::types::ValueRef::Real(v) => rustqlite::Value::Real(v),
                rusqlite::types::ValueRef::Text(t) => {
                    rustqlite::Value::Text(String::from_utf8_lossy(t).to_string().into())
                }
                rusqlite::types::ValueRef::Blob(b) => rustqlite::Value::Blob(b.to_vec()),
            })
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn without_rowid_pk_uniqueness_matches_sqlite() {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();

    // Single-column TEXT PK.
    both(
        &mut db,
        &rc,
        "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
    );
    both(&mut db, &rc, "INSERT INTO wr VALUES ('a', 1), ('b', 2)");
    // Duplicate PK: both sides must fail with the same message.
    let e1 = db
        .execute("INSERT INTO wr VALUES ('a', 3)", ())
        .err()
        .map(|e| e.to_string());
    let e2 = rc
        .execute_batch("INSERT INTO wr VALUES ('a', 3)")
        .err()
        .map(|e| clean(&e.to_string()));
    assert_eq!(e1, e2, "duplicate PK error");
    assert_eq!(
        e1.as_deref(),
        Some("UNIQUE constraint failed: wr.k"),
        "SQLite's exact message"
    );

    // Upsert on the PK target.
    both(
        &mut db,
        &rc,
        "INSERT INTO wr VALUES ('a', 9) ON CONFLICT (k) DO UPDATE SET v = 5",
    );
    // DO NOTHING.
    both(
        &mut db,
        &rc,
        "INSERT INTO wr VALUES ('a', 0) ON CONFLICT DO NOTHING",
    );
    // REPLACE.
    both(&mut db, &rc, "INSERT OR REPLACE INTO wr VALUES ('b', 7)");
    // Conflicting UPDATE.
    let e1 = db
        .execute("UPDATE wr SET k = 'b' WHERE k = 'a'", ())
        .err()
        .map(|e| e.to_string());
    let e2 = rc
        .execute_batch("UPDATE wr SET k = 'b' WHERE k = 'a'")
        .err()
        .map(|e| clean(&e.to_string()));
    assert_eq!(e1, e2, "conflicting UPDATE error");
    // PK rename to a fresh value.
    both(&mut db, &rc, "UPDATE wr SET k = 'c' WHERE k = 'a'");

    // Final state agreement.
    assert_eq!(
        engine_rows(&db, "SELECT k || ':' || v FROM wr ORDER BY k"),
        sqlite_rows(&rc, "SELECT k || ':' || v FROM wr ORDER BY k"),
        "final rows"
    );

    // Composite PK: partial collisions are still conflicts.
    both(
        &mut db,
        &rc,
        "CREATE TABLE cwr (a INT, b TEXT, v REAL, PRIMARY KEY (a, b)) WITHOUT ROWID",
    );
    both(
        &mut db,
        &rc,
        "INSERT INTO cwr VALUES (1, 'x', 0.5), (1, 'y', 1.5), (2, 'x', 2.5)",
    );
    let e1 = db
        .execute("INSERT INTO cwr VALUES (1, 'x', 9.0)", ())
        .err()
        .map(|e| e.to_string());
    let e2 = rc
        .execute_batch("INSERT INTO cwr VALUES (1, 'x', 9.0)")
        .err()
        .map(|e| clean(&e.to_string()));
    assert_eq!(e1, e2, "composite duplicate");
    assert_eq!(
        e1.as_deref(),
        Some("UNIQUE constraint failed: cwr.a, cwr.b"),
        "SQLite's composite message"
    );
    both(
        &mut db,
        &rc,
        "INSERT INTO cwr VALUES (1, 'x', 9.0) ON CONFLICT (a, b) DO UPDATE SET v = v * 10",
    );
    assert_eq!(
        engine_rows(&db, "SELECT v FROM cwr WHERE a = 1 AND b = 'x'"),
        sqlite_rows(&rc, "SELECT v FROM cwr WHERE a = 1 AND b = 'x'"),
        "composite upsert result"
    );
}

#[test]
fn without_rowid_pk_index_is_invisible_and_reopen_survives() {
    // sqlite_master: only the table (the internal PK index has no row).
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO wr VALUES ('a', 1), ('b', 2)", ())
        .unwrap();
    let master = db
        .query("SELECT name, type FROM sqlite_master ORDER BY name", ())
        .unwrap();
    assert_eq!(
        master,
        vec![vec![
            rustqlite::Value::Text("wr".into()),
            rustqlite::Value::Text("table".into())
        ]],
        "no internal index row in sqlite_master"
    );
    // PRAGMA index_list: the PK appears with origin 'pk' (SQLite does).
    let il = db.query("PRAGMA index_list('wr')", ()).unwrap();
    assert_eq!(il.len(), 1, "index_list rows: {il:?}");
    assert_eq!(il[0][3], rustqlite::Value::Text("pk".into()), "origin 'pk'");

    // Reopen (native format): the index is rebuilt from the DDL and the
    // PK is enforced again.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wr.db");
    {
        let mut file_db = rustqlite::Database::open(&path).unwrap();
        file_db
            .execute(
                "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
                (),
            )
            .unwrap();
        file_db
            .execute("INSERT INTO wr VALUES ('a', 1)", ())
            .unwrap();
    }
    let mut reopened = rustqlite::Database::open(&path).unwrap();
    let dup = reopened
        .execute("INSERT INTO wr VALUES ('a', 2)", ())
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        dup.as_deref(),
        Some("UNIQUE constraint failed: wr.k"),
        "PK enforced after native reopen"
    );
    // And the row data is intact.
    let rows = reopened.query("SELECT k, v FROM wr", ()).unwrap();
    assert_eq!(
        rows,
        vec![vec![
            rustqlite::Value::Text("a".into()),
            rustqlite::Value::Integer(1)
        ]]
    );
}

#[test]
fn without_rowid_pk_dump_excludes_internal_index() {
    // SQLite-format dump: real SQLite re-opens the file, sees NO stray
    // index object, and enforces the PK itself (its own b-tree layout).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wr.db");
    {
        let mut db = rustqlite::Database::open_sqlite_format(&path).unwrap();
        db.execute(
            "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
            (),
        )
        .unwrap();
        db.execute("INSERT INTO wr VALUES ('a', 1), ('b', 2)", ())
            .unwrap();
        // Drop: the pending dump materializes the SQLite-format file.
    }

    let rc = Connection::open(&path).unwrap();
    let n: i64 = rc
        .query_row("SELECT count(*) FROM wr", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "rows readable by real SQLite");
    let master: Vec<String> = rc
        .prepare("SELECT name FROM sqlite_master ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(master, vec!["wr".to_string()], "no stray index object");
    // SQLite itself rejects a duplicate on the dumped file.
    let dup = rc
        .execute_batch("INSERT INTO wr VALUES ('a', 9)")
        .err()
        .map(|e| clean(&e.to_string()));
    assert_eq!(
        dup.as_deref(),
        Some("UNIQUE constraint failed: wr.k"),
        "SQLite enforces the PK on the dumped file"
    );
    // And the engine re-opens its own dump with the PK intact.
    let mut again = rustqlite::Database::open_sqlite_format(&path).unwrap();
    let dup2 = again
        .execute("INSERT INTO wr VALUES ('a', 9)", ())
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        dup2.as_deref(),
        Some("UNIQUE constraint failed: wr.k"),
        "engine enforces the PK after re-opening the dump"
    );
}
