//! Foreign-key enforcement end to end: PRAGMA toggle, child-side checks on
//! INSERT/UPDATE, parent-side checks on DELETE, and all ON DELETE actions.
use rustqlite::{Database, Value};

fn fk_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO parent (name) VALUES ('a'), ('b')", []).unwrap();
    db
}

#[test]
fn fk_off_by_default_allows_orphans() {
    // SQLite semantics: FKs are OFF unless the pragma enables them.
    let mut db = fk_db();
    db.execute("INSERT INTO child (pid) VALUES (999)", []).unwrap();
    let rows = db.query("SELECT pid FROM child", []).unwrap();
    assert_eq!(rows.len(), 1, "orphan insert must pass with FKs off");
}

#[test]
fn fk_on_rejects_orphan_insert() {
    let mut db = fk_db();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    let err = db.execute("INSERT INTO child (pid) VALUES (999)", []);
    assert!(err.is_err(), "orphan insert must fail with FKs on");
    assert!(
        err.unwrap_err().to_string().contains("FOREIGN KEY"),
        "error must mention FOREIGN KEY"
    );
    // Valid parent still works.
    db.execute("INSERT INTO child (pid) VALUES (1)", []).unwrap();
    // NULL passes (MATCH SIMPLE).
    db.execute("INSERT INTO child (pid) VALUES (NULL)", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM child", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(2));
}

#[test]
fn fk_on_rejects_orphan_update() {
    let mut db = fk_db();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO child (pid) VALUES (1)", []).unwrap();
    let err = db.execute("UPDATE child SET pid = 999 WHERE id = 1", []);
    assert!(err.is_err(), "orphaning UPDATE must fail");
    // Reparenting to a valid parent works.
    db.execute("UPDATE child SET pid = 2 WHERE id = 1", []).unwrap();
    let rows = db.query("SELECT pid FROM child", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(2));
}

#[test]
fn fk_on_rejects_parent_delete_with_children() {
    let mut db = fk_db();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO child (pid) VALUES (1)", []).unwrap();
    let err = db.execute("DELETE FROM parent WHERE id = 1", []);
    assert!(err.is_err(), "delete of referenced parent must fail");
    // Unreferenced parent deletes fine.
    db.execute("DELETE FROM parent WHERE id = 2", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM parent", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
}

#[test]
fn fk_on_delete_cascade() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE)",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO p (id) VALUES (1), (2)", []).unwrap();
    db.execute("INSERT INTO c (pid) VALUES (1), (1), (2)", []).unwrap();
    db.execute("DELETE FROM p WHERE id = 1", []).unwrap();
    let rows = db.query("SELECT pid FROM c ORDER BY pid", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(2)]], "children must cascade");
}

#[test]
fn fk_on_delete_set_null() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE SET NULL)",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO p (id) VALUES (1), (2)", []).unwrap();
    db.execute("INSERT INTO c (pid) VALUES (1), (2)", []).unwrap();
    db.execute("DELETE FROM p WHERE id = 1", []).unwrap();
    let rows = db.query("SELECT pid FROM c ORDER BY id", []).unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Null], vec![Value::Integer(2)]],
        "child key must become NULL"
    );
}

#[test]
fn fk_on_delete_set_default() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER DEFAULT 0 REFERENCES p(id) ON DELETE SET DEFAULT)",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    // Parent 0 exists so the rewritten children stay valid.
    db.execute("INSERT INTO p (id) VALUES (0), (1)", []).unwrap();
    db.execute("INSERT INTO c (pid) VALUES (1)", []).unwrap();
    db.execute("DELETE FROM p WHERE id = 1", []).unwrap();
    let rows = db.query("SELECT pid FROM c", []).unwrap();
    assert_eq!(rows, vec![vec![Value::Integer(0)]], "child key must become the default");
}

#[test]
fn fk_composite_key_enforcement() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE pt (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE ct (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, FOREIGN KEY (x, y) REFERENCES pt(a, b))",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO pt VALUES (1, 2)", []).unwrap();
    // Wrong second component must fail.
    let err = db.execute("INSERT INTO ct (x, y) VALUES (1, 3)", []);
    assert!(err.is_err(), "composite FK mismatch must fail");
    // Exact match passes.
    db.execute("INSERT INTO ct (x, y) VALUES (1, 2)", []).unwrap();
    // Deleting the referenced composite parent must fail.
    let err = db.execute("DELETE FROM pt", []);
    assert!(err.is_err(), "referenced composite parent delete must fail");
}

#[test]
fn fk_implicit_parent_pk_reference() {
    // `REFERENCES parent` without columns → parent's PRIMARY KEY.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
    db.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent)",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO parent (id, name) VALUES (5, 'e')", []).unwrap();
    db.execute("INSERT INTO child (pid) VALUES (5)", []).unwrap();
    let err = db.execute("INSERT INTO child (pid) VALUES (6)", []);
    assert!(err.is_err(), "implicit-PK reference must be enforced");
}

#[test]
fn fk_cascade_is_recursive() {
    // grandparent -> parent -> child: deleting the grandparent cascades
    // through both levels.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE gp (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, gpid INTEGER REFERENCES gp(id) ON DELETE CASCADE)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE)",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO gp (id) VALUES (1)", []).unwrap();
    db.execute("INSERT INTO p (id, gpid) VALUES (10, 1)", []).unwrap();
    db.execute("INSERT INTO c (id, pid) VALUES (100, 10)", []).unwrap();
    db.execute("DELETE FROM gp WHERE id = 1", []).unwrap();
    assert_eq!(db.query("SELECT COUNT(*) FROM p", []).unwrap()[0][0], Value::Integer(0));
    assert_eq!(db.query("SELECT COUNT(*) FROM c", []).unwrap()[0][0], Value::Integer(0));
}

#[test]
fn fk_text_parent_key() {
    // Non-integer parent key exercises the scan-based parent lookup.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE users (email TEXT PRIMARY KEY, name TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, email TEXT REFERENCES users(email))",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO users (email, name) VALUES ('a@x', 'Ann')", []).unwrap();
    db.execute("INSERT INTO orders (email) VALUES ('a@x')", []).unwrap();
    let err = db.execute("INSERT INTO orders (email) VALUES ('nobody@x')", []);
    assert!(err.is_err(), "text FK must be enforced");
    let err = db.execute("DELETE FROM users WHERE email = 'a@x'", []);
    assert!(err.is_err(), "referenced text parent must not delete");
}

#[test]
fn fk_pragma_can_be_toggled_back_off() {
    let mut db = fk_db();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    assert!(db.execute("INSERT INTO child (pid) VALUES (999)", []).is_err());
    db.execute("PRAGMA foreign_keys = OFF", []).unwrap();
    db.execute("INSERT INTO child (pid) VALUES (999)", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM child", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
}

#[test]
fn fk_delete_no_pk_table_with_fks() {
    // DELETE on a table without INTEGER PRIMARY KEY (the streaming delete
    // path) must still enforce parent-side FKs.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE nopk (pid INTEGER REFERENCES parent(id), tag TEXT)",
        [],
    )
    .unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO parent (id) VALUES (1)", []).unwrap();
    db.execute("INSERT INTO nopk VALUES (1, 'x')", []).unwrap();
    let err = db.execute("DELETE FROM parent WHERE id = 1", []);
    assert!(err.is_err(), "no-PK child table must protect the parent");
    db.execute("DELETE FROM nopk WHERE tag = 'x'", []).unwrap();
    db.execute("DELETE FROM parent WHERE id = 1", []).unwrap();
    assert_eq!(db.query("SELECT COUNT(*) FROM parent", []).unwrap()[0][0], Value::Integer(0));
}

#[test]
fn fk_index_maintenance_on_cascade_delete() {
    // Cascaded child deletions must also remove their index entries.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY)", []).unwrap();
    db.execute(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id) ON DELETE CASCADE, tag TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_tag ON c(tag)", []).unwrap();
    db.execute("PRAGMA foreign_keys = ON", []).unwrap();
    db.execute("INSERT INTO p (id) VALUES (1), (2)", []).unwrap();
    db.execute("INSERT INTO c (pid, tag) VALUES (1, 'one'), (1, 'uno'), (2, 'two')", []).unwrap();
    db.execute("DELETE FROM p WHERE id = 1", []).unwrap();
    // The index must no longer return the cascaded rows.
    let rows = db.query("SELECT tag FROM c WHERE tag = 'one'", []).unwrap();
    assert!(rows.is_empty(), "index must drop cascaded entries");
    let rows = db.query("SELECT tag FROM c WHERE tag = 'two'", []).unwrap();
    assert_eq!(rows.len(), 1);
}
