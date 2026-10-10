//! SQLITE_FULL rollback SCOPE parity with real SQLite.
//!
//! sqlite3VdbeHalt treats SQLITE_FULL ("database or disk is full" — here
//! OP_NewRowid's exhausted AUTOINCREMENT counter) as a special error: it
//! rolls back only the failing STATEMENT when the statement runs with a
//! statement journal (`isMultiWrite && mayAbort`), and otherwise the
//! WHOLE TRANSACTION — earlier statements' writes vanish and the
//! connection returns to autocommit (stateful-fuzz seeds 4004/5005: a
//! later COMMIT failed with "cannot commit - no transaction is active").
//!
//! Every scenario runs on both engines from the same schema: BEGIN, write
//! a marker row, run a statement that fails with SQLITE_FULL, then compare
//! the autocommit state, the marker's survival and the full table states.
//! The scenarios sweep each input of the decision — multi-row sources,
//! triggers, NOT NULL / CHECK / UNIQUE / rowid / FK checks, conflict
//! clauses, upsert targets, function call sites (inline ones excluded).

use rusqlite::types::Value as Sv;
use rusqlite::Connection;
use rustqlite::{Database, Value};

const MAX: &str = "9223372036854775807";

fn ours_run(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    if sql.trim_start().to_ascii_uppercase().starts_with("SELECT") {
        db.query(sql, []).map_err(|e| e.to_string())
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    }
}

fn sqlite_run(rc: &Connection, sql: &str) -> Result<Vec<Vec<Sv>>, String> {
    let mut stmt = rc.prepare(sql).map_err(|e| e.to_string())?;
    let n = stmt.column_count();
    if n == 0 {
        return stmt
            .execute([])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string());
    }
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        out.push((0..n).map(|i| row.get(i).unwrap_or(Sv::Null)).collect());
    }
    Ok(out)
}

fn norm_ours(rows: Vec<Vec<Value>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => format!("I{i}"),
                    Value::Real(f) => format!("R{f:?}"),
                    Value::Text(t) => format!("T{}", t.as_str()),
                    Value::Blob(b) => format!("B{b:?}"),
                })
                .collect()
        })
        .collect()
}

fn norm_sqlite(rows: Vec<Vec<Sv>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Sv::Null => "NULL".to_string(),
                    Sv::Integer(i) => format!("I{i}"),
                    Sv::Real(f) => format!("R{f:?}"),
                    Sv::Text(t) => format!("T{t}"),
                    Sv::Blob(b) => format!("B{b:?}"),
                })
                .collect()
        })
        .collect()
}

struct Scenario {
    name: &'static str,
    /// Extra schema beyond the shared `full_t` / `marker` tables.
    setup: &'static [&'static str],
    /// Opens the transaction (BEGIN, or a bare SAVEPOINT).
    open: &'static str,
    /// The statement that fails with SQLITE_FULL.
    failing: &'static str,
    /// Tables to dump after the failure.
    dump: &'static [&'static str],
}

/// Run one scenario on both engines; returns SQLite's verdict (true =
/// the whole transaction was rolled back).
fn run(s: &Scenario) -> bool {
    let mut db = Database::open_in_memory().unwrap();
    let rc = Connection::open_in_memory().unwrap();
    let base = [
        "CREATE TABLE full_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v)".to_string(),
        format!("INSERT INTO full_t (id, v) VALUES ({MAX}, 'top')"),
        "CREATE TABLE marker (x)".to_string(),
    ];
    for sql in base.iter().map(String::as_str).chain(s.setup.iter().copied()) {
        ours_run(&mut db, sql).unwrap_or_else(|e| panic!("[{}] ours setup {sql}: {e}", s.name));
        sqlite_run(&rc, sql).unwrap_or_else(|e| panic!("[{}] sqlite setup {sql}: {e}", s.name));
    }
    for sql in [s.open, "INSERT INTO marker VALUES (1)"] {
        ours_run(&mut db, sql).unwrap_or_else(|e| panic!("[{}] ours {sql}: {e}", s.name));
        sqlite_run(&rc, sql).unwrap_or_else(|e| panic!("[{}] sqlite {sql}: {e}", s.name));
    }
    let theirs = sqlite_run(&rc, s.failing);
    let ours = ours_run(&mut db, s.failing);
    let theirs_err = theirs.expect_err(&format!("[{}] SQLite must fail with SQLITE_FULL", s.name));
    assert!(
        theirs_err.contains("database or disk is full"),
        "[{}] scenario must exercise SQLITE_FULL, SQLite said: {theirs_err}",
        s.name
    );
    let ours_err = ours.expect_err(&format!("[{}] ours must fail like SQLite", s.name));
    assert_eq!(ours_err, "database or disk is full", "[{}] error text", s.name);
    let sqlite_rolled_back = rc.is_autocommit();
    assert_eq!(
        db.is_autocommit(),
        sqlite_rolled_back,
        "[{}] rollback scope: SQLite {} the transaction",
        s.name,
        if sqlite_rolled_back { "rolled back" } else { "kept" }
    );
    for t in ["marker", "full_t", "sqlite_sequence"]
        .iter()
        .chain(s.dump.iter())
    {
        let q = format!("SELECT * FROM {t} ORDER BY rowid");
        let a = norm_ours(ours_run(&mut db, &q).unwrap());
        let b = norm_sqlite(sqlite_run(&rc, &q).unwrap());
        assert_eq!(a, b, "[{}] state of {t} after the failure", s.name);
    }
    // The transaction state must also agree for what comes next.
    let a = ours_run(&mut db, "COMMIT").is_ok();
    let b = sqlite_run(&rc, "COMMIT").is_ok();
    assert_eq!(a, b, "[{}] COMMIT outcome after the failure", s.name);
    sqlite_rolled_back
}

#[test]
fn sqlite_full_rollback_scope_matches_sqlite() {
    let scenarios = [
        Scenario {
            name: "single-row, rowid not supplied",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (v) VALUES ('a')",
            dump: &[],
        },
        Scenario {
            name: "multi-row, rowid not supplied (no abortable check)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (v) VALUES ('a'), ('b')",
            dump: &[],
        },
        Scenario {
            name: "multi-row, rowid alias supplied (rowid conflict check)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (id, v) VALUES (NULL, 'a'), (NULL, 'b')",
            dump: &[],
        },
        Scenario {
            name: "multi-row, no column list (alias implicitly supplied)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t VALUES (NULL, 'a'), (NULL, 'b')",
            dump: &[],
        },
        Scenario {
            name: "single-row, rowid alias supplied (single write)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (id, v) VALUES (NULL, 'a')",
            dump: &[],
        },
        Scenario {
            name: "multi-row into a NOT NULL table",
            setup: &[
                "CREATE TABLE nn_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL)",
                "INSERT INTO nn_t VALUES (9223372036854775807, 'top')",
            ],
            open: "BEGIN",
            failing: "INSERT INTO nn_t (v) VALUES ('a'), ('b')",
            dump: &["nn_t"],
        },
        Scenario {
            name: "multi-row into a UNIQUE-indexed table",
            setup: &[
                "CREATE TABLE uq_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v UNIQUE)",
                "INSERT INTO uq_t VALUES (9223372036854775807, 'top')",
            ],
            open: "BEGIN",
            failing: "INSERT INTO uq_t (v) VALUES ('a'), ('b')",
            dump: &["uq_t"],
        },
        Scenario {
            name: "multi-row OR IGNORE into a UNIQUE-indexed table",
            setup: &[
                "CREATE TABLE uq_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v UNIQUE)",
                "INSERT INTO uq_t VALUES (9223372036854775807, 'top')",
            ],
            open: "BEGIN",
            failing: "INSERT OR IGNORE INTO uq_t (v) VALUES ('a'), ('b')",
            dump: &["uq_t"],
        },
        Scenario {
            name: "multi-row into a CHECK table",
            setup: &[
                "CREATE TABLE ck_t (id INTEGER PRIMARY KEY AUTOINCREMENT, v CHECK (v <> 'zz'))",
                "INSERT INTO ck_t VALUES (9223372036854775807, 'top')",
            ],
            open: "BEGIN",
            failing: "INSERT INTO ck_t (v) VALUES ('a'), ('b')",
            dump: &["ck_t"],
        },
        Scenario {
            name: "INSERT ... SELECT (no abortable check)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (v) SELECT x FROM marker",
            dump: &[],
        },
        Scenario {
            name: "multi-row with a function call site",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (v) VALUES (abs(-1)), (2)",
            dump: &[],
        },
        Scenario {
            name: "multi-row with an inline function (coalesce)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (v) VALUES (coalesce(NULL, 1)), (2)",
            dump: &[],
        },
        Scenario {
            name: "multi-row with LIKE (a function call in SQLite)",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (v) VALUES ('a' LIKE 'a'), (2)",
            dump: &[],
        },
        Scenario {
            name: "multi-row OR REPLACE with the rowid supplied",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT OR REPLACE INTO full_t (id, v) VALUES (NULL, 'a'), (NULL, 'b')",
            dump: &[],
        },
        Scenario {
            name: "multi-row upsert DO NOTHING on the rowid alias",
            setup: &[],
            open: "BEGIN",
            failing: "INSERT INTO full_t (id, v) VALUES (NULL, 'a'), (NULL, 'b') \
                      ON CONFLICT(id) DO NOTHING",
            dump: &[],
        },
        Scenario {
            name: "INSERT firing a trigger whose body fails",
            setup: &[
                "CREATE TABLE src (x)",
                "CREATE TRIGGER tr AFTER INSERT ON src BEGIN \
                 INSERT INTO full_t (v) VALUES (new.x); END",
            ],
            open: "BEGIN",
            failing: "INSERT INTO src VALUES (7)",
            dump: &["src"],
        },
        Scenario {
            name: "trigger body with RAISE(ABORT)",
            setup: &[
                "CREATE TABLE src (x)",
                "CREATE TRIGGER tr AFTER INSERT ON src BEGIN \
                 SELECT RAISE(ABORT, 'negative') WHERE new.x < 0; \
                 INSERT INTO full_t (v) VALUES (new.x); END",
            ],
            open: "BEGIN",
            failing: "INSERT INTO src VALUES (7)",
            dump: &["src"],
        },
        Scenario {
            name: "UPDATE by rowid firing an inserting trigger",
            setup: &[
                "CREATE TABLE u_t (id INTEGER PRIMARY KEY, x)",
                "INSERT INTO u_t VALUES (1, 1)",
                "CREATE TRIGGER tr AFTER UPDATE ON u_t BEGIN \
                 INSERT INTO full_t (v) VALUES (new.x); END",
            ],
            open: "BEGIN",
            failing: "UPDATE u_t SET x = 2 WHERE id = 1",
            dump: &["u_t"],
        },
        Scenario {
            name: "UPDATE of a NOT NULL column firing an inserting trigger",
            setup: &[
                "CREATE TABLE u_t (id INTEGER PRIMARY KEY, x NOT NULL)",
                "INSERT INTO u_t VALUES (1, 1)",
                "CREATE TRIGGER tr AFTER UPDATE ON u_t BEGIN \
                 INSERT INTO full_t (v) VALUES (new.x); END",
            ],
            open: "BEGIN",
            failing: "UPDATE u_t SET x = 2 WHERE id = 1",
            dump: &["u_t"],
        },
        Scenario {
            name: "DELETE firing an inserting trigger",
            setup: &[
                "CREATE TABLE d_t (id INTEGER PRIMARY KEY, x)",
                "INSERT INTO d_t VALUES (1, 1), (2, 2)",
                "CREATE TRIGGER tr AFTER DELETE ON d_t BEGIN \
                 INSERT INTO full_t (v) VALUES (old.x); END",
            ],
            open: "BEGIN",
            failing: "DELETE FROM d_t WHERE id = 1",
            dump: &["d_t"],
        },
        Scenario {
            name: "multi-row into an FK child (foreign_keys=ON)",
            setup: &[
                "PRAGMA foreign_keys = ON",
                "CREATE TABLE p (id INTEGER PRIMARY KEY)",
                "CREATE TABLE c (id INTEGER PRIMARY KEY AUTOINCREMENT, pid REFERENCES p(id))",
                "INSERT INTO c VALUES (9223372036854775807, NULL)",
            ],
            open: "BEGIN",
            failing: "INSERT INTO c (pid) VALUES (NULL), (NULL)",
            dump: &["c"],
        },
        Scenario {
            name: "transaction opened by a bare SAVEPOINT",
            setup: &[],
            open: "SAVEPOINT s1",
            failing: "INSERT INTO full_t (v) VALUES ('a'), ('b')",
            dump: &[],
        },
    ];
    let mut whole = 0;
    let mut stmt_only = 0;
    for s in &scenarios {
        if run(s) {
            whole += 1;
        } else {
            stmt_only += 1;
        }
    }
    // The sweep must exercise BOTH scopes, or it pins nothing.
    assert!(
        whole >= 5 && stmt_only >= 5,
        "scenario mix: {whole} whole-transaction / {stmt_only} statement-only rollbacks"
    );
}
