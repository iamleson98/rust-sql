//! ALTER TABLE RENAME TO / ADD COLUMN — catalog moves, schema persistence,
//! index/trigger attachment, FK reference rewriting, and default back-fill.
use rustqlite::{Database, Value};

#[test]
fn alter_rename_basic() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3)", [])
        .unwrap();
    db.execute("ALTER TABLE t RENAME TO t2", []).unwrap();
    // Data survives under the new name.
    let rows = db.query("SELECT COUNT(*), MAX(x) FROM t2", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(3));
    assert_eq!(rows[0][1], Value::Integer(3));
    // Old name is gone.
    assert!(db.query("SELECT * FROM t", []).is_err());
    // New inserts work (rowid continuation).
    db.execute("INSERT INTO t2 (x) VALUES (4)", []).unwrap();
    let rows = db.query("SELECT id FROM t2 ORDER BY id", []).unwrap();
    assert_eq!(rows[3][0], Value::Integer(4));
}

#[test]
fn alter_rename_persists_across_reopen() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path();
    {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (7)", []).unwrap();
        db.execute("ALTER TABLE t RENAME TO renamed", []).unwrap();
        db.execute("INSERT INTO renamed (x) VALUES (8)", [])
            .unwrap();
    }
    let db = Database::open(path).unwrap();
    let rows = db
        .query("SELECT id, x FROM renamed ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Integer(7));
    assert_eq!(rows[1][1], Value::Integer(8));
    assert!(db.query("SELECT * FROM t", []).is_err());
}

#[test]
fn alter_rename_keeps_indexes_attached() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", [])
        .unwrap();
    db.execute("CREATE INDEX idx_x ON t(x)", []).unwrap();
    db.execute("ALTER TABLE t RENAME TO t2", []).unwrap();
    // Indexed lookups must still work (the catalog's index registration
    // moved with the table).
    let rows = db.query("SELECT id FROM t2 WHERE x = 20", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    // And the index schema row survives a reopen.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path();
    {
        let mut db2 = Database::open(path).unwrap();
        db2.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, y INTEGER)", [])
            .unwrap();
        db2.execute("INSERT INTO a (y) VALUES (1), (2)", [])
            .unwrap();
        db2.execute("CREATE INDEX ia ON a(y)", []).unwrap();
        db2.execute("ALTER TABLE a RENAME TO b", []).unwrap();
    }
    let db3 = Database::open(path).unwrap();
    let rows = db3.query("SELECT id FROM b WHERE y = 2", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn alter_rename_rewrites_fk_references() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    db.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
        [],
    )
    .unwrap();
    db.execute("ALTER TABLE parent RENAME TO guardian", [])
        .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    // FK now resolves against the renamed parent.
    db.execute("INSERT INTO guardian (id) VALUES (1)", [])
        .unwrap();
    db.execute("INSERT INTO child (pid) VALUES (1)", [])
        .unwrap();
    let err = db.execute("INSERT INTO child (pid) VALUES (99)", []);
    assert!(err.is_err(), "FK must survive the rename");
    let err = db.execute("DELETE FROM guardian WHERE id = 1", []);
    assert!(err.is_err(), "parent-side FK must survive the rename");
}

#[test]
fn alter_rename_collision_rejected() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (x INTEGER)", []).unwrap();
    db.execute("CREATE TABLE b (x INTEGER)", []).unwrap();
    let err = db.execute("ALTER TABLE a RENAME TO b", []);
    assert!(err.is_err(), "rename onto an existing name must fail");
    // Both tables intact after the failure.
    assert_eq!(
        db.query("SELECT COUNT(*) FROM a", []).unwrap()[0][0],
        Value::Integer(0)
    );
    assert_eq!(
        db.query("SELECT COUNT(*) FROM b", []).unwrap()[0][0],
        Value::Integer(0)
    );
}

#[test]
fn alter_add_column_basic() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2)", []).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN name TEXT", [])
        .unwrap();
    // Existing rows see NULL for the new column.
    let rows = db
        .query("SELECT id, x, name FROM t ORDER BY id", [])
        .unwrap();
    assert_eq!(
        rows[0],
        vec![Value::Integer(1), Value::Integer(1), Value::Null]
    );
    // New inserts can use it.
    db.execute("INSERT INTO t (x, name) VALUES (3, 'three')", [])
        .unwrap();
    let rows = db.query("SELECT name FROM t WHERE x = 3", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Text("three".into())]]);
}

#[test]
fn alter_add_column_default_backfill() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3)", [])
        .unwrap();
    db.execute("ALTER TABLE t ADD COLUMN status TEXT DEFAULT 'active'", [])
        .unwrap();
    // Existing rows materialize the default (SQLite read-time semantics).
    let rows = db.query("SELECT status FROM t ORDER BY id", []).unwrap();
    for r in rows {
        assert_eq!(r[0], Value::Text("active".into()));
    }
    // COUNT on the defaulted column works.
    let n = db
        .query("SELECT COUNT(*) FROM t WHERE status = 'active'", [])
        .unwrap();
    assert_eq!(n[0][0], Value::Integer(3));
}

#[test]
fn alter_add_column_restrictions() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    // NOT NULL without DEFAULT is rejected (SQLite rule).
    let err = db.execute("ALTER TABLE t ADD COLUMN y INTEGER NOT NULL", []);
    assert!(err.is_err());
    // PRIMARY KEY columns can't be added.
    let err = db.execute("ALTER TABLE t ADD COLUMN y INTEGER PRIMARY KEY", []);
    assert!(err.is_err());
    // NOT NULL WITH a default is fine.
    db.execute("INSERT INTO t VALUES (1)", []).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN y INTEGER NOT NULL DEFAULT 0", [])
        .unwrap();
    let rows = db.query("SELECT y FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(0));
}

#[test]
fn alter_add_column_persists_across_reopen() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path();
    {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (5)", []).unwrap();
        db.execute("ALTER TABLE t ADD COLUMN note TEXT DEFAULT 'hi'", [])
            .unwrap();
    }
    let mut db = Database::open(path).unwrap();
    let rows = db.query("SELECT x, note FROM t", []).unwrap();
    assert_eq!(rows[0], vec![Value::Integer(5), Value::Text("hi".into())]);
    // And new inserts see the wider column list.
    db.execute("INSERT INTO t (x, note) VALUES (6, 'there')", [])
        .unwrap();
    let rows = db.query("SELECT note FROM t WHERE x = 6", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Text("there".into())]]);
}

#[test]
fn alter_add_column_with_index_still_works() {
    // Index maintenance after widening: inserts must update the index with
    // the full (wider) row encoding.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t (x) VALUES (1)", []).unwrap();
    db.execute("CREATE INDEX ix ON t(x)", []).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN tag TEXT", []).unwrap();
    db.execute("INSERT INTO t (x, tag) VALUES (2, 'b')", [])
        .unwrap();
    let rows = db.query("SELECT tag FROM t WHERE x = 2", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Text("b".into())]]);
    // DELETE by indexed column after the widen.
    db.execute("DELETE FROM t WHERE x = 2", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
}

#[test]
fn alter_rename_column_and_drop_work() {
    // RENAME COLUMN and DROP COLUMN are now implemented (see
    // tests/alter_column.rs for full coverage). The single-column
    // rejection for DROP is still enforced here.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (x INTEGER, y INTEGER)", [])
        .unwrap();
    db.execute("INSERT INTO t (x, y) VALUES (1, 2)", [])
        .unwrap();
    db.execute("ALTER TABLE t RENAME COLUMN x TO a", [])
        .unwrap();
    let rows = db.query("SELECT a, y FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
    db.execute("ALTER TABLE t DROP COLUMN y", []).unwrap();
    let rows = db.query("SELECT a FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));

    // A one-column table refuses DROP COLUMN.
    let mut db2 = Database::open_in_memory().unwrap();
    db2.execute("CREATE TABLE u (only INTEGER)", []).unwrap();
    assert!(db2.execute("ALTER TABLE u DROP COLUMN only", []).is_err());
}

#[test]
fn alter_add_column_lands_before_table_constraints() {
    // SQLite places an ADD COLUMN definition with the column defs,
    // BEFORE table-level constraints. Appending it after a trailing
    // FOREIGN KEY produced DDL that C SQLite rejects when re-reading
    // sqlite_master ("malformed database schema — near '<col>': syntax
    // error"), which broke every external tool opening the file
    // (sqlite3 CLI, python sqlite3, backup agents). This is the exact
    // shape the pdf-tts migration chain produces via sea-orm.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    db.execute(
        "CREATE TABLE child (\
         id INTEGER PRIMARY KEY, \
         pid INTEGER, \
         FOREIGN KEY (pid) REFERENCES parent(id) ON DELETE CASCADE)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO child (pid) VALUES (1)", [])
        .unwrap();
    db.execute("ALTER TABLE child ADD COLUMN speed REAL", [])
        .unwrap();

    let ddl = db
        .query("SELECT sql FROM sqlite_master WHERE name = 'child'", [])
        .unwrap();
    let sql = match &ddl[0][0] {
        Value::Text(s) => s.clone(),
        v => panic!("expected TEXT ddl, got {v:?}"),
    };
    // The new column must precede the table-level FOREIGN KEY…
    let col_pos = sql.find("speed REAL").expect("column in ddl");
    let fk_pos = sql.find("FOREIGN KEY").expect("constraint in ddl");
    assert!(
        col_pos < fk_pos,
        "added column must precede table constraints, ddl: {sql}"
    );
    // …and the constraint must still close the list.
    let close_pos = sql.rfind(')').unwrap();
    assert!(
        fk_pos < close_pos,
        "constraint still inside the list, ddl: {sql}"
    );

    // Column order is unchanged for positional access (speed is still
    // the LAST column) and the value semantics survive.
    let rows = db.query("SELECT id, pid, speed FROM child", []).unwrap();
    assert_eq!(rows[0][2], Value::Null);
    db.execute("INSERT INTO child (pid, speed) VALUES (1, 1.5)", [])
        .unwrap();
    let rows = db
        .query("SELECT speed FROM child WHERE id = 2", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Real(1.5));
}

#[test]
fn alter_add_column_before_named_constraint_and_quoted_ddl() {
    // sea-orm-style quoted DDL with a NAMED table constraint: the added
    // column must land before `CONSTRAINT … FOREIGN KEY`, and quoted
    // identifiers / nested parens must not confuse the scanner.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE IF NOT EXISTS \"ver\" (\
         \"id\" varchar NOT NULL PRIMARY KEY, \
         \"doc\" varchar NOT NULL, \
         \"cfg\" varchar NULL DEFAULT 'a,b(c)', \
         CONSTRAINT \"fk_ver_doc\" FOREIGN KEY (\"doc\") REFERENCES \"docs\" (\"id\") ON DELETE CASCADE)",
        [],
    )
    .unwrap();
    db.execute("ALTER TABLE \"ver\" ADD COLUMN speed double NULL", [])
        .unwrap();

    let ddl = db
        .query("SELECT sql FROM sqlite_master WHERE name = 'ver'", [])
        .unwrap();
    let sql = match &ddl[0][0] {
        Value::Text(s) => s.clone(),
        v => panic!("expected TEXT ddl, got {v:?}"),
    };
    let col_pos = sql.find("speed double NULL").expect("column in ddl");
    let named_fk = sql.find("CONSTRAINT \"fk_ver_doc\"").expect("named fk");
    assert!(
        col_pos < named_fk,
        "column must precede the named constraint, ddl: {sql}"
    );
    // The string-literal DEFAULT with commas+parens survived intact.
    assert!(
        sql.contains("'a,b(c)'"),
        "default literal intact, ddl: {sql}"
    );

    // Roundtrip: reopen and the schema still parses + the column reads.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path();
    {
        let mut file_db = Database::open(path).unwrap();
        file_db
            .execute("CREATE TABLE docs (id varchar PRIMARY KEY)", [])
            .unwrap();
        file_db.execute(
            "CREATE TABLE IF NOT EXISTS \"ver\" (\
             \"id\" varchar NOT NULL PRIMARY KEY, \
             \"doc\" varchar NOT NULL, \
             CONSTRAINT \"fk_ver_doc\" FOREIGN KEY (\"doc\") REFERENCES \"docs\" (\"id\") ON DELETE CASCADE)",
            [],
        )
        .unwrap();
        file_db
            .execute(
                "INSERT INTO \"ver\" (\"id\", \"doc\") VALUES ('v1', 'd1')",
                [],
            )
            .unwrap();
        file_db
            .execute("ALTER TABLE \"ver\" ADD COLUMN speed double NULL", [])
            .unwrap();
        file_db
            .execute("UPDATE \"ver\" SET speed = 2.5 WHERE \"id\" = 'v1'", [])
            .unwrap();
    }
    let file_db = Database::open(path).unwrap();
    let rows = file_db
        .query("SELECT \"id\", speed FROM \"ver\"", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Text("v1".into()));
    assert_eq!(rows[0][1], Value::Real(2.5));
}

#[test]
fn alter_add_column_preserves_statement_tail() {
    // STRICT / WITHOUT ROWID live AFTER the closing ')' — the rewrite
    // must keep them (the old splice dropped everything past ')').
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (a TEXT, b TEXT CHECK (length(b) < 10), PRIMARY KEY (a)) STRICT",
        [],
    )
    .unwrap();
    db.execute("ALTER TABLE t ADD COLUMN c TEXT", []).unwrap();
    let ddl = db
        .query("SELECT sql FROM sqlite_master WHERE name = 't'", [])
        .unwrap();
    let sql = match &ddl[0][0] {
        Value::Text(s) => s.clone(),
        v => panic!("expected TEXT ddl, got {v:?}"),
    };
    assert!(sql.contains("STRICT"), "tail preserved, ddl: {sql}");
    // Column inserted before the table-level PRIMARY KEY (…), the CHECK
    // inside a column def does not count as a table constraint.
    let c_pos = sql.find("c TEXT").expect("new col");
    let pk_pos = sql.find("PRIMARY KEY (a)").expect("table pk");
    assert!(c_pos < pk_pos, "column before table PK, ddl: {sql}");
    // STRICT tables still work after the widen.
    db.execute("INSERT INTO t (a, b, c) VALUES ('x', 'yy', 'zz')", [])
        .unwrap();
    let rows = db.query("SELECT c FROM t WHERE a = 'x'", []).unwrap();
    assert_eq!(rows[0][0], Value::Text("zz".into()));
}

#[test]
fn alter_add_column_plain_table_still_appends_at_end() {
    // No table-level constraints: the column is appended after the last
    // column definition exactly as before.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b INTEGER)", [])
        .unwrap();
    db.execute("ALTER TABLE t ADD COLUMN c INTEGER", [])
        .unwrap();
    let ddl = db
        .query("SELECT sql FROM sqlite_master WHERE name = 't'", [])
        .unwrap();
    let sql = match &ddl[0][0] {
        Value::Text(s) => s.clone(),
        v => panic!("expected TEXT ddl, got {v:?}"),
    };
    // Column ORDER is positional — c must be the LAST column.
    assert!(
        sql.find("b INTEGER").unwrap() < sql.find("c INTEGER").unwrap(),
        "appended after last column, ddl: {sql}"
    );
    // Positional insert reflects the order.
    db.execute("INSERT INTO t VALUES (1, 2, 3)", []).unwrap();
    let rows = db.query("SELECT c FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(3));
}
