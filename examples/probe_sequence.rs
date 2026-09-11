//! AUTOINCREMENT / sqlite_sequence semantics vs SQLite's pinned contract:
//! high-water after DELETE, explicit-higher bumps, user re-arm via UPDATE,
//! drop removes the row, reopen persistence, validation errors.
use rustqlite::{Database, Value};

fn q(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

fn main() {
    let mut db = Database::open_in_memory().unwrap();

    // 1. High-water after DELETE (never reuse).
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO t VALUES(NULL, 'a'), (NULL, 'b')", [])
        .unwrap();
    db.execute("DELETE FROM t WHERE id = 2", []).unwrap();
    db.execute("INSERT INTO t VALUES(NULL, 'd')", []).unwrap();
    let rows = q(&mut db, "SELECT id FROM t ORDER BY id");
    let seq = q(&mut db, "SELECT name, seq FROM sqlite_sequence");
    println!("ids: {:?} (expect 1,3)", rows);
    println!("seq: {:?} (expect t,3)", seq);

    // 2. Explicit higher rowid bumps the sequence.
    db.execute("INSERT INTO t VALUES(100, 'c')", []).unwrap();
    db.execute("INSERT INTO t VALUES(NULL, 'e')", []).unwrap();
    let seq = q(&mut db, "SELECT seq FROM sqlite_sequence");
    println!("seq after 100: {:?} (expect 101)", seq);

    // 3. User re-arm via UPDATE on sqlite_sequence (SQLite allows).
    db.execute("UPDATE sqlite_sequence SET seq = 1000 WHERE name = 't'", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES(NULL, 'e')", []).unwrap();
    let rows = q(&mut db, "SELECT id FROM t ORDER BY id DESC LIMIT 1");
    println!("after re-arm top id: {:?} (expect 1001)", rows);

    // 4. Second autoincrement table shares sqlite_sequence.
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
        .unwrap();
    db.execute("INSERT INTO u VALUES(NULL)", []).unwrap();
    let seq = q(
        &mut db,
        "SELECT name, seq FROM sqlite_sequence ORDER BY name",
    );
    println!("two seqs: {:?} (expect t:1001, u:1)", seq);

    // 5. sqlite_master row.
    let master = q(
        &mut db,
        "SELECT type, name, rootpage, sql FROM sqlite_master WHERE name = 'sqlite_sequence'",
    );
    println!("master row: {:?} (expect table, sqlite_sequence, 3or4, CREATE TABLE sqlite_sequence(name,seq))", master);

    // 6. DROP removes the sequence row.
    db.execute("DROP TABLE u", []).unwrap();
    let seq = q(&mut db, "SELECT name FROM sqlite_sequence");
    println!("after drop u: {:?} (expect only t)", seq);

    // 7. Validation errors.
    for (ddl, expect) in [
        (
            "CREATE TABLE bad1 (a TEXT PRIMARY KEY AUTOINCREMENT)",
            "INTEGER PRIMARY KEY",
        ),
        (
            "CREATE TABLE bad2 (a INTEGER PRIMARY KEY AUTOINCREMENT) WITHOUT ROWID",
            "WITHOUT ROWID",
        ),
        ("CREATE TABLE sqlite_foo (x)", "reserved"),
    ] {
        match db.execute(ddl, []) {
            Ok(_) => println!("[{ddl}] ACCEPTED (BUG: expected error)"),
            Err(e) => {
                let msg = e.to_string();
                println!("[{ddl}] err contains '{expect}': {}", msg.contains(expect));
            }
        }
    }

    // 8. Reopen persistence: file DB round-trip keeps high-water.
    let path = std::env::temp_dir().join("probe_seq_rustqlite.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY AUTOINCREMENT)", [])
            .unwrap();
        db.execute("INSERT INTO s VALUES(50)", []).unwrap();
        db.execute("DELETE FROM s WHERE id = 50", []).unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("INSERT INTO s VALUES(NULL)", []).unwrap();
        let rows = q(&mut db, "SELECT id FROM s");
        let seq = q(&mut db, "SELECT seq FROM sqlite_sequence");
        println!(
            "reopen insert after delete-50: {:?} (expect 51), seq {:?} (expect 50)",
            rows, seq
        );
    }
    let _ = std::fs::remove_file(&path);

    println!("\n=== SQLite reference (same script) ===");
    let c = rusqlite::Connection::open_in_memory().unwrap();
    let ex = |sql: &str| -> String {
        match c.execute_batch(sql) {
            Ok(_) => "ok".into(),
            Err(e) => format!("ERR {e}"),
        }
    };
    ex(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
        INSERT INTO t VALUES(NULL), (NULL);
        DELETE FROM t WHERE id = 2;
        INSERT INTO t VALUES(NULL);",
    );
    let ids: String = c
        .prepare("SELECT group_concat(id) FROM t")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .flatten()
        .next()
        .unwrap();
    let seq: String = c
        .prepare("SELECT group_concat(name || ':' || seq) FROM sqlite_sequence")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .flatten()
        .next()
        .unwrap();
    println!("sqlite ids: {ids}, seq: {seq}");
}
