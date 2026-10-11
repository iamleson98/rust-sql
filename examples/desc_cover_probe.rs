//! Check both engines' plans for the failing covering-scan test cases.
use rustqlite::Database;

fn main() {
    let rc = rusqlite::Connection::open_in_memory().unwrap();
    let mut db = Database::open_in_memory().unwrap();
    let setup = r#"
CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT);
CREATE INDEX t_a ON t(a);
CREATE INDEX t_k ON t(k);
CREATE INDEX t_kv ON t(k, v);
CREATE INDEX td ON t(a DESC);
INSERT INTO t VALUES (1, 3, 10, 'x'), (2, 1, 20, 'y'), (3, 3, 10, 'z'),
 (5, 2, 30, 'w'), (7, 1, 20, 'v'), (11, 2, 10, 'u');
ANALYZE;
"#;
    for stmt in setup.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            db.execute(stmt, []).unwrap();
        }
    }
    rc.execute_batch(setup).unwrap();
    for q in [
        "SELECT a FROM t WHERE a > 1 OR k = 10",
        "SELECT a FROM t WHERE k = 10 OR a > 1",
        "SELECT a FROM t WHERE a > 1",
        "SELECT a FROM t WHERE k = 10 OR a = 3",
    ] {
        println!("Q: {q}");
        let mut stmt = rc.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            println!("  sqlite : {}", r.get::<_, String>(3).unwrap());
        }
        for r in &db.query(&format!("EXPLAIN QUERY PLAN {q}"), []).unwrap() {
            println!(
                "  engine : {}",
                r.iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let sq: Vec<String> = {
            let mut stmt = rc.prepare(q).unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut v = Vec::new();
            while let Some(r) = rows.next().unwrap() {
                v.push(format!("{}", r.get::<_, i64>(0).unwrap()));
            }
            v
        };
        let eng: Vec<String> = db
            .query(q, [])
            .unwrap()
            .iter()
            .map(|r| match &r[0] {
                rustqlite::Value::Integer(i) => i.to_string(),
                c => format!("{c:?}"),
            })
            .collect();
        println!(
            "  rows   : sqlite={} engine={}",
            sq.join(","),
            eng.join(",")
        );
    }
    println!("=== DESC index ===");
    let setup2 = r#"
CREATE TABLE d(id INTEGER PRIMARY KEY, a INT, pad TEXT);
CREATE INDEX dd ON d(a DESC);
INSERT INTO d VALUES (1, 3, 'x'), (2, 1, 'y'), (3, 2, 'z'), (4, 3, 'w');
ANALYZE;
"#;
    let mut db2 = Database::open_in_memory().unwrap();
    let rc2 = rusqlite::Connection::open_in_memory().unwrap();
    for stmt in setup2.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            db2.execute(stmt, []).unwrap();
        }
    }
    rc2.execute_batch(setup2).unwrap();
    for q in ["SELECT a FROM d", "SELECT a FROM d ORDER BY a DESC"] {
        println!("Q: {q}");
        let mut stmt = rc2.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            println!("  sqlite : {}", r.get::<_, String>(3).unwrap());
        }
        for r in &db2.query(&format!("EXPLAIN QUERY PLAN {q}"), []).unwrap() {
            println!(
                "  engine : {}",
                r.iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let sq: Vec<String> = {
            let mut stmt = rc2.prepare(q).unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut v = Vec::new();
            while let Some(r) = rows.next().unwrap() {
                v.push(format!("{}", r.get::<_, i64>(0).unwrap()));
            }
            v
        };
        let eng: Vec<String> = db2
            .query(q, [])
            .unwrap()
            .iter()
            .map(|r| match &r[0] {
                rustqlite::Value::Integer(i) => i.to_string(),
                c => format!("{c:?}"),
            })
            .collect();
        println!(
            "  rows   : sqlite={} engine={}",
            sq.join(","),
            eng.join(",")
        );
    }
}
