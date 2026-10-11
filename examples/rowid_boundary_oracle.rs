//! Focused probe: the rowid vs boundary REAL -9223372036854775808.0
//! plan-dependent class — rowid IN inside OR, a redundant index on the
//! INTEGER PRIMARY KEY column, and DISTINCT / ORDER BY tipping from a
//! covering index back to the rowid seek. Engine vs bundled SQLite.
use rustqlite::Database;

fn fmt(rows: &[Vec<rustqlite::Value>]) -> String {
    let mut out = String::new();
    for r in rows {
        if !out.is_empty() {
            out.push('|');
        }
        for (i, c) in r.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match c {
                rustqlite::Value::Null => out.push('~'),
                rustqlite::Value::Integer(v) => out.push_str(&v.to_string()),
                rustqlite::Value::Real(f) => out.push_str(&format!("{f:?}")),
                rustqlite::Value::Text(t) => out.push_str(
                    &t.as_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                ),
                rustqlite::Value::Blob(b) => {
                    out.push_str(&b.iter().map(|x| format!("{x:02x}")).collect::<String>())
                }
            }
        }
    }
    out
}

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
        format!("SELECT id FROM t WHERE id = {b}"),
        format!("SELECT id FROM t WHERE id = {b} NOT INDEXED"),
        format!("SELECT id FROM t WHERE id IN ({b})"),
        format!("SELECT id FROM t WHERE id IN ({b}, 1)"),
        format!("SELECT id FROM t WHERE id IN (1, {b})"),
        format!("SELECT id FROM t WHERE id IN ({b}, -9223372036854775808)"),
        format!("SELECT id FROM t WHERE id NOT IN ({b})"),
        format!("SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE id IN ({b}, 1))"),
        // rowid IN inside OR whose sibling branch has no index
        format!("SELECT id FROM t WHERE id IN ({b}, 1) OR v = 'z'"),
        format!("SELECT id FROM t WHERE v = 'z' OR id IN ({b}, 1)"),
        format!("SELECT id FROM t WHERE id IN ({b}) OR v LIKE '%z'"),
        // redundant index on the IPK column
        format!("SELECT id FROM t WHERE id = {b} AND k = 30"),
        format!("SELECT id FROM t WHERE +id = {b}"),
        format!("SELECT id FROM t WHERE id > {b} - 1"),
        format!("SELECT id FROM t WHERE id BETWEEN {b} AND 1"),
        format!("SELECT count(*) FROM t WHERE id = {b}"),
        format!("SELECT max(id) FROM t WHERE id <= {b}"),
        format!("SELECT min(id) FROM t WHERE id >= {b}"),
        // DISTINCT / ORDER BY tipping
        format!("SELECT DISTINCT k FROM t WHERE id <= {b} + 1 ORDER BY k"),
        format!("SELECT DISTINCT v FROM t WHERE id IN ({b}, 1, 5) ORDER BY v"),
        format!("SELECT k, count(*) FROM t WHERE id IN ({b}, 1) GROUP BY k ORDER BY k"),
        format!("SELECT v FROM t WHERE id IN ({b}, 1) ORDER BY v"),
        format!("SELECT v FROM t WHERE id IN ({b}, 1) ORDER BY v DESC LIMIT 2"),
        // mixed affinity against the boundary
        format!("SELECT id FROM t WHERE id = CAST({b} AS REAL)"),
        format!("SELECT id FROM t WHERE id IN (CAST({b} AS REAL), 2)"),
        "SELECT id FROM t WHERE id IN ('-9223372036854775808.0')".to_string(),
        "SELECT id FROM t WHERE id = '-9223372036854775808.0'".to_string(),
        format!("SELECT id FROM t WHERE rowid = {b}"),
        format!("SELECT id FROM t WHERE rowid IN ({b}, 3)"),
        format!("SELECT id FROM t WHERE _rowid_ = {b}"),
        format!("SELECT id FROM t WHERE oid IN ({b}, 3) OR a = 1"),
        // update/delete with the boundary
        format!("SELECT count(*) FROM t WHERE id IN ({b}, 1) AND k = 10"),
        // IN-list on the indexed redundant IPK with ANALYZE present
        format!("SELECT id FROM t WHERE id IN ({b}, 1, 9223372036854775807) ORDER BY id"),
        format!("SELECT id, v FROM t WHERE id IN ({b}, 1) ORDER BY id LIMIT 1"),
        // nested: IN over a subquery that itself holds the boundary
        format!("SELECT id FROM t WHERE id IN (SELECT CASE WHEN 1 THEN {b} ELSE 1 END, 2)"),
    ];
    let mut n_ok = 0;
    let mut n_bad = 0;
    for q in &qs {
        eprintln!("Q: {q}");
        let eng = match db.query(q, []) {
            Ok(rows) => Ok(fmt(&rows)),
            Err(e) => Err(format!("ERR: {e}")),
        };
        let sq: Result<String, String> = (|| {
            let mut stmt = rc.prepare(q).map_err(|e| format!("ERR: {e}"))?;
            let mut out = String::new();
            let mut rows = stmt.query([]).map_err(|e| format!("ERR: {e}"))?;
            loop {
                match rows.next() {
                    Ok(Some(r)) => {
                        if !out.is_empty() {
                            out.push('|');
                        }
                        for i in 0..r.as_ref().column_count() {
                            if i > 0 {
                                out.push(',');
                            }
                            use rusqlite::types::ValueRef;
                            match r.get_ref(i).unwrap() {
                                ValueRef::Null => out.push('~'),
                                ValueRef::Integer(v) => out.push_str(&v.to_string()),
                                ValueRef::Real(v) => out.push_str(&format!("{v:?}")),
                                ValueRef::Text(t) => out.push_str(
                                    &t.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                                ),
                                ValueRef::Blob(t) => out.push_str(
                                    &t.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                                ),
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Err(format!("ERR: {e}")),
                }
            }
            Ok(out)
        })();
        // Both-error pairs compare by CLASS, not raw text: the engine's
        // native Rust Display prefixes ("semantic error: ", "parse error
        // at L:C: ") are its own API convention — the C ABI
        // (sqlite3_errmsg) surfaces the bare SQLite-exact message, which
        // is the compat surface. A parse error on both sides is the same
        // verdict (e.g. `... NOT INDEXED` after a WHERE term is invalid
        // SQL in SQLite too).
        let class = |s: &str| {
            if s.contains("parse error") || s.contains("syntax error") {
                "parse"
            } else if s.contains("sub-select returns") {
                "sub-select arity"
            } else {
                "other"
            }
        };
        match (&eng, &sq) {
            (Ok(a), Ok(b2)) if a == b2 => n_ok += 1,
            (Err(a), Err(b2)) if class(a) == class(b2) => n_ok += 1,
            _ => {
                n_bad += 1;
                println!("DIVERGE: {q}\n  engine={eng:?}\n  sqlite={sq:?}");
            }
        }
    }
    println!("== {n_ok} match, {n_bad} DIVERGE of {} ==", qs.len());
}
