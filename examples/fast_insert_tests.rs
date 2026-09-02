//! Edge-case verification for the fast INSERT path.

use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();

    // 1. Column permutation + explicit rowid + UTF-8 + quotes + negatives.
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t (val, name, id) VALUES (-2.5, 'it''s «héllo»', 7)",
        [],
    )
    .unwrap();
    let rows = db.query("SELECT id, name, val FROM t", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], rustqlite::Value::Integer(7));
    assert_eq!(
        rows[0][1],
        rustqlite::Value::Text("it's «héllo»".to_string().into())
    );
    assert_eq!(rows[0][2], rustqlite::Value::Real(-2.5));
    println!("[ok] permutation + utf8 + escapes + negative real");

    // 2. Missing columns -> NULLs (no defaults on this table).
    db.execute("INSERT INTO t (name) VALUES ('partial')", [])
        .unwrap();
    let rows = db
        .query("SELECT id, val FROM t WHERE name = 'partial'", [])
        .unwrap();
    assert_eq!(rows[0][0], rustqlite::Value::Integer(8)); // autogen rowid continues after explicit 7 (SQLite semantics)
    assert_eq!(rows[0][1], rustqlite::Value::Null);
    println!("[ok] partial insert with NULL fill + autogen rowid");

    // 3. NOT NULL violation errors identically (via a NOT NULL column).
    db.execute("CREATE TABLE nn (a INTEGER NOT NULL, b TEXT)", [])
        .unwrap();
    let r = db.execute("INSERT INTO nn (b) VALUES ('x')", []);
    assert!(r.is_err(), "NOT NULL violation must error");
    println!(
        "[ok] NOT NULL enforcement: {:?}",
        r.err().map(|e| e.to_string()).unwrap_or_default()
    );

    // 4. UNIQUE index maintenance through fast path.
    db.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO u (email) VALUES ('a@x.com')", [])
        .unwrap();
    let r = db.execute("INSERT INTO u (email) VALUES ('a@x.com')", []);
    assert!(r.is_err(), "UNIQUE violation must error");
    db.execute("INSERT INTO u (email) VALUES ('b@x.com')", [])
        .unwrap();
    let n = db.query("SELECT COUNT(*) FROM u", []).unwrap();
    assert_eq!(n[0][0], rustqlite::Value::Integer(2));
    println!("[ok] UNIQUE index maintenance via fast path");

    // 5. Hex + big ints + booleans + exponent floats.
    db.execute("INSERT INTO t (id, val) VALUES (0x10, 1e3)", [])
        .unwrap();
    let rows = db.query("SELECT id, val FROM t WHERE id = 16", []).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], rustqlite::Value::Real(1000.0));
    println!("[ok] hex literal + exponent float");

    // 6. Multi-row and parameterized inserts still work (slow path).
    db.execute("INSERT INTO t (name) VALUES ('m1'), ('m2')", [])
        .unwrap();
    db.execute(
        "INSERT INTO t (name) VALUES (?)",
        [rustqlite::Value::Text("p1".to_string().into())],
    )
    .unwrap();
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], rustqlite::Value::Integer(6));
    println!("[ok] multi-row + parameterized inserts (slow path)");

    // 7. Case-insensitive table/column/keyword names.
    db.execute("insert into T (NAME) values ('cased')", [])
        .unwrap();
    let n = db
        .query("SELECT COUNT(*) FROM t WHERE name = 'cased'", [])
        .unwrap();
    assert_eq!(n[0][0], rustqlite::Value::Integer(1));
    println!("[ok] case-insensitive identifiers");

    // 8. Transaction + rollback via fast path.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t (name) VALUES ('tx1')", [])
        .unwrap();
    db.execute("ROLLBACK", []).unwrap();
    let n = db
        .query("SELECT COUNT(*) FROM t WHERE name = 'tx1'", [])
        .unwrap();
    assert_eq!(n[0][0], rustqlite::Value::Integer(0));
    println!("[ok] rollback discards fast-path insert");

    // 9. File-backed DB durability: insert, drop, reopen.
    let path = std::env::temp_dir().join("fast_insert_durability_test.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db2 = Database::open(&path).unwrap();
        db2.execute("CREATE TABLE d (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        db2.execute("INSERT INTO d (name) VALUES ('persisted')", [])
            .unwrap();
    }
    {
        let db3 = Database::open(&path).unwrap();
        let rows = db3.query("SELECT name FROM d", []).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0],
            rustqlite::Value::Text("persisted".to_string().into())
        );
    }
    let _ = std::fs::remove_file(&path);
    println!("[ok] file-backed durability across reopen");

    println!("\nALL FAST-INSERT EDGE CASES PASSED");
}
