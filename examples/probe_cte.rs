// CTE feature tests: basic, chained, nested WITH, RECURSIVE, shadowing, subquery refs.
use rustqlite::Database;

fn main() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10i64 {
        let s = format!("INSERT INTO t (name, val) VALUES ('n{}', {})", i, i * 10);
        db.execute(&s, []).unwrap();
    }
    db.execute("COMMIT", []).unwrap();

    let check = |db: &Database, sql: &str, expect: &str| match db.query(sql, []) {
        Ok(rows) => {
            let got = format!("{:?}", rows);
            if got == expect {
                println!("[ok] {}", sql);
            } else {
                println!("[FAIL] {}\n  got:    {}\n  expect: {}", sql, got, expect);
            }
        }
        Err(e) => println!("[ERR] {} -> {}", sql, e),
    };

    // 1. Basic CTE
    check(
        &db,
        "WITH big AS (SELECT * FROM t WHERE val > 50) SELECT COUNT(*) FROM big",
        "[[Integer(5)]]",
    );
    // 2. CTE with projection + outer filter
    check(
        &db,
        "WITH v AS (SELECT id, val FROM t) SELECT SUM(val) FROM v WHERE id <= 4",
        "[[Integer(100)]]",
    );
    // 3. Chained CTEs (later sees earlier)
    check(&db, "WITH a AS (SELECT val FROM t WHERE val > 30), b AS (SELECT val * 2 AS v2 FROM a) SELECT SUM(v2) FROM b", "[[Integer(980)]]");
    // 4. Explicit column list
    check(
        &db,
        "WITH c(x) AS (SELECT val FROM t WHERE id = 1) SELECT x FROM c",
        "[[Integer(10)]]",
    );
    // 5. CTE referenced twice
    check(
        &db,
        "WITH d AS (SELECT val FROM t) SELECT (SELECT MAX(val) FROM d) - (SELECT MIN(val) FROM d)",
        "[[Integer(90)]]",
    );
    // 6. CTE + join with a real table
    check(&db, "WITH j AS (SELECT id, val FROM t WHERE val > 70) SELECT COUNT(*) FROM j JOIN t ON j.id = t.id", "[[Integer(3)]]");
    // 7. RECURSIVE counter
    check(&db, "WITH RECURSIVE cnt(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM cnt WHERE x < 10) SELECT SUM(x) FROM cnt", "[[Integer(55)]]");
    // 8. RECURSIVE fibonacci
    check(&db, "WITH RECURSIVE fib(a, b) AS (VALUES(0, 1) UNION ALL SELECT b, a+b FROM fib WHERE b < 100) SELECT MAX(a) FROM fib", "[[Integer(89)]]");
    // 9. RECURSIVE UNION (dedup)
    check(&db, "WITH RECURSIVE r(x) AS (VALUES(1) UNION SELECT x+1 FROM r WHERE x < 5) SELECT COUNT(*) FROM r", "[[Integer(5)]]");
    // 10. Nested WITH inside a CTE body
    check(&db, "WITH outer1 AS (WITH inner1 AS (SELECT val FROM t WHERE val > 80) SELECT COUNT(*) AS c FROM inner1) SELECT c FROM outer1", "[[Integer(2)]]");
    // 11. CTE shadowing a real table name
    check(
        &db,
        "WITH t AS (VALUES(42)) SELECT * FROM t",
        "[[Integer(42)]]",
    );
    // 12. CTE in a subquery
    check(&db, "WITH s AS (SELECT val FROM t WHERE val > 50) SELECT COUNT(*) FROM t WHERE t.val IN (SELECT val FROM s)", "[[Integer(5)]]");
    // 13. Params in CTE
    let r = db
        .query(
            "WITH p AS (SELECT val FROM t WHERE val > ?) SELECT COUNT(*) FROM p",
            [rustqlite::Value::Integer(50)],
        )
        .unwrap();
    println!(
        "[{}] params in CTE: {:?} (expect 5)",
        if r == vec![vec![rustqlite::Value::Integer(5)]] {
            "ok"
        } else {
            "FAIL"
        },
        r
    );
    // 14. RECURSIVE via execute() path
    db.execute("WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM c WHERE x < 5) SELECT SUM(x) FROM c", []).unwrap();
    println!("[ok] recursive via execute()");
    // 15. Repeated execution (no stale cache)
    for _ in 0..3 {
        let r = db
            .query(
                "WITH rr AS (SELECT COUNT(*) AS c FROM t) SELECT c FROM rr",
                [],
            )
            .unwrap();
        assert_eq!(r, vec![vec![rustqlite::Value::Integer(10)]]);
    }
    println!("[ok] repeated WITH executions stable");
    // 16. Materialized hint accepted (parsed, treated as materialized)
    check(
        &db,
        "WITH m AS MATERIALIZED (SELECT val FROM t WHERE id = 2) SELECT val FROM m",
        "[[Integer(20)]]",
    );
    // 17. Recursive traversal of a graph-ish structure
    check(&db, "WITH RECURSIVE down(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM down WHERE x < 100) SELECT COUNT(*) FROM down", "[[Integer(100)]]");
}
