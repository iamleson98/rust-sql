//! Dev probe: pin real SQLite's sqlite_stat4 behavior (bundled oracle,
//! compiled with -DSQLITE_ENABLE_STAT4).
use rusqlite::Connection;

fn main() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE t(a INT, b INT, c TEXT);
         CREATE INDEX ia ON t(a);
         CREATE INDEX iab ON t(a, b);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i < 1000)
         INSERT INTO t SELECT i % 10, i % 100, 'x' || (i % 7) FROM s;
         ANALYZE;",
    )
    .unwrap();
    println!("== schema");
    let mut stmt = db
        .prepare("SELECT name, sql FROM sqlite_master WHERE name LIKE 'sqlite_stat%'")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (n, s) in rows {
        println!("  {} => {}", n, s);
    }
    println!("== stat1");
    for r in q(&db, "SELECT tbl, idx, stat FROM sqlite_stat1") {
        println!("  {:?}", r);
    }
    println!("== stat4 counts");
    for r in q(
        &db,
        "SELECT idx, count(*), sum(neq), max(nlt) FROM sqlite_stat4 GROUP BY idx",
    ) {
        println!("  {:?}", r);
    }
    println!("== stat4 shape (first 5 of ia)");
    for r in q(
        &db,
        "SELECT tbl, idx, neq, nlt, ndlt, hex(sample), length(sample) FROM sqlite_stat4 WHERE idx='ia' ORDER BY nlt LIMIT 5",
    ) {
        println!("  {:?}", r);
    }
    println!("== stat4 shape (first 3 of iab)");
    for r in q(
        &db,
        "SELECT tbl, idx, neq, nlt, ndlt, hex(sample), length(sample) FROM sqlite_stat4 WHERE idx='iab' ORDER BY nlt LIMIT 3",
    ) {
        println!("  {:?}", r);
    }
    println!("== plans with stat4 present");
    for sql in [
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 3",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 3 AND b BETWEEN 10 AND 20",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b = 5",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a > 8",
    ] {
        for r in q(&db, sql) {
            println!("  {} :: {:?}", sql, r.last());
        }
    }
    println!("== ANALYZE with no indexes");
    let db2 = Connection::open_in_memory().unwrap();
    db2.execute_batch("CREATE TABLE u(x); INSERT INTO u VALUES (1),(2); ANALYZE;")
        .unwrap();
    for r in q(&db2, "SELECT tbl, idx, stat FROM sqlite_stat1") {
        println!("  {:?}", r);
    }
    for r in q(&db2, "SELECT count(*) FROM sqlite_stat4") {
        println!("  stat4 count: {:?}", r);
    }
    println!("== DELETE FROM t + re-ANALYZE updates stat4");
    db.execute_batch("DELETE FROM t WHERE a < 5; ANALYZE;")
        .unwrap();
    for r in q(&db, "SELECT idx, count(*) FROM sqlite_stat4 GROUP BY idx") {
        println!("  {:?}", r);
    }
    println!("== DROP INDEX cleans stat rows");
    db.execute_batch("DROP INDEX ia;").unwrap();
    for r in q(&db, "SELECT idx FROM sqlite_stat4 GROUP BY idx") {
        println!("  {:?}", r);
    }
    for r in q(&db, "SELECT idx FROM sqlite_stat1") {
        println!("  stat1: {:?}", r);
    }
    println!("==== done");
}

fn q(db: &Connection, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = match db.prepare(sql) {
        Ok(s) => s,
        Err(e) => return vec![vec![format!("<prep err {}>", e)]],
    };
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let mut cells = Vec::new();
        for i in 0..row.as_ref().column_count() {
            cells.push(
                row.get_ref(i)
                    .map(|v| match v {
                        rusqlite::types::ValueRef::Null => "NULL".into(),
                        rusqlite::types::ValueRef::Integer(x) => format!("{}", x),
                        rusqlite::types::ValueRef::Real(x) => format!("{}", x),
                        rusqlite::types::ValueRef::Text(t) => {
                            format!("'{}'", String::from_utf8_lossy(t))
                        }
                        rusqlite::types::ValueRef::Blob(b) => format!("x'{}'", hex(b)),
                    })
                    .unwrap_or_else(|e| format!("<{}>", e)),
            );
        }
        out.push(cells);
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}
