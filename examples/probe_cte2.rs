// Debug the remaining CTE issues.
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

    // Issue 1: chained CTE with expression projection
    let r = db.query("WITH a AS (SELECT val FROM t WHERE val > 30), b AS (SELECT val * 2 AS v2 FROM a) SELECT SUM(v2) FROM b", []).unwrap();
    println!("chained: {:?} (expect 280 = (40+50+...+100)*2 = 40*2+50*2+...+100*2 where val>30: 40,50,60,70,80,90,100 -> 560)", r);

    let r = db.query("WITH a AS (SELECT val FROM t WHERE val > 30) SELECT SUM(val) FROM a", []).unwrap();
    println!("simple over a: {:?} (expect 490)", r);

    let r = db.query("WITH a AS (SELECT val FROM t WHERE val > 30), b AS (SELECT val FROM a) SELECT SUM(val) FROM b", []).unwrap();
    println!("passthrough b: {:?} (expect 490)", r);

    let r = db.query("WITH a AS (SELECT val FROM t WHERE val > 30), b AS (SELECT val * 2 AS v2 FROM a) SELECT SUM(v2) FROM b", []).unwrap();
    println!("expr b: {:?} (expect 980)", r);

    // Issue 2: scalar subquery in arithmetic WITHOUT CTE (pre-existing?)
    let r = db.query("SELECT (SELECT MAX(val) FROM t) - (SELECT MIN(val) FROM t)", []).unwrap();
    println!("scalar arith no cte: {:?} (expect 90) err={:?}", r, db.query("SELECT (SELECT MAX(val) FROM t) - (SELECT MIN(val) FROM t)", []).is_err());
    match db.query("SELECT (SELECT MAX(val) FROM t) - (SELECT MIN(val) FROM t)", []) {
        Ok(v) => println!("  -> {:?}", v),
        Err(e) => println!("  -> ERR {}", e),
    }

    // Issue 3: nested WITH
    match db.query("WITH outer1 AS (WITH inner1 AS (SELECT val FROM t WHERE val > 80) SELECT COUNT(*) AS c FROM inner1) SELECT c FROM outer1", []) {
        Ok(v) => println!("nested: {:?} (expect 2)", v),
        Err(e) => println!("nested ERR: {}", e),
    }
    // Simplify: does a bare nested WITH work?
    match db.query("WITH inner1 AS (SELECT val FROM t WHERE val > 80) SELECT COUNT(*) FROM inner1", []) {
        Ok(v) => println!("bare inner: {:?} (expect 2)", v),
        Err(e) => println!("bare inner ERR: {}", e),
    }

    // Issue 4: CTE referenced in IN-subquery
    match db.query("WITH s AS (SELECT val FROM t WHERE val > 50) SELECT COUNT(*) FROM t WHERE t.val IN (SELECT val FROM s)", []) {
        Ok(v) => println!("in-subquery: {:?} (expect 5)", v),
        Err(e) => println!("in-subquery ERR: {}", e),
    }
    // Same without CTE:
    match db.query("SELECT COUNT(*) FROM t WHERE t.val IN (SELECT val FROM t WHERE val > 50)", []) {
        Ok(v) => println!("in-subquery no cte: {:?} (expect 5)", v),
        Err(e) => println!("in-subquery no cte ERR: {}", e),
    }
}
