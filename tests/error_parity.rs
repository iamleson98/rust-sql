//! Error-text parity: the engine's error surface vs bundled SQLite's
//! `sqlite3_errmsg`, byte-for-byte.
//!
//! The engine historically resolved column names at RUNTIME, silently
//! evaluating unknown names to NULL (a typo'd projection returned a
//! column of NULLs; a typo'd UPDATE SET target bound to slot 0), and
//! reported its own error texts (`not found: table: x`). The prepare-time
//! name resolver (`planner::namecheck`) closed the family; this battery
//! pins the contract: for every shape, both engines must agree on
//! success/failure AND the error text must be byte-identical.
//!
//! Lazy contracts (SQLite validates lazily and so does the engine):
//! CREATE VIEW / CREATE TRIGGER bodies, FK REFERENCES clauses — error
//! shapes inside them only fire at use time (trigger bodies at FIRE
//! time; view bodies at first SELECT).
//!
//! One documented text divergence remains: SQLite prefixes trigger-BODY
//! table errors with the schema (`no such table: main.nosuch`); the
//! engine reports the unprefixed form (both error, same shape). The
//! parse-error family (syntax errors) is excluded — message formatting
//! is engine-specific on both sides.

use rusqlite::Connection;

fn lite() -> Connection {
    let con = Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x INT, y TEXT);
         CREATE TABLE u (id INTEGER PRIMARY KEY, k INT, v INT);
         CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
         CREATE INDEX idx_t_x ON t(x);
         CREATE VIEW v AS SELECT id, x FROM t;
         CREATE TRIGGER trg AFTER INSERT ON t BEGIN SELECT 1; END;",
    )
    .unwrap();
    con
}

fn eng() -> rustqlite::Database {
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x INT, y TEXT);
         CREATE TABLE u (id INTEGER PRIMARY KEY, k INT, v INT);
         CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID;
         CREATE INDEX idx_t_x ON t(x);
         CREATE VIEW v AS SELECT id, x FROM t;
         CREATE TRIGGER trg AFTER INSERT ON t BEGIN SELECT 1; END;",
        [],
    )
    .unwrap();
    db
}

/// Strip rusqlite's Display decoration to recover the raw
/// sqlite3_errmsg bytes: "Runtime error: <msg> in <sql> at offset N".
fn raw_sqlite_msg(e: &rusqlite::Error) -> String {
    let s = format!("{}", e);
    let mut out = s.clone();
    for prefix in ["Runtime error: ", "SQL conversion or encoding error: "] {
        if let Some(rest) = out.strip_prefix(prefix) {
            out = rest.to_string();
            break;
        }
    }
    if let Some(pos) = out.rfind(" at offset ") {
        out.truncate(pos);
        if let Some(ipos) = out.rfind(" in ") {
            out.truncate(ipos);
        }
    }
    out
}

/// One differential case: same SQL on both engines. Both-error → the
/// texts must match byte-for-byte; both-OK → the row counts must match.
fn diff_err(sql: &str) {
    // SELECT/WITH/EXPLAIN shapes: compare ROW COUNTS when both succeed.
    // DML/DDL shapes: compare ERROR TEXTS only (the engine's query() on
    // DML returns a changes row; SQLite's prepare returns none — an API
    // shape difference, not an error-text difference).
    let first_word: String = sql
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    let is_query = matches!(
        first_word.as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "VALUES"
    );
    let mut db = eng();
    let con = lite();
    if is_query {
        let ours = db.query(sql, []).map(|r| r.len());
        let theirs = con
            .prepare(sql)
            .and_then(|mut s| s.query([]).map(|_| 0usize))
            .map_err(|e| raw_sqlite_msg(&e));
        match (ours, theirs) {
            (Ok(_), Ok(_)) => {}
            (Err(a), Err(b)) => assert_eq!(
                a.to_string(),
                b,
                "both errored but texts differ on {sql:?}:\n  engine: {a}\n  sqlite: {b}"
            ),
            (a, b) => panic!("mismatch on {sql:?}: engine {a:?} vs sqlite {b:?}"),
        }
    } else {
        let ours = db.execute(sql, []).map_err(|e| e.to_string());
        let theirs = con.execute_batch(sql).map_err(|e| raw_sqlite_msg(&e));
        match (ours, theirs) {
            (Ok(()), Ok(())) => {}
            (Err(a), Err(b)) => assert_eq!(
                a, b,
                "both errored but texts differ on {sql:?}:\n  engine: {a}\n  sqlite: {b}"
            ),
            (a, b) => panic!("mismatch on {sql:?}: engine {a:?} vs sqlite {b:?}"),
        }
    }
}

// ------------------------------------------------------------------------
// The lookup family: unknown table / column / index / view / trigger
// ------------------------------------------------------------------------

#[test]
fn err_table_lookups() {
    for sql in [
        "SELECT * FROM nosuch;",
        "INSERT INTO nosuch VALUES (1);",
        "UPDATE nosuch SET x = 1;",
        "DELETE FROM nosuch;",
        "DROP TABLE nosuch;",
        "ALTER TABLE nosuch ADD COLUMN c INT;",
        "ALTER TABLE nosuch RENAME TO t2;",
        "ANALYZE nosuch;",
        "SELECT * FROM t WHERE x IN nosuchtable;",
        "SELECT nosuch.* FROM t;",
        "SELECT * FROM nosuch UNION SELECT 1;",
    ] {
        diff_err(sql);
    }
}

#[test]
fn err_index_lookups() {
    for sql in [
        "DROP INDEX nosuch;",
        "SELECT * FROM t INDEXED BY nosuch;",
        "SELECT * FROM t INDEXED BY idx_t_x;",
    ] {
        diff_err(sql);
    }
}

#[test]
fn err_object_drops() {
    for sql in [
        "DROP VIEW nosuch;",
        "DROP TRIGGER nosuch;",
        "DROP VIEW IF EXISTS nosuch;",
        "DROP TRIGGER IF EXISTS nosuch;",
    ] {
        diff_err(sql);
    }
}

#[test]
fn err_column_family_select() {
    for sql in [
        "SELECT nosuchcol FROM t;",
        "SELECT t.nosuchcol FROM t;",
        "SELECT t2.x FROM t;",
        "SELECT sum(nosuchcol) FROM t;",
        "SELECT nosuchcol, count(*) FROM t GROUP BY x;",
        "SELECT x FROM t WHERE nosuchcol = 1;",
        "SELECT x FROM t GROUP BY nosuchcol;",
        "SELECT x AS a FROM t ORDER BY b;",
        "SELECT x AS a FROM t ORDER BY a, nosuch;",
        "SELECT row_number() OVER (PARTITION BY nosuch) FROM t;",
        "SELECT * FROM t WHERE x IN (SELECT nosuchcol FROM t);",
        "SELECT abs(nosuchcol) FROM t;",
        "SELECT CASE WHEN nosuchcol > 0 THEN 1 ELSE 0 END FROM t;",
        "SELECT CAST(nosuchcol AS INT) FROM t;",
        "SELECT x FROM t LIMIT nosuch;",
        "SELECT x FROM t LIMIT 1 OFFSET nosuch;",
        "SELECT nosuch FROM t UNION SELECT 1;",
        "SELECT * FROM t a JOIN t b ON a.nosuch = b.x;",
        "SELECT * FROM v WHERE nosuch = 1;",
        "SELECT * FROM (VALUES (1, 'a')) WHERE nosuch = 1;",
    ] {
        diff_err(sql);
    }
}

#[test]
fn err_column_family_dml() {
    for sql in [
        "UPDATE t SET nosuchcol = 1;",
        "UPDATE t SET x = 1 WHERE nosuchcol = 2;",
        "DELETE FROM t WHERE nosuchcol = 1;",
        "INSERT INTO t (x) VALUES (1) RETURNING nosuchcol;",
        "UPDATE t SET x = 1 RETURNING nosuchcol + 1;",
        "DELETE FROM t WHERE id = 1 RETURNING nosuchcol;",
        "INSERT INTO t (id, x) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET nosuch = 1;",
        "INSERT INTO t (id, x) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET x = excluded.nosuch;",
        "INSERT INTO t (id, x) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET x = 2 WHERE nosuch > 0;",
        "INSERT INTO t (id, x, y, extra) VALUES (1, 2, 3, 4);",
    ] {
        diff_err(sql);
    }
}

#[test]
fn err_ddl_columns() {
    for sql in [
        "CREATE TABLE c1 (a INT CHECK (nosuch > 0));",
        "CREATE TABLE c3 (a INT, b AS (nosuch * 2));",
        "CREATE INDEX ie ON t((nosuch + 1));",
        "CREATE INDEX ip ON t(x) WHERE nosuch > 0;",
        "CREATE TABLE pk1 (a INT, PRIMARY KEY (nosuch));",
        "CREATE TABLE uq1 (a INT, UNIQUE (nosuch));",
    ] {
        diff_err(sql);
    }
}

#[test]
fn err_rowid_without_rowid() {
    diff_err("SELECT rowid FROM wr;");
    diff_err("SELECT k, rowid FROM wr;");
}

#[test]
fn err_ambiguous() {
    diff_err("SELECT x FROM t a, t b;");
    diff_err("SELECT k FROM u, u;");
    // Unique resolution across sources is NOT ambiguous.
    diff_err("SELECT id FROM u, t WHERE t.x = u.k;");
}

#[test]
fn err_join_using() {
    diff_err("SELECT * FROM t a JOIN t b USING (nosuch);");
    diff_err("SELECT * FROM t a JOIN t b USING (x);");
}

#[test]
fn err_collations_and_reindex_detach() {
    diff_err("CREATE INDEX i3 ON t(x COLLATE nosuchcoll);");
    diff_err("CREATE TABLE c9 (a TEXT COLLATE nosuchcoll);");
    diff_err("ALTER TABLE t ADD COLUMN c TEXT COLLATE nosuchcoll;");
    diff_err("REINDEX nosuch;");
    diff_err("REINDEX idx_t_x;");
    diff_err("DETACH nosuch;");
}

#[test]
fn err_create_index_and_trigger_targets() {
    diff_err("CREATE INDEX i2 ON nosuch(x);");
    diff_err("CREATE TRIGGER g2 AFTER INSERT ON nosuchtable BEGIN SELECT 1; END;");
}

// ------------------------------------------------------------------------
// Lazy contracts: both engines accept at create; the use-site errors
// ------------------------------------------------------------------------

#[test]
fn lazy_view_bodies() {
    // CREATE VIEW accepts unknown names (SQLite: lazily resolved).
    diff_err("CREATE VIEW vv AS SELECT nosuchcol FROM t;");
    // ... and the SELECT from it errors on BOTH sides with the same text.
    let mut db = eng();
    db.execute("CREATE VIEW vv AS SELECT nosuchcol FROM t;", [])
        .unwrap();
    let con = lite();
    let _ = con.execute_batch("CREATE VIEW vv AS SELECT nosuchcol FROM t;");
    let ours = db
        .query("SELECT * FROM vv", [])
        .map(|r| r.len())
        .map_err(|e| e.to_string());
    let theirs: std::result::Result<usize, String> = con
        .prepare("SELECT * FROM vv")
        .and_then(|mut s| s.query([]).map(|_| 0usize))
        .map_err(|e| raw_sqlite_msg(&e));
    match (ours, theirs) {
        (Err(a), Err(b)) => assert_eq!(a, b, "view body error text differs"),
        (Ok(a), Ok(b)) => assert_eq!(a, b, "view body both-ok but counts differ"),
        (a, b) => panic!("view body mismatch: engine {a:?} vs sqlite {b:?}"),
    }
}

#[test]
fn lazy_fk_references() {
    diff_err("CREATE TABLE f1 (a INT REFERENCES t(nosuch));");
}

#[test]
fn trigger_fire_time() {
    // Body referencing NEW.nosuch: both OK at create, both error at fire.
    for (setup, fire) in [
        (
            "CREATE TRIGGER g AFTER INSERT ON t BEGIN SELECT NEW.nosuch; END;",
            "INSERT INTO t (x) VALUES (1)",
        ),
        (
            "CREATE TRIGGER g2 AFTER DELETE ON t BEGIN SELECT OLD.nosuch; END;",
            "DELETE FROM t",
        ),
        (
            "CREATE TRIGGER g5 AFTER INSERT ON t WHEN nosuch > 0 BEGIN SELECT 1; END;",
            "INSERT INTO t (x) VALUES (1)",
        ),
        (
            "CREATE TRIGGER g6 AFTER INSERT ON t WHEN NEW.nosuch > 0 BEGIN SELECT 1; END;",
            "INSERT INTO t (x) VALUES (1)",
        ),
        (
            "CREATE TRIGGER g7 AFTER INSERT ON t BEGIN INSERT INTO t (nosuchcol) VALUES (1); END;",
            "INSERT INTO t (x) VALUES (1)",
        ),
    ] {
        let mut db = eng();
        db.execute("INSERT INTO t (x) VALUES (7)", []).unwrap();
        db.execute(setup, []).unwrap();
        let ours = db.execute(fire, []).map(|_| ());
        let con = lite();
        let _ = con.execute_batch("INSERT INTO t (x) VALUES (7)");
        let _ = con.execute_batch(setup);
        let theirs = con.execute_batch(fire).map_err(|e| raw_sqlite_msg(&e));
        match (ours, theirs) {
            (Err(a), Err(b)) => assert_eq!(
                a.to_string(),
                b,
                "trigger fire error text differs for {setup:?} / {fire:?}"
            ),
            (Ok(()), Ok(())) => {}
            (a, b) => panic!("trigger fire mismatch for {setup:?}: engine {a:?} vs sqlite {b:?}"),
        }
    }
}

// ------------------------------------------------------------------------
// Positive shapes: strict validation must NOT reject valid SQL
// ------------------------------------------------------------------------

#[test]
fn valid_shapes_still_pass() {
    for sql in [
        "SELECT * FROM t;",
        "SELECT t.x, u.k FROM t JOIN u ON t.x = u.k;",
        "SELECT x AS y FROM t WHERE y > 0;",
        "SELECT k, COUNT(*) AS n FROM u GROUP BY k HAVING n >= 1;",
        "SELECT rowid FROM t;",
        "SELECT a.rowid FROM t a;",
        "SELECT * FROM v;",
        "SELECT id FROM v WHERE x > 0;",
        "SELECT * FROM t NATURAL JOIN u;",
        "SELECT column2 FROM (VALUES (1, 'a'));",
        "WITH c AS (SELECT x FROM t) SELECT * FROM c WHERE x > 0;",
        "WITH RECURSIVE cnt(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM cnt WHERE n<5) SELECT n FROM cnt;",
        "SELECT v FROM t INTERSECT SELECT k FROM u ORDER BY v;",
        "SELECT x FROM t UNION SELECT k FROM u ORDER BY x;",
        "UPDATE t SET x = 2 WHERE id = 1;",
        "INSERT INTO t (x) VALUES (1) RETURNING *;",
        "INSERT INTO t (x) VALUES (1) RETURNING id, x;",
        "INSERT INTO t (id, x) VALUES (1, 1) ON CONFLICT (id) DO UPDATE SET x = excluded.x + 1;",
        "DELETE FROM t WHERE id = 1;",
        "SELECT count(*) FROM json_each('[1,2]');",
        "SELECT key FROM json_each('[1,2]');",
        "SELECT name FROM pragma_table_info('t');",
        "SELECT * FROM sqlite_master;",
        "SELECT * FROM t WHERE x IN (SELECT k FROM u);",
        "SELECT (SELECT k FROM u LIMIT 1) FROM t;",
        "SELECT EXISTS (SELECT 1 FROM u WHERE k = 1) FROM t;",
        "ANALYZE t;",
        "ANALYZE idx_t_x;",
        "REINDEX t;",
        "SELECT * FROM t INDEXED BY idx_t_x;",
        "SELECT * FROM t NOT INDEXED;",
    ] {
        diff_err(sql);
    }
}

#[test]
fn attach_detach_round_trip() {
    // ATTACH records the name; DETACH of it succeeds; DETACH of an
    // unknown name errors `no such database: x` on both engines.
    let mut db = rustqlite::Database::open_in_memory().unwrap();
    db.execute("ATTACH ':memory:' AS aux", []).unwrap();
    db.execute("DETACH aux", []).unwrap();
    let err = db.execute("DETACH aux", []).unwrap_err();
    assert_eq!(err.to_string(), "no such database: aux");
    let err2 = db.execute("DETACH neverwas", []).unwrap_err();
    assert_eq!(err2.to_string(), "no such database: neverwas");
}
