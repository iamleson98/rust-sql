//! ALTER TABLE RENAME COLUMN / DROP COLUMN tests (mirrors SQLite semantics).
use rustqlite::{Database, Value};

fn memdb() -> Database {
    Database::open_in_memory().unwrap()
}

#[test]
fn rename_column_basic() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (name, val) VALUES ('a', 1), ('b', 2)", []).unwrap();

    db.execute("ALTER TABLE t RENAME COLUMN name TO label", []).unwrap();

    // New name selects real data.
    let rows = db.query("SELECT label, val FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "a");
    assert_eq!(rows[1][0].as_text(), "b");
    // WHERE on the new name works precisely.
    let rows = db.query("SELECT COUNT(*) FROM t WHERE label = 'a'", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);

    // Inserts via new name work.
    db.execute("INSERT INTO t (label, val) VALUES ('c', 3)", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 3);

    // WHERE on the new name.
    let rows = db.query("SELECT id FROM t WHERE label = 'b'", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_integer(), 2);
}

#[test]
fn rename_column_survives_reopen() {
    let path = std::env::temp_dir().join("rename_col_reopen.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER, y TEXT)", []).unwrap();
        db.execute("INSERT INTO t (x, y) VALUES (10, 'ten')", []).unwrap();
        db.execute("ALTER TABLE t RENAME COLUMN x TO alpha", []).unwrap();
        db.flush().unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    let rows = db.query("SELECT alpha, y FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 10);
    assert_eq!(rows[0][1].as_text(), "ten");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rename_column_updates_indexes_and_check() {
    let mut db = memdb();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER CHECK (v > 0), w TEXT)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t (v, w) VALUES (1, 'a'), (5, 'b'), (9, 'c')", []).unwrap();
    db.execute("CREATE INDEX idx_v ON t(v)", []).unwrap();

    db.execute("ALTER TABLE t RENAME COLUMN v TO score", []).unwrap();

    // Index still serves the renamed column.
    let rows = db.query("SELECT id FROM t WHERE score = 5", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_integer(), 2);

    // CHECK constraint survived the rename.
    assert!(db.execute("INSERT INTO t (score, w) VALUES (-1, 'bad')", []).is_err());
    db.execute("INSERT INTO t (score, w) VALUES (7, 'ok')", []).unwrap();

    // The index survived with correct contents.
    let rows = db.query("SELECT id FROM t WHERE score BETWEEN 4 AND 8 ORDER BY score", []).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0].as_integer(), 2);
    assert_eq!(rows[1][0].as_integer(), 4);
}

#[test]
fn rename_column_rewrites_fk_references() {
    let mut db = memdb();
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT UNIQUE)", []).unwrap();
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, pcode TEXT REFERENCES p(code))", []).unwrap();
    db.execute("INSERT INTO p (code) VALUES ('x1')", []).unwrap();
    db.execute("INSERT INTO c (pcode) VALUES ('x1')", []).unwrap();

    db.execute("ALTER TABLE p RENAME COLUMN code TO tag", []).unwrap();

    // FK still enforced with the new parent column name.
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    assert!(db.execute("INSERT INTO c (pcode) VALUES ('nope')", []).is_err());
    db.execute("INSERT INTO c (pcode) VALUES ('x1')", []).unwrap();

    // (FK enforcement with the renamed parent column verified above.)
}

#[test]
fn rename_column_trigger_body() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, log TEXT)", []).unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO t (v, log) VALUES (NEW.v * -1, 'negated'); END",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t (v, log) VALUES (10, 'orig')", []).unwrap();

    // Renaming v must rewrite NEW.v and the INSERT column list.
    db.execute("ALTER TABLE t RENAME COLUMN v TO amount", []).unwrap();
    db.execute("INSERT INTO t (amount, log) VALUES (6, 'second')", []).unwrap();

    // Trigger rows: insert(10) fires once (SQLite default: no recursive
    // triggers) → -10; insert(6) fires once → -6.
    let rows = db.query("SELECT amount FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0].as_integer(), 10);
    assert_eq!(rows[1][0].as_integer(), -10);
    assert_eq!(rows[2][0].as_integer(), 6);
    assert_eq!(rows[3][0].as_integer(), -6); // trigger fired with NEW.amount
}

#[test]
fn rename_column_view_sql() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES (1), (2)", []).unwrap();
    db.execute("CREATE VIEW big AS SELECT id FROM t WHERE v > 1", []).unwrap();

    db.execute("ALTER TABLE t RENAME COLUMN v TO val", []).unwrap();

    let rows = db.query("SELECT * FROM big", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_integer(), 2);
}

#[test]
fn rename_column_errors() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)", []).unwrap();
    // Unknown column.
    assert!(db.execute("ALTER TABLE t RENAME COLUMN zz TO q", []).is_err());
    // Duplicate target name.
    assert!(db.execute("ALTER TABLE t RENAME COLUMN a TO b", []).is_err());
    // Rowid alias.
    assert!(db.execute("ALTER TABLE t RENAME COLUMN id TO key", []).is_err());
}

#[test]
fn drop_column_basic() {
    let mut db = memdb();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t (name, val, score) VALUES ('a', 1, 1.5), ('b', 2, 2.5)", []).unwrap();

    db.execute("ALTER TABLE t DROP COLUMN score", []).unwrap();

    let rows = db.query("SELECT * FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 3); // id, name, val
    assert_eq!(rows[0][0].as_integer(), 1);
    assert_eq!(rows[0][1].as_text(), "a");
    assert_eq!(rows[0][2].as_integer(), 1);

    // Insert without the dropped column.
    db.execute("INSERT INTO t (name, val) VALUES ('c', 3)", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 3);
}

#[test]
fn drop_column_middle_position() {
    // Dropping a MIDDLE column must shift the remaining columns correctly.
    let mut db = memdb();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t (a, b, c) VALUES (1, 'one', 1.25), (2, 'two', 2.5)", []).unwrap();

    db.execute("ALTER TABLE t DROP COLUMN b", []).unwrap();

    let rows = db.query("SELECT a, c FROM t ORDER BY id", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    assert_eq!(rows[0][1].as_real(), 1.25);
    assert_eq!(rows[1][0].as_integer(), 2);
    assert_eq!(rows[1][1].as_real(), 2.5);

    // The rowid alias (id, col 0) is before the dropped col; alias intact.
    let rows = db.query("SELECT id, a FROM t WHERE id = 2", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 2);
    assert_eq!(rows[0][1].as_integer(), 2);
}

#[test]
fn drop_column_alias_after_dropped() {
    // INTEGER PRIMARY KEY NOT in first position: dropping a column BEFORE
    // it must keep the alias pointing at the right column.
    let mut db = memdb();
    db.execute("CREATE TABLE t (name TEXT, id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (name, v) VALUES ('a', 10)", []).unwrap();
    db.execute("ALTER TABLE t DROP COLUMN name", []).unwrap();
    let rows = db.query("SELECT id, v FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    assert_eq!(rows[0][1].as_integer(), 10);
    // rowid lookup still works.
    let rows = db.query("SELECT v FROM t WHERE id = 1", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 10);
}

#[test]
fn drop_column_survives_reopen() {
    let path = std::env::temp_dir().join("drop_col_reopen.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)", []).unwrap();
        db.execute("INSERT INTO t (a, b) VALUES (1, 'x')", []).unwrap();
        db.execute("ALTER TABLE t DROP COLUMN a", []).unwrap();
        db.flush().unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    let rows = db.query("SELECT id, b FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    assert_eq!(rows[0][1].as_text(), "x");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_column_errors() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER UNIQUE, b INTEGER, ck INTEGER CHECK (ck > 0))", []).unwrap();
    db.execute("INSERT INTO t (a, b, ck) VALUES (1, 2, 3)", []).unwrap();
    // Unknown.
    assert!(db.execute("ALTER TABLE t DROP COLUMN zz", []).is_err());
    // Rowid alias.
    assert!(db.execute("ALTER TABLE t DROP COLUMN id", []).is_err());
    // UNIQUE.
    assert!(db.execute("ALTER TABLE t DROP COLUMN a", []).is_err());
    // CHECK-referenced.
    assert!(db.execute("ALTER TABLE t DROP COLUMN ck", []).is_err());
    // Indexed.
    db.execute("CREATE INDEX idx_b ON t(b)", []).unwrap();
    assert!(db.execute("ALTER TABLE t DROP COLUMN b", []).is_err());
}

#[test]
fn drop_column_referenced_by_fk() {
    let mut db = memdb();
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT)", []).unwrap();
    db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, pcode TEXT REFERENCES p(code))", []).unwrap();
    // Parent-side reference blocks the drop.
    assert!(db.execute("ALTER TABLE p DROP COLUMN code", []).is_err());
    // Child-side FK use blocks the drop.
    assert!(db.execute("ALTER TABLE c DROP COLUMN pcode", []).is_err());
}

#[test]
fn drop_column_referenced_by_view() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
    db.execute("INSERT INTO t (v) VALUES (1)", []).unwrap();
    db.execute("CREATE VIEW vv AS SELECT id FROM t WHERE v > 0", []).unwrap();
    assert!(db.execute("ALTER TABLE t DROP COLUMN v", []).is_err());
}

#[test]
fn drop_column_updates_triggers() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, log TEXT)", []).unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN UPDATE t SET log = 'seen' WHERE v = NEW.v; END",
        [],
    )
    .unwrap();
    // Trigger references v — drop must be rejected.
    assert!(db.execute("ALTER TABLE t DROP COLUMN v", []).is_err());
    // log is referenced via SET in the trigger: also rejected.
    assert!(db.execute("ALTER TABLE t DROP COLUMN log", []).is_err());
}

#[test]
fn drop_column_many_rows() {
    let mut db = memdb();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute("INSERT INTO t (a, b) VALUES (?, ?)",
            [Value::Integer(i), Value::Text(format!("row{}", i))]).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    db.execute("ALTER TABLE t DROP COLUMN a", []).unwrap();

    let rows = db.query("SELECT COUNT(*), MIN(id), MAX(id) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1000);
    assert_eq!(rows[0][1].as_integer(), 1);
    assert_eq!(rows[0][2].as_integer(), 1000);
    let rows = db.query("SELECT b FROM t WHERE id = 500", []).unwrap();
    assert_eq!(rows[0][0].as_text(), "row500");
}
