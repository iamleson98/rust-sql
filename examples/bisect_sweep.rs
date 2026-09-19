//! Triage bisector for stateful-fuzz failure logs: replays the failure
//! script on BOTH engines (ours + real SQLite) and, after EVERY
//! statement, compares a full ordered dump of every table plus the
//! statement's error/row-count parity — reporting the FIRST statement
//! where the two databases diverge.
//!
//! Usage: `cargo run --example bisect_sweep -- /path/to/failure.log [stop_after]`
use rusqlite::types::Value as Sv;
use rustqlite::{Database, Value};

fn run_ours(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let u = sql.trim_start().to_ascii_uppercase();
    let is_query = u.starts_with("SELECT") || u.starts_with("PRAGMA") || u.starts_with("WITH");
    if is_query {
        match db.query(sql, []) {
            Ok(rows) => Ok(rows.into_iter().collect()),
            Err(e) => Err(format!("{e}")),
        }
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| format!("{e}"))
    }
}

fn run_sq(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Sv>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {e}"))?;
    let ncols = stmt.column_count();
    if ncols == 0 {
        return stmt
            .execute([])
            .map(|n| vec![vec![Sv::Integer(n as i64)]])
            .map_err(|e| format!("execute: {e}"));
    }
    let mut rows = stmt.query([]).map_err(|e| format!("query: {e}"))?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut r = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    r.push(row.get(i).unwrap_or(Sv::Null));
                }
                out.push(r);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("query: {e}")),
        }
    }
    Ok(out)
}

fn values_match(a: &Value, b: &Sv) -> bool {
    match (a, b) {
        (Value::Null, Sv::Null) => true,
        (Value::Null, _) | (_, Sv::Null) => false,
        (Value::Integer(x), Sv::Integer(y)) => x == y,
        (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() <= 1e-9 * y.abs().max(1.0),
        (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        (Value::Real(x), Sv::Real(y)) => {
            (x.is_nan() && y.is_nan())
                || x == y
                || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Text(x), Sv::Text(y)) => x.as_str() == y,
        (Value::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

fn fmt_val(a: &Value) -> String {
    match a {
        Value::Null => "NULL".into(),
        Value::Integer(i) => format!("{i}"),
        Value::Real(f) => format!("{f:?}"),
        Value::Text(t) => format!("'{}'", t.as_str()),
        Value::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|x| format!("{x:02X}")).collect::<String>()
        ),
    }
}

fn fmt_sq(b: &Sv) -> String {
    match b {
        Sv::Null => "NULL".into(),
        Sv::Integer(i) => format!("{i}"),
        Sv::Real(f) => format!("{f:?}"),
        Sv::Text(t) => format!("'{}'", t),
        Sv::Blob(v) => format!(
            "x'{}'",
            v.iter().map(|x| format!("{x:02X}")).collect::<String>()
        ),
    }
}

/// Parse the stateful-fuzz failure-log script format.
fn parse_script(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let body = text
        .split("--- script ---")
        .nth(1)
        .and_then(|s| s.split("--- end script ---").next())
        .unwrap_or("");
    for ln in body.lines() {
        let t = ln.trim_start();
        if t.is_empty() || t.starts_with("note:") {
            continue;
        }
        if let Some(pos) = t.find(": ") {
            let (num, rest) = t.split_at(pos);
            if let Ok(n) = num.trim().parse::<usize>() {
                out.push((n, rest[2..].to_string()));
                continue;
            }
        }
        if let Some(last) = out.last_mut() {
            last.1.push('\n');
            last.1.push_str(t);
        }
    }
    out
}

fn table_list_ours(db: &mut Database) -> Vec<String> {
    db.query(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
        [],
    )
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| r.first().cloned())
    .filter_map(|v| {
        if let Value::Text(t) = v {
            Some(t.as_str().to_string())
        } else {
            None
        }
    })
    .filter(|n| !n.starts_with("sqlite_"))
    .collect()
}

fn table_list_sq(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt =
        match conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let rows: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    };
    rows.into_iter()
        .filter(|n| !n.starts_with("sqlite_"))
        .collect()
}

fn dump_ours(db: &mut Database, t: &str) -> Result<Vec<Vec<Value>>, String> {
    // rowid tables first; WITHOUT ROWID falls back to ordered-by-all-cols.
    let q_rowid = format!("SELECT rowid, * FROM \"{t}\" ORDER BY rowid");
    if let Ok(rows) = db.query(&q_rowid, []) {
        return Ok(rows.into_iter().collect());
    }
    let cols: Vec<String> = db
        .query(&format!("PRAGMA table_info(\"{t}\")"), [])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.get(1).cloned())
        .filter_map(|v| {
            if let Value::Text(s) = v {
                Some(s.as_str().to_string())
            } else {
                None
            }
        })
        .collect();
    let order = if cols.is_empty() {
        "1".to_string()
    } else {
        (1..=cols.len())
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    db.query(&format!("SELECT * FROM \"{t}\" ORDER BY {order}"), [])
        .map(|rows| rows.into_iter().collect())
        .map_err(|e| format!("{e}"))
}

fn dump_sq(conn: &rusqlite::Connection, t: &str) -> Result<Vec<Vec<Sv>>, String> {
    let q_rowid = format!("SELECT rowid, * FROM \"{t}\" ORDER BY rowid");
    if let Ok(mut stmt) = conn.prepare(&q_rowid) {
        let ncols = stmt.column_count();
        if ncols == 0 {
            return Err("no columns".into());
        }
        let mut rows = stmt.query([]).map_err(|e| format!("{e}"))?;
        let mut out = Vec::new();
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    let mut r = Vec::with_capacity(ncols);
                    for i in 0..ncols {
                        r.push(row.get(i).unwrap_or(Sv::Null));
                    }
                    out.push(r);
                }
                Ok(None) => break,
                Err(e) => return Err(format!("{e}")),
            }
        }
        return Ok(out);
    }
    let cols: Vec<String> = {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info(\"{t}\")"))
            .map_err(|e| format!("{e}"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| format!("{e}"))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let order = if cols.is_empty() {
        "1".to_string()
    } else {
        (1..=cols.len())
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM \"{t}\" ORDER BY {order}"))
        .map_err(|e| format!("{e}"))?;
    let ncols = stmt.column_count();
    let mut rows = stmt.query([]).map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut r = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    r.push(row.get(i).unwrap_or(Sv::Null));
                }
                out.push(r);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("{e}")),
        }
    }
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let log_path = args
        .get(1)
        .expect("usage: bisect_sweep <failure.log> [stop_after]");
    let stop_after: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let text = std::fs::read_to_string(log_path).expect("read log");
    let script = parse_script(&text);
    println!("script statements: {}", script.len());

    let mut db = Database::open_in_memory().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();

    for (i, sql) in script.iter() {
        if *i > stop_after {
            break;
        }
        let ours = run_ours(&mut db, sql);
        let theirs = run_sq(&conn, sql);

        // Error parity: both must accept or both must reject.
        match (&ours, &theirs) {
            (Ok(_), Ok(_)) => {}
            (Err(a), Err(b)) => {
                println!("stmt {i}: BOTH ERROR (ok): ours={a:?} sqlite={b:?}");
            }
            (Err(a), Ok(_)) => {
                println!("stmt {i}: DIVERGENCE — OURS ERRORS, SQLITE OK");
                println!("  SQL: {sql}");
                println!("  our error: {a}");
                return;
            }
            (Ok(_), Err(b)) => {
                println!("stmt {i}: DIVERGENCE — SQLITE ERRORS, OURS OK");
                println!("  SQL: {sql}");
                println!("  sqlite error: {b}");
                return;
            }
        }

        // Query-result parity: compare SELECT/PRAGMA result rows.
        let u = sql.trim_start().to_ascii_uppercase();
        let is_query = u.starts_with("SELECT") || u.starts_with("PRAGMA") || u.starts_with("WITH");
        if is_query {
            if let (Ok(a), Ok(b)) = (&ours, &theirs) {
                if a.len() != b.len() {
                    println!(
                        "stmt {i}: QUERY DIVERGENCE — row count {} vs sqlite {}",
                        a.len(),
                        b.len()
                    );
                    println!("  SQL: {sql}");
                    println!(
                        "  ours:   {}",
                        a.iter()
                            .map(|r| r.iter().map(fmt_val).collect::<Vec<_>>().join("|"))
                            .collect::<Vec<_>>()
                            .join(" ; ")
                    );
                    println!(
                        "  sqlite: {}",
                        b.iter()
                            .map(|r| r.iter().map(fmt_sq).collect::<Vec<_>>().join("|"))
                            .collect::<Vec<_>>()
                            .join(" ; ")
                    );
                } else {
                    for (ri, (r1, r2)) in a.iter().zip(b.iter()).enumerate() {
                        for (ci, (v1, v2)) in r1.iter().zip(r2.iter()).enumerate() {
                            if !values_match(v1, v2) {
                                println!("stmt {i}: QUERY DIVERGENCE — row {ri} col {ci}: ours={} sqlite={}", fmt_val(v1), fmt_sq(v2));
                                println!("  SQL: {sql}");
                            }
                        }
                    }
                }
            }
        }

        // Integrity probe: our engine must stay index-consistent after
        // every statement (the oracle is always consistent).
        match db.query("PRAGMA integrity_check", []) {
            Ok(rows) => {
                let mut bad: Vec<String> = Vec::new();
                for r in rows {
                    if let Some(Value::Text(t)) = r.first() {
                        if t.as_str() != "ok" {
                            bad.push(t.as_str().to_string());
                        }
                    }
                }
                if !bad.is_empty() {
                    let n_tab = db
                        .query("SELECT count(*) FROM audit", [])
                        .ok()
                        .and_then(|r| r.first().and_then(|row| row.first().cloned()))
                        .map(|v| if let Value::Integer(n) = v { n } else { -1 })
                        .unwrap_or(-1);
                    let n_ix = db
                        .query("SELECT count(*) FROM audit WHERE note IS NOT NULL", [])
                        .ok()
                        .and_then(|r| r.first().and_then(|row| row.first().cloned()))
                        .map(|v| if let Value::Integer(n) = v { n } else { -1 })
                        .unwrap_or(-1);
                    println!(
                        "stmt {i}: INTEGRITY FAILURES ({}): first={}, last={}",
                        bad.len(),
                        bad.first().unwrap(),
                        bad.last().unwrap()
                    );
                    println!("  SQL: {sql}");
                    println!("  audit rows: {n_tab}; non-null note rows: {n_ix}");
                    let rows_dbg = db
                        .query("SELECT rowid, note FROM audit ORDER BY rowid", [])
                        .unwrap_or_default();
                    println!(
                        "  table rows ({}): {:?}",
                        rows_dbg.len(),
                        rows_dbg
                            .iter()
                            .rev()
                            .take(8)
                            .rev()
                            .map(|r| format!("{:?}", r))
                            .collect::<Vec<_>>()
                    );
                    return;
                }
            }
            Err(e) => println!("stmt {i}: integrity_check error: {e}"),
        }

        // Full state compare after every statement.
        let ours_tables = table_list_ours(&mut db);
        let theirs_tables = table_list_sq(&conn);
        if ours_tables != theirs_tables {
            println!(
                "stmt {i}: DIVERGENCE — TABLE LIST ours={ours_tables:?} sqlite={theirs_tables:?}"
            );
            println!("  SQL: {sql}");
            return;
        }
        for t in &ours_tables {
            let da = dump_ours(&mut db, t);
            let dbb = dump_sq(&conn, t);
            match (&da, &dbb) {
                (Ok(ra), Ok(rb)) => {
                    if ra.len() != rb.len() {
                        println!(
                            "stmt {i}: DIVERGENCE — TABLE {t}: {} rows vs sqlite {} rows",
                            ra.len(),
                            rb.len()
                        );
                        println!("  SQL: {sql}");
                        println!(
                            "  ours:   {}",
                            ra.iter()
                                .map(|r| r.iter().map(fmt_val).collect::<Vec<_>>().join("|"))
                                .collect::<Vec<_>>()
                                .join(" ; ")
                        );
                        println!(
                            "  sqlite: {}",
                            rb.iter()
                                .map(|r| r.iter().map(fmt_sq).collect::<Vec<_>>().join("|"))
                                .collect::<Vec<_>>()
                                .join(" ; ")
                        );
                        return;
                    }
                    for (ri, (r1, r2)) in ra.iter().zip(rb.iter()).enumerate() {
                        if r1.len() != r2.len() {
                            println!(
                                "stmt {i}: DIVERGENCE — TABLE {t} row {ri}: col count {} vs {}",
                                r1.len(),
                                r2.len()
                            );
                            println!("  SQL: {sql}");
                            return;
                        }
                        for (ci, (v1, v2)) in r1.iter().zip(r2.iter()).enumerate() {
                            if !values_match(v1, v2) {
                                println!("stmt {i}: DIVERGENCE — TABLE {t} row {ri} col {ci}: ours={} sqlite={}", fmt_val(v1), fmt_sq(v2));
                                println!("  SQL: {sql}");
                                println!(
                                    "  ours row:   {}",
                                    r1.iter().map(fmt_val).collect::<Vec<_>>().join("|")
                                );
                                println!(
                                    "  sqlite row: {}",
                                    r2.iter().map(fmt_sq).collect::<Vec<_>>().join("|")
                                );
                                return;
                            }
                        }
                    }
                }
                _ => {
                    println!(
                        "stmt {i}: dump error on {t}: ours={:?} sqlite={:?}",
                        da.err(),
                        dbb.err()
                    );
                    println!("  SQL: {sql}");
                }
            }
        }
    }
    println!("NO DIVERGENCE through stmt {stop_after} — replay clean.");
    // Post-mortem: dump every table (both engines) for manual probing.
    let ours_tables = table_list_ours(&mut db);
    for t in &ours_tables {
        match dump_ours(&mut db, t) {
            Ok(rows) => {
                println!("TABLE {t} ({} rows):", rows.len());
                for r in rows {
                    println!("  {}", r.iter().map(fmt_val).collect::<Vec<_>>().join("|"));
                }
            }
            Err(e) => println!("TABLE {t}: dump error {e}"),
        }
    }
}
