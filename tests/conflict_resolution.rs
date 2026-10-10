//! Conflict resolution follows sqlite3GenerateConstraintChecks.
//!
//! Every constraint resolves by the statement's OR clause when it has one,
//! else by its OWN `ON CONFLICT` clause (NOT NULL / PRIMARY KEY / UNIQUE,
//! column- or table-level), else ABORT:
//! * IGNORE skips the row (INSERT) or leaves it unchanged (UPDATE) — for
//!   NOT NULL and CHECK too;
//! * REPLACE gives a NOT NULL column its DEFAULT (ABORT without one),
//!   aborts a CHECK, and deletes the holder of a conflicting key;
//! * FAIL keeps the changes the statement made before the violation;
//! * ROLLBACK ends the whole transaction.
//!
//! Constraints are checked in SQLite's order — NOT NULL, CHECK, the rowid,
//! then the UNIQUE indexes newest first with REPLACE ones last — so a row
//! breaking several reports the same one. Each statement's outcome (the
//! exact error message), `changes()`, the autocommit state and every
//! table's rows must equal the bundled SQLite's.

use rusqlite::types::Value as Sv;
use rusqlite::Connection;
use rustqlite::{Database, Value};

fn ours_rows(db: &mut Database, sql: &str) -> Vec<String> {
    db.query(sql, [])
        .unwrap_or_else(|e| panic!("ours {sql}: {e}"))
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("I{i}"),
                    Value::Real(f) => format!("R{f:?}"),
                    Value::Text(t) => format!("T{}", t.as_str()),
                    Value::Blob(b) => format!("B{b:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn sqlite_rows(c: &Connection, sql: &str) -> Vec<String> {
    let mut st = c.prepare(sql).unwrap();
    let n = st.column_count();
    let mut rows = st.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push(
            (0..n)
                .map(|i| match r.get::<_, Sv>(i).unwrap() {
                    Sv::Null => "NULL".to_string(),
                    Sv::Integer(i) => format!("I{i}"),
                    Sv::Real(f) => format!("R{f:?}"),
                    Sv::Text(t) => format!("T{t}"),
                    Sv::Blob(b) => format!("B{b:?}"),
                })
                .collect::<Vec<_>>()
                .join("|"),
        );
    }
    out
}

fn sqlite_error_message(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqliteFailure(_, Some(m)) => m.clone(),
        other => other.to_string(),
    }
}

/// Run every statement in both engines and compare outcome, changes(),
/// autocommit state and the listed tables after each one.
fn run(tables: &[&str], script: &[&str]) {
    let mut db = Database::open_in_memory().unwrap();
    let c = Connection::open_in_memory().unwrap();
    for sql in script {
        let ours = db.execute(sql, []);
        let theirs = c.execute_batch(sql);
        match (&ours, &theirs) {
            (Ok(()), Ok(())) => {}
            // Constraint messages are SQLite-exact (apps match on them).
            (Err(a @ rustqlite::Error::Constraint(_)), Err(b)) => assert_eq!(
                a.to_string(),
                sqlite_error_message(b),
                "error text differs: {sql}"
            ),
            (Err(_), Err(_)) => {}
            _ => panic!("{sql}: ours {ours:?} vs sqlite {theirs:?}"),
        }
        assert_eq!(
            db.is_autocommit(),
            c.is_autocommit(),
            "autocommit state after {sql}"
        );
        assert_eq!(
            ours_rows(&mut db, "SELECT changes()"),
            sqlite_rows(&c, "SELECT changes()"),
            "changes() after {sql}"
        );
        for t in tables {
            let exists = format!("SELECT count(*) FROM sqlite_master WHERE name = '{t}'");
            let present = sqlite_rows(&c, &exists);
            assert_eq!(
                ours_rows(&mut db, &exists),
                present,
                "{t} existence after {sql}"
            );
            if present == ["I0"] {
                continue;
            }
            // A multiset (WITHOUT ROWID tables have no rowid to order by).
            let q = format!("SELECT * FROM {t}");
            let mut a = ours_rows(&mut db, &q);
            let mut b = sqlite_rows(&c, &q);
            a.sort();
            b.sort();
            assert_eq!(a, b, "{t} after {sql}");
        }
    }
}

#[test]
fn statement_or_clauses_on_not_null_and_check() {
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL DEFAULT 'da', \
                  b INTEGER NOT NULL, c INTEGER CHECK (c > 0), u TEXT UNIQUE, \
                  n INTEGER NOT NULL DEFAULT NULL)";
    for or in [
        "",
        "OR ABORT",
        "OR FAIL",
        "OR IGNORE",
        "OR REPLACE",
        "OR ROLLBACK",
    ] {
        let ins = |vals: &str| format!("INSERT {or} INTO t VALUES {vals}");
        let upd = |set: &str, w: &str| format!("UPDATE {or} t SET {set} WHERE {w}");
        let script = vec![
            schema.to_string(),
            "INSERT INTO t VALUES (1, 'x', 1, 1, 'u1', 0), (2, 'y', 2, 2, 'u2', 0)".to_string(),
            "BEGIN".to_string(),
            ins("(3, NULL, 3, 3, 'u3', 0), (4, 'z', 4, 4, 'u4', 0)"),
            ins("(5, 'q', 5, 5, 'u5', 0), (6, 'w', NULL, 6, 'u6', 0), (7, 'v', 7, 7, 'u7', 0)"),
            ins("(8, 'p', 8, -8, 'u8', 0), (9, 'o', 9, 9, 'u9', 0)"),
            ins("(10, 'm', 10, 10, 'u10', NULL)"),
            ins("(11, 'k', 11, 11, 'u1', 0), (12, 'j', 12, 12, 'u12', 0)"),
            ins("(1, 'r', 13, 13, 'u13', 0)"),
            upd("a = NULL", "id IN (1, 2)"),
            upd("c = -1", "id = 2"),
            upd("b = NULL", "id >= 2"),
            upd("u = 'u2'", "id = 1"),
            upd("id = 2", "id = 1"),
            "COMMIT".to_string(),
        ];
        let refs: Vec<&str> = script.iter().map(|s| s.as_str()).collect();
        run(&["t"], &refs);
    }
}

#[test]
fn column_and_table_conflict_clauses() {
    run(
        &["a", "b", "c", "d", "e", "f", "g"],
        &[
            "CREATE TABLE a (x INTEGER PRIMARY KEY, y TEXT NOT NULL ON CONFLICT REPLACE DEFAULT 'd', \
             z TEXT UNIQUE ON CONFLICT REPLACE)",
            "INSERT INTO a VALUES (1, NULL, 'u')",
            "INSERT INTO a VALUES (2, 'b', 'u')",
            "INSERT OR ABORT INTO a VALUES (3, 'c', 'u')",
            "INSERT OR IGNORE INTO a VALUES (3, NULL, 'v')",
            "UPDATE a SET y = NULL",
            "CREATE TABLE b (x INTEGER PRIMARY KEY, y TEXT NOT NULL ON CONFLICT IGNORE)",
            "INSERT INTO b VALUES (1, NULL), (2, 'k')",
            "UPDATE b SET y = NULL WHERE x = 2",
            "CREATE TABLE c (x INTEGER PRIMARY KEY ON CONFLICT REPLACE, y TEXT)",
            "INSERT INTO c VALUES (1, 'a')",
            "INSERT INTO c VALUES (1, 'b')",
            "UPDATE c SET x = 5",
            "INSERT INTO c VALUES (6, 'c')",
            "UPDATE c SET x = 5 WHERE x = 6",
            "CREATE TABLE d (p, q, r, PRIMARY KEY (p, q) ON CONFLICT IGNORE, UNIQUE (r) ON CONFLICT FAIL)",
            "INSERT INTO d VALUES (1, 1, 'a'), (1, 1, 'b'), (2, 2, 'c')",
            "INSERT INTO d VALUES (3, 3, 'd'), (4, 4, 'a'), (5, 5, 'e')",
            "UPDATE d SET r = 'z' WHERE p >= 2",
            "CREATE TABLE e (x INTEGER PRIMARY KEY, u UNIQUE ON CONFLICT ROLLBACK, w NOT NULL ON CONFLICT FAIL)",
            "INSERT INTO e VALUES (1, 'a', 1)",
            "BEGIN",
            "INSERT INTO e VALUES (2, 'b', 2)",
            "INSERT INTO e VALUES (3, 'a', 3)",
            "INSERT INTO e VALUES (4, 'c', 4), (5, 'd', NULL), (6, 'e', 6)",
            "CREATE TABLE f (x INTEGER PRIMARY KEY, u UNIQUE ON CONFLICT IGNORE, v UNIQUE)",
            "INSERT INTO f VALUES (1, 'a', 'p'), (2, 'b', 'q')",
            "INSERT INTO f VALUES (3, 'a', 'q')",
            "INSERT INTO f VALUES (4, 'c', 'p')",
            "INSERT OR REPLACE INTO f VALUES (5, 'a', 'q')",
            "CREATE TABLE g (k TEXT PRIMARY KEY ON CONFLICT REPLACE, v) WITHOUT ROWID",
            "INSERT INTO g VALUES ('a', 1), ('a', 2), ('b', 3)",
            "UPDATE g SET k = 'b' WHERE k = 'a'",
        ],
    );
}

#[test]
fn several_conflicts_report_sqlites_constraint() {
    run(
        &["t", "w"],
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a UNIQUE, b UNIQUE, c, d NOT NULL, e CHECK (e > 0))",
            "CREATE UNIQUE INDEX tc ON t(c)",
            "INSERT INTO t VALUES (1, 1, 1, 1, 1, 1)",
            "INSERT INTO t VALUES (1, 1, 1, 1, 1, 1)",
            "INSERT INTO t VALUES (2, 1, 1, 1, 1, 1)",
            "INSERT INTO t VALUES (2, 2, 1, 1, 1, 1)",
            "INSERT INTO t VALUES (2, 1, 2, 1, 1, 1)",
            "INSERT INTO t VALUES (1, 1, 1, 1, NULL, -1)",
            "INSERT INTO t VALUES (1, 1, 1, 1, 1, -1)",
            "INSERT INTO t VALUES (2, 2, 2, 2, 2, 2)",
            "UPDATE t SET a = 1, b = 1, c = 1 WHERE id = 2",
            "UPDATE t SET id = 1 WHERE id = 2",
            "CREATE TABLE w (p, q, PRIMARY KEY (p), UNIQUE (q)) WITHOUT ROWID",
            "INSERT INTO w VALUES (1, 1)",
            "INSERT INTO w VALUES (1, 1)",
            "INSERT INTO w VALUES (2, 1)",
        ],
    );
}

#[test]
fn fail_keeps_earlier_rows_and_rollback_ends_the_transaction() {
    run(
        &["t"],
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL, u TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'x', 'u1')",
            "INSERT OR FAIL INTO t VALUES (2, 'y', 'u2'), (3, NULL, 'u3'), (4, 'z', 'u4')",
            "INSERT OR FAIL INTO t VALUES (5, 'y', 'u5'), (6, 'q', 'u1'), (7, 'z', 'u7')",
            "UPDATE OR FAIL t SET u = 'k' WHERE id > 1",
            "UPDATE OR FAIL t SET a = NULL WHERE id >= 5",
            "BEGIN",
            "INSERT INTO t VALUES (8, 'b', 'u8')",
            "INSERT OR FAIL INTO t VALUES (9, 'c', 'u9'), (10, NULL, 'u10')",
            "INSERT OR ROLLBACK INTO t VALUES (11, 'y', 'u11'), (12, NULL, 'u12')",
            "COMMIT",
            "BEGIN",
            "UPDATE OR ROLLBACK t SET u = 'u1' WHERE id = 2",
            "ROLLBACK",
            "INSERT OR ROLLBACK INTO t VALUES (13, NULL, 'u13')",
        ],
    );
}

#[test]
fn fail_commits_its_rows_on_a_file_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fail.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT NOT NULL)",
            [],
        )
        .unwrap();
        assert!(db
            .execute(
                "INSERT OR FAIL INTO t VALUES (1, 'x'), (2, 'y'), (3, NULL), (4, 'z')",
                []
            )
            .is_err());
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        ours_rows(&mut db, "SELECT id FROM t ORDER BY id"),
        vec!["I1".to_string(), "I2".to_string()]
    );
    assert_eq!(
        ours_rows(&mut db, "PRAGMA integrity_check"),
        vec!["Tok".to_string()]
    );
}

#[test]
fn triggers_inherit_and_last_insert_rowid_reverts() {
    run(
        &["t", "log"],
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v UNIQUE)",
            "CREATE TABLE log (id INTEGER PRIMARY KEY, e UNIQUE)",
            "CREATE TRIGGER ta AFTER INSERT ON t BEGIN INSERT INTO log(e) VALUES (new.v); \
             INSERT INTO log(e) VALUES ('lir ' || last_insert_rowid()); END",
            "INSERT INTO t VALUES (100, 'x')",
            "SELECT last_insert_rowid()",
            "INSERT OR IGNORE INTO t VALUES (101, 'x')",
            "INSERT OR IGNORE INTO t VALUES (102, 'y')",
            "INSERT OR ABORT INTO t VALUES (103, 'y')",
            "INSERT OR REPLACE INTO t VALUES (104, 'x')",
            "INSERT INTO t(v) VALUES ('p'), ('q')",
            "CREATE TABLE m (a INTEGER PRIMARY KEY, b)",
            "INSERT INTO m(b) VALUES (last_insert_rowid()), (last_insert_rowid())",
            "INSERT INTO t VALUES (200, 'z') ON CONFLICT(v) DO UPDATE SET id = 201",
            "INSERT INTO t VALUES (300, 'z') ON CONFLICT(v) DO UPDATE SET id = 202",
        ],
    );
}

#[test]
fn check_failures_name_the_constraint_like_sqlite() {
    run(
        &["k"],
        &[
            "CREATE TABLE k (a INTEGER CHECK (a > 0), b CHECK(  b  <> 'x' ), \
             c CONSTRAINT c_pos CHECK (c >= 0), d CONSTRAINT dn NOT NULL CHECK (length(d) < 5), \
             e, CHECK ((e) IS NULL OR (e % 2) = 0), CONSTRAINT \"odd name\" CHECK (a <> 7), \
             CONSTRAINT [br] CHECK (a <> 8))",
            "INSERT INTO k VALUES (1, 'y', 0, 'ok', NULL)",
            "INSERT INTO k VALUES (0, 'y', 0, 'ok', NULL)",
            "INSERT INTO k VALUES (1, 'x', 0, 'ok', NULL)",
            "INSERT INTO k VALUES (1, 'y', -1, 'ok', NULL)",
            "INSERT INTO k VALUES (1, 'y', 0, 'toolong', NULL)",
            "INSERT INTO k VALUES (1, 'y', 0, 'ok', 3)",
            "INSERT INTO k VALUES (7, 'y', 0, 'ok', NULL)",
            "INSERT INTO k VALUES (8, 'y', 0, 'ok', NULL)",
            "UPDATE k SET a = -5",
            "INSERT OR IGNORE INTO k VALUES (-1, 'y', 0, 'ok', NULL), (2, 'z', 1, 'ok', 4)",
        ],
    );
}

/// A WITHOUT ROWID table's PRIMARY KEY index is numbered at its textual
/// position among the table's constraints (`sqlite_autoindex_w_1` for
/// `PRIMARY KEY (p)` before `UNIQUE (q)`, which is `_2`), and the indexes
/// are checked in SQLite's order. A file written before that change
/// (fixtures/legacy_without_rowid_autoindex.db: the UNIQUE indexes
/// numbered as if the PK did not exist, the PK after them) keeps its
/// layout on open — every UNIQUE stays enforced, no tree is orphaned.
#[test]
fn without_rowid_autoindex_numbering_and_legacy_files() {
    run(
        &["w", "w2"],
        &[
            "CREATE TABLE w (p, q, PRIMARY KEY (p), UNIQUE (q)) WITHOUT ROWID",
            "CREATE TABLE w2 (a UNIQUE, b PRIMARY KEY, c UNIQUE) WITHOUT ROWID",
            "INSERT INTO w VALUES (1, 'a'), (2, 'b')",
            "INSERT INTO w VALUES (1, 'a')",
            "INSERT INTO w2 VALUES (1, 'x', 10), (2, 'y', 20)",
            "INSERT INTO w2 VALUES (1, 'x', 10)",
        ],
    );
    let mut db = Database::open_in_memory().unwrap();
    let c = Connection::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE w (p, q, PRIMARY KEY (p), UNIQUE (q)) WITHOUT ROWID",
        "CREATE TABLE w2 (a UNIQUE, b PRIMARY KEY, c UNIQUE) WITHOUT ROWID",
    ] {
        db.execute(s, []).unwrap();
        c.execute_batch(s).unwrap();
    }
    for q in [
        "PRAGMA index_list(w)",
        "PRAGMA index_list(w2)",
        "SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY 1",
    ] {
        assert_eq!(ours_rows(&mut db, q), sqlite_rows(&c, q), "{q}");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    std::fs::copy("tests/fixtures/legacy_without_rowid_autoindex.db", &path).unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(ours_rows(&mut db, "PRAGMA integrity_check"), vec!["Tok"]);
    for (sql, err) in [
        (
            "INSERT INTO w VALUES (3, 'a')",
            "UNIQUE constraint failed: w.q",
        ),
        (
            "INSERT INTO w VALUES (1, 'z')",
            "UNIQUE constraint failed: w.p",
        ),
        (
            "INSERT INTO w2 VALUES (3, 'z', 10)",
            "UNIQUE constraint failed: w2.c",
        ),
        (
            "INSERT INTO w2 VALUES (1, 'q', 30)",
            "UNIQUE constraint failed: w2.a",
        ),
        (
            "INSERT INTO w2 VALUES (3, 'x', 30)",
            "UNIQUE constraint failed: w2.b",
        ),
    ] {
        assert_eq!(db.execute(sql, []).unwrap_err().to_string(), err, "{sql}");
    }
    db.execute("INSERT INTO w VALUES (3, 'c')", []).unwrap();
    assert_eq!(ours_rows(&mut db, "PRAGMA integrity_check"), vec!["Tok"]);
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(ours_rows(&mut db, "PRAGMA integrity_check"), vec!["Tok"]);
    assert_eq!(ours_rows(&mut db, "SELECT count(*) FROM w"), vec!["I3"]);
}

/// Prepared (stepped) statements keep the same statement atomicity as
/// `Database::execute`: a failing multi-row INSERT leaves none of its rows
/// — nor its triggers' — while a FAIL conflict keeps the rows before it.
/// The stepped path used to keep every row written before the failure.
#[test]
fn prepared_statements_are_atomic_like_sqlite() {
    let mut db = Database::open_in_memory().unwrap();
    let c = Connection::open_in_memory().unwrap();
    for s in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u UNIQUE, n NOT NULL)",
        "CREATE TABLE log (e)",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.u); END",
    ] {
        db.execute(s, []).unwrap();
        c.execute_batch(s).unwrap();
    }
    let cases: &[(&str, &[i64])] = &[
        ("INSERT INTO t VALUES (?, ?, 1), (?, ?, 1)", &[1, 10, 2, 10]),
        (
            "INSERT OR FAIL INTO t VALUES (?, ?, 1), (?, ?, NULL)",
            &[3, 30, 4, 40],
        ),
        (
            "INSERT INTO t VALUES (?, ?, 1), (?, ?, 1), (?, ?, NULL)",
            &[5, 50, 6, 60, 7, 70],
        ),
        ("UPDATE t SET u = ? WHERE id >= ?", &[99, 0]),
        (
            "INSERT OR IGNORE INTO t VALUES (?, ?, NULL), (?, ?, 1)",
            &[8, 80, 9, 90],
        ),
    ];
    for (sql, vals) in cases {
        let ours = {
            let mut st = db.prepare(sql).unwrap();
            st.bind_all(&vals.iter().map(|v| Value::Integer(*v)).collect::<Vec<_>>())
                .unwrap();
            st.step().map(|_| ())
        };
        let theirs = c
            .prepare(sql)
            .unwrap()
            .execute(rusqlite::params_from_iter(vals.iter()));
        assert_eq!(
            ours.is_ok(),
            theirs.is_ok(),
            "{sql}: {ours:?} vs {theirs:?}"
        );
        for q in [
            "SELECT * FROM t ORDER BY id",
            "SELECT * FROM log ORDER BY rowid",
            "SELECT changes()",
        ] {
            assert_eq!(
                ours_rows(&mut db, q),
                sqlite_rows(&c, q),
                "after {sql}: {q}"
            );
        }
        assert_eq!(
            db.changes() as u64,
            c.changes(),
            "changes() API after {sql}"
        );
    }
    assert_eq!(ours_rows(&mut db, "PRAGMA integrity_check"), vec!["Tok"]);
}

/// changes() / total_changes() belong to the CONNECTION (they used to be
/// per thread): two connections interleaved on one thread each see their
/// own counts — through the API, through the SQL functions, and from
/// another thread.
#[test]
fn change_counters_are_per_connection() {
    let mut a = Database::open_in_memory().unwrap();
    let mut b = Database::open_in_memory().unwrap();
    for db in [&mut a, &mut b] {
        db.execute("CREATE TABLE t (x)", []).unwrap();
    }
    a.execute("INSERT INTO t VALUES (1), (2), (3)", []).unwrap();
    b.execute("INSERT INTO t VALUES (1)", []).unwrap();
    assert_eq!((a.changes(), a.total_changes()), (3, 3));
    assert_eq!((b.changes(), b.total_changes()), (1, 1));
    assert_eq!(
        ours_rows(&mut a, "SELECT changes(), total_changes()"),
        vec!["I3|I3"]
    );
    assert_eq!(
        ours_rows(&mut b, "SELECT changes(), total_changes()"),
        vec!["I1|I1"]
    );
    a.execute("UPDATE t SET x = x + 1 WHERE x > 1", []).unwrap();
    assert_eq!(
        ours_rows(&mut b, "SELECT changes(), total_changes()"),
        vec!["I1|I1"]
    );
    assert_eq!(
        ours_rows(&mut a, "SELECT changes(), total_changes()"),
        vec!["I2|I5"]
    );
    let a = std::thread::spawn(move || (a.changes(), a.total_changes(), a))
        .join()
        .unwrap();
    assert_eq!((a.0, a.1), (2, 5));
    let mut a = a.2;
    a.execute("DELETE FROM t", []).unwrap();
    assert_eq!((a.changes(), a.total_changes()), (3, 8));
    assert_eq!((b.changes(), b.total_changes()), (1, 1));
}
