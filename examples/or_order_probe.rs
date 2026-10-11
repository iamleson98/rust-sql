//! Quick probe: print SQLite's EXPLAIN QUERY PLAN for the OR shapes the
//! boundary oracle flagged, next to the engine's own plan output.
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
CREATE INDEX t_id ON t(id);
INSERT INTO t VALUES
 (1, 1, 10, 'x'),
 (2, 2, 10, 'y'),
 (3, 1, 20, 'z'),
 (-9223372036854775808, 3, 30, 'min'),
 (9223372036854775807, 1, 40, 'max'),
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

    let b = "-9223372036854775808.0";
    let qs: Vec<String> = vec![
        format!("SELECT id FROM t WHERE oid IN ({b}, 3) OR a = 1"),
        format!("SELECT id FROM t WHERE a = 1 OR oid IN ({b}, 3)"),
        format!("SELECT id FROM t WHERE oid IN ({b}, 3) OR k = 10"),
        format!("SELECT id FROM t WHERE oid IN ({b}, 3) OR v = 'z'"),
        format!("SELECT id FROM t WHERE id IN ({b}, 1) OR v = 'z'"),
    ];
    for q in &qs {
        println!("Q: {q}");
        let mut stmt = rc.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
        let rows: Vec<(i64, i64, i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect();
        for (sel, ord, frm, det) in &rows {
            println!("  sqlite-plan [{sel},{ord},{frm}]: {det}");
        }
        drop(stmt);
        match db.query(&format!("EXPLAIN QUERY PLAN {q}"), []) {
            Ok(rows) => {
                for r in &rows {
                    let mut line = String::new();
                    for c in r {
                        if !line.is_empty() {
                            line.push(' ');
                        }
                        line.push_str(&format!("{c:?}"));
                    }
                    println!("  engine-plan: {line}");
                }
            }
            Err(e) => println!("  engine-plan ERR: {e}"),
        }
        let sq: Vec<String> = {
            let mut stmt = rc.prepare(q).unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut out = Vec::new();
            while let Some(r) = rows.next().unwrap() {
                out.push(format!("{}", r.get::<_, i64>(0).unwrap()));
            }
            out
        };
        let eng = db
            .query(q, [])
            .unwrap()
            .iter()
            .map(|r| match &r[0] {
                rustqlite::Value::Integer(i) => i.to_string(),
                c => format!("{c:?}"),
            })
            .collect::<Vec<_>>();
        println!("  sqlite = {}", sq.join("|"));
        println!("  engine = {}", eng.join("|"));
        println!();
    }
}
