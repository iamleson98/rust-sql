//! Does the engine ever pick covering-index scans for residual-only
//! predicates, like SQLite's `SCAN t USING COVERING INDEX t_a`?
use rustqlite::Database;

fn main() {
    use rusqlite::Connection;
    let rc = Connection::open_in_memory().unwrap();
    let mut db = Database::open_in_memory().unwrap();
    let setup = r#"
CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, k INT, v TEXT);
CREATE INDEX t_a ON t(a);
CREATE INDEX t_k ON t(k);
CREATE INDEX t_kv ON t(k, v);
INSERT INTO t VALUES
 (1, 1, 10, 'x'), (2, 2, 10, 'y'), (3, 1, 20, 'z'),
 (5, 2, 20, 'w');
ANALYZE;
"#;
    for stmt in setup.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            db.execute(stmt, []).unwrap();
        }
    }
    rc.execute_batch(setup).unwrap();

    let qs = [
        "SELECT a FROM t",
        "SELECT count(*) FROM t",
        "SELECT count(*) FROM t WHERE a > 1",
        "SELECT a FROM t WHERE k = 10 OR a = 1",
        "SELECT id FROM t WHERE a = 1 OR k = 10",
        "SELECT a FROM t WHERE v = 'z' OR a = 1",
        "SELECT id FROM t WHERE oid IN (1.0, 3) OR a = 1",
    ];
    for q in qs {
        println!("Q: {q}");
        let plans: Vec<String> = {
            let mut stmt = rc.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut out = Vec::new();
            while let Some(r) = rows.next().unwrap() {
                out.push(r.get::<_, String>(3).unwrap());
            }
            out
        };
        for p in &plans {
            println!("  sqlite : {p}");
        }
        match db.query(&format!("EXPLAIN QUERY PLAN {q}"), []) {
            Ok(rows) => {
                for r in &rows {
                    println!(
                        "  engine : {}",
                        r.iter()
                            .map(|c| format!("{c:?}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                }
            }
            Err(e) => println!("  engine : ERR {e}"),
        }
        println!();
    }
}
