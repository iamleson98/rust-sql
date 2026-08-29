// View feature tests.
use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10i64 {
        let s = format!("INSERT INTO t (name, val) VALUES ('n{}', {})", i, i * 10);
        db.execute(&s, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let check = |db: &Database, sql: &str, expect: &str| {
        match db.query(sql, []) {
            Ok(rows) => {
                let got = format!("{:?}", rows);
                if got == expect { println!("[ok] {}", sql); }
                else { println!("[FAIL] {}\n  got:    {}\n  expect: {}", sql, got, expect); }
            }
            Err(e) => println!("[ERR] {} -> {}", sql, e),
        }
    };

    // 1. Basic view
    db.execute("CREATE VIEW big AS SELECT * FROM t WHERE val > 50", []).unwrap();
    check(&db, "SELECT COUNT(*) FROM big", "[[Integer(5)]]");
    // 2. View with projection
    check(&db, "SELECT name FROM big WHERE val = 70", "[[Text(\"n7\")]]");
    // 3. View joined with a table
    check(&db, "SELECT COUNT(*) FROM big JOIN t ON big.id = t.id", "[[Integer(5)]]");
    // 4. View over view
    db.execute("CREATE VIEW bigger AS SELECT * FROM big WHERE val > 80", []).unwrap();
    check(&db, "SELECT COUNT(*) FROM bigger", "[[Integer(2)]]");
    // 5. View with column list
    db.execute("CREATE VIEW renamed(a, b) AS SELECT name, val FROM t WHERE id <= 3", []).unwrap();
    check(&db, "SELECT a, b FROM renamed WHERE b = 20", "[[Text(\"n2\"), Integer(20)]]");
    // 6. Aggregate view
    db.execute("CREATE VIEW stats AS SELECT COUNT(*) AS c, SUM(val) AS s FROM t", []).unwrap();
    check(&db, "SELECT c, s FROM stats", "[[Integer(10), Integer(550)]]");
    // 7. View + GROUP BY
    db.execute("CREATE VIEW byten AS SELECT val / 10 AS bucket, COUNT(*) AS c FROM t GROUP BY bucket", []).unwrap();
    check(&db, "SELECT SUM(c) FROM byten", "[[Integer(10)]]");
    // 8. UPDATE through view errors
    match db.execute("UPDATE big SET val = 1 WHERE id = 1", []) {
        Ok(_) => println!("[FAIL] update through view should error"),
        Err(e) => println!("[ok] update view rejected: {}", e),
    }
    // 9. DROP VIEW
    db.execute("DROP VIEW bigger", []).unwrap();
    match db.query("SELECT * FROM bigger", []) {
        Ok(_) => println!("[FAIL] dropped view still queryable"),
        Err(_) => println!("[ok] dropped view gone"),
    }
    // 10. View alias
    check(&db, "SELECT COUNT(*) FROM big b WHERE b.val > 90", "[[Integer(1)]]");
    // 11. CREATE VIEW IF NOT EXISTS
    db.execute("CREATE VIEW IF NOT EXISTS big AS SELECT 1", []).unwrap();
    check(&db, "SELECT COUNT(*) FROM big", "[[Integer(5)]]");
    // 12. View survives reopen (schema row persisted)
    drop(db);
    // (in-memory: reopen not possible — test via file db)
    let path = std::env::temp_dir().join("view_test_rsql.db");
    let _ = std::fs::remove_file(&path);
    let mut db2 = Database::open(&path).unwrap();
    db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
    db2.execute("INSERT INTO t (x) VALUES (1), (2)", []).unwrap();
    db2.execute("CREATE VIEW vx AS SELECT x FROM t", []).unwrap();
    drop(db2);
    let db3 = Database::open(&path).unwrap();
    check(&db3, "SELECT SUM(x) FROM vx", "[[Integer(3)]]");
    let _ = std::fs::remove_file(&path);
}
