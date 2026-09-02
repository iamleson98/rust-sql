use rustqlite::Database;
fn main() {
    let mut db = Database::open_in_memory().unwrap();
    // 1. UPDATE ... FROM
    println!("--- UPDATE FROM ---");
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("CREATE TABLE src (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1,'old'),(2,'keep')", [])
        .unwrap();
    db.execute("INSERT INTO src VALUES (1,'new')", []).unwrap();
    match db.execute("UPDATE t SET v = src.v FROM src WHERE t.id = src.id", []) {
        Ok(_) => println!(
            "UPDATE FROM: OK, v={:?}",
            db.query("SELECT v FROM t ORDER BY id", []).unwrap()
        ),
        Err(e) => println!("UPDATE FROM: FAIL {}", e),
    }
    // 2. NOCASE in index + comparison
    println!("--- NOCASE ---");
    match db.execute("CREATE INDEX idx_nc ON t(v COLLATE NOCASE)", []) {
        Ok(_) => println!("NOCASE index: OK"),
        Err(e) => println!("NOCASE index: FAIL {}", e),
    }
    match db.query("SELECT id FROM t WHERE v = 'OLD' COLLATE NOCASE", []) {
        Ok(r) => println!("NOCASE compare: {:?}", r),
        Err(e) => println!("NOCASE compare: FAIL {}", e),
    }
    match db.query("SELECT id FROM t WHERE v = 'OLD'", []) {
        Ok(r) => println!("plain compare: {:?}", r),
        Err(e) => println!("plain compare: FAIL {}", e),
    }
    // 3. NOCASE in ORDER BY
    match db.query("SELECT v FROM t ORDER BY v COLLATE NOCASE", []) {
        Ok(_) => println!("NOCASE order: OK"),
        Err(e) => println!("NOCASE order: FAIL {}", e),
    }
    // 4. UNIQUE + NOCASE conflict
    println!("--- UNIQUE NOCASE ---");
    db.execute("CREATE TABLE u (name TEXT UNIQUE COLLATE NOCASE)", [])
        .unwrap();
    db.execute("INSERT INTO u VALUES ('Alice')", []).unwrap();
    match db.execute("INSERT INTO u VALUES ('ALICE')", []) {
        Ok(_) => println!("UNIQUE NOCASE: ACCEPTED (BUG - should conflict)"),
        Err(e) => println!(
            "UNIQUE NOCASE: conflict raised ({:?})",
            e.to_string().chars().take(80).collect::<String>()
        ),
    }
    // 5. sqlite_master via SQL
    println!("--- sqlite_master ---");
    match db.query("SELECT name, type FROM sqlite_master ORDER BY name", []) {
        Ok(r) => println!(
            "sqlite_master: {:?}",
            r.iter().map(|row| format!("{:?}", row)).collect::<Vec<_>>()
        ),
        Err(e) => println!("sqlite_master: FAIL {}", e),
    }
    match db.query("SELECT type FROM sqlite_temp_master", []) {
        Ok(_) => println!("sqlite_temp_master: OK"),
        Err(e) => println!("sqlite_temp_master: FAIL {}", e),
    }
    // 6. PRAGMA table_info
    println!("--- PRAGMA ---");
    match db.query("PRAGMA table_info(t)", []) {
        Ok(r) => println!("table_info: {} rows", r.len()),
        Err(e) => println!("table_info: FAIL {}", e),
    }
    match db.query("PRAGMA index_list(t)", []) {
        Ok(_) => println!("index_list: OK"),
        Err(e) => println!("index_list: FAIL {}", e),
    }
    match db.query("PRAGMA foreign_key_list(u)", []) {
        Ok(_) => println!("foreign_key_list: OK"),
        Err(e) => println!("foreign_key_list: FAIL {}", e),
    }
    // 7. DELETE RETURNING
    println!("--- RETURNING ---");
    match db.execute("DELETE FROM t WHERE id = 2 RETURNING v", []) {
        Ok(_) => println!("DELETE RETURNING: OK"),
        Err(e) => println!("DELETE RETURNING: FAIL {}", e),
    }
    // 8. Subquery in FROM (derived table)
    println!("--- derived table ---");
    match db.query("SELECT x FROM (SELECT 1 AS x UNION SELECT 2)", []) {
        Ok(r) => println!("derived: {:?}", r),
        Err(e) => println!("derived: FAIL {}", e),
    }
    // 9. CTE
    match db.query("WITH c AS (SELECT 1 AS x) SELECT x FROM c", []) {
        Ok(r) => println!("CTE: {:?}", r),
        Err(e) => println!("CTE: FAIL {}", e),
    }
    // 10. recursive CTE
    match db.query("WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x<5) SELECT x FROM cnt", []) {
        Ok(r) => println!("recursive CTE: {:?}", r),
        Err(e) => println!("recursive CTE: FAIL {}", e),
    }
    // 11. window functions
    println!("--- window ---");
    match db.query("SELECT v, ROW_NUMBER() OVER (ORDER BY v) FROM t", []) {
        Ok(_) => println!("ROW_NUMBER OVER: OK"),
        Err(e) => println!("ROW_NUMBER OVER: FAIL {}", e),
    }
    // 12. PRAGMA journal_mode returns row
    match db.query("PRAGMA journal_mode=WAL", []) {
        Ok(r) => println!("journal_mode set: {:?}", r),
        Err(e) => println!("journal_mode set: FAIL {}", e),
    }
    // 13. last_insert_rowid
    let _ = db.execute("CREATE TABLE lr (id INTEGER PRIMARY KEY, x INT)", []);
    let _ = db.execute("INSERT INTO lr VALUES (NULL, 5)", []);
    println!("last_rowid: {:?}", db.last_insert_rowid());
    // 14. view support
    println!("--- views ---");
    match db.execute("CREATE VIEW vv AS SELECT id FROM t", []) {
        Ok(_) => match db.query("SELECT * FROM vv", []) {
            Ok(_) => println!("CREATE VIEW + query: OK"),
            Err(e) => println!("view query: FAIL {}", e),
        },
        Err(e) => println!("CREATE VIEW: FAIL {}", e),
    }
    // 15. trigger support
    println!("--- triggers ---");
    match db.execute(
        "CREATE TRIGGER tg AFTER INSERT ON t BEGIN UPDATE u SET name = name; END",
        [],
    ) {
        Ok(_) => println!("CREATE TRIGGER: OK"),
        Err(e) => println!("CREATE TRIGGER: FAIL {}", e),
    }
}
