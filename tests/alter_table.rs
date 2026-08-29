//! ALTER TABLE RENAME TO / ADD COLUMN — catalog moves, schema persistence,
//! index/trigger attachment, FK reference rewriting, and default back-fill.
use rustqlite::{Database, Value};

#[test]
fn alter_rename_basic() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3)", []).unwrap();
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
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (7)", []).unwrap();
        db.execute("ALTER TABLE t RENAME TO renamed", []).unwrap();
        db.execute("INSERT INTO renamed (x) VALUES (8)", []).unwrap();
    }
    let db = Database::open(path).unwrap();
    let rows = db.query("SELECT id, x FROM renamed ORDER BY id", []).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Integer(7));
    assert_eq!(rows[1][1], Value::Integer(8));
    assert!(db.query("SELECT * FROM t", []).is_err());
}

#[test]
fn alter_rename_keeps_indexes_attached() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", []).unwrap();
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
        db2.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, y INTEGER)", []).unwrap();
        db2.execute("INSERT INTO a (y) VALUES (1), (2)", []).unwrap();
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
    db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
        [],
    )
    .unwrap();
    db.execute("ALTER TABLE parent RENAME TO guardian", []).unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    // FK now resolves against the renamed parent.
    db.execute("INSERT INTO guardian (id) VALUES (1)", []).unwrap();
    db.execute("INSERT INTO child (pid) VALUES (1)", []).unwrap();
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
    assert_eq!(db.query("SELECT COUNT(*) FROM a", []).unwrap()[0][0], Value::Integer(0));
    assert_eq!(db.query("SELECT COUNT(*) FROM b", []).unwrap()[0][0], Value::Integer(0));
}

#[test]
fn alter_add_column_basic() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2)", []).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN name TEXT", []).unwrap();
    // Existing rows see NULL for the new column.
    let rows = db.query("SELECT id, x, name FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0], vec![Value::Integer(1), Value::Integer(1), Value::Null]);
    // New inserts can use it.
    db.execute("INSERT INTO t (x, name) VALUES (3, 'three')", []).unwrap();
    let rows = db.query("SELECT name FROM t WHERE x = 3", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Text("three".into())]]);
}

#[test]
fn alter_add_column_default_backfill() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1), (2), (3)", []).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN status TEXT DEFAULT 'active'", []).unwrap();
    // Existing rows materialize the default (SQLite read-time semantics).
    let rows = db.query("SELECT status FROM t ORDER BY id", []).unwrap();
    for r in rows {
        assert_eq!(r[0], Value::Text("active".into()));
    }
    // COUNT on the defaulted column works.
    let n = db.query("SELECT COUNT(*) FROM t WHERE status = 'active'", []).unwrap();
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
    db.execute("ALTER TABLE t ADD COLUMN y INTEGER NOT NULL DEFAULT 0", []).unwrap();
    let rows = db.query("SELECT y FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(0));
}

#[test]
fn alter_add_column_persists_across_reopen() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path();
    {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (5)", []).unwrap();
        db.execute("ALTER TABLE t ADD COLUMN note TEXT DEFAULT 'hi'", []).unwrap();
    }
    let mut db = Database::open(path).unwrap();
    let rows = db.query("SELECT x, note FROM t", []).unwrap();
    assert_eq!(rows[0], vec![Value::Integer(5), Value::Text("hi".into())]);
    // And new inserts see the wider column list.
    db.execute("INSERT INTO t (x, note) VALUES (6, 'there')", []).unwrap();
    let rows = db.query("SELECT note FROM t WHERE x = 6", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Text("there".into())]]);
}

#[test]
fn alter_add_column_with_index_still_works() {
    // Index maintenance after widening: inserts must update the index with
    // the full (wider) row encoding.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (x) VALUES (1)", []).unwrap();
    db.execute("CREATE INDEX ix ON t(x)", []).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN tag TEXT", []).unwrap();
    db.execute("INSERT INTO t (x, tag) VALUES (2, 'b')", []).unwrap();
    let rows = db.query("SELECT tag FROM t WHERE x = 2", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Text("b".into())]]);
    // DELETE by indexed column after the widen.
    db.execute("DELETE FROM t WHERE x = 2", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
}

#[test]
fn alter_rename_column_and_drop_parse_but_reject() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
    let err = db.execute("ALTER TABLE t RENAME COLUMN x TO y", []);
    assert!(err.is_err(), "RENAME COLUMN is parsed but unsupported");
    let err = db.execute("ALTER TABLE t DROP COLUMN x", []);
    assert!(err.is_err(), "DROP COLUMN is parsed but unsupported");
    // Table intact.
    assert_eq!(db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0], Value::Integer(0));
}
