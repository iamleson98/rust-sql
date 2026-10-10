//! Differential replay: run a one-statement-per-line SQL script against
//! this engine and the bundled SQLite side by side, printing every
//! statement whose success/failure differs and every query's results.
//! The tool behind triaging stateful-fuzz divergences (paste the failing
//! script, then bisect by deleting lines).
//!
//! ```text
//! cargo run --release --example diff_replay -- script.sql
//! V=1 ...      # print every statement's outcome, not just queries
//! PROBE="SELECT * FROM t ORDER BY rowid;SELECT ..." ...
//!              # run the probes after EVERY statement and stop at the
//!              # first statement after which they diverge
//! ```

use rusqlite::types::Value as Sv;
use rustqlite::{Database, Value};

fn is_queryish(sql: &str) -> bool {
    let head = sql.trim_start().to_ascii_uppercase();
    head.starts_with("SELECT")
        || head.starts_with("WITH")
        || head.starts_with("VALUES")
        || head.starts_with("PRAGMA")
        || head.starts_with("EXPLAIN")
}

fn our_run(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    if is_queryish(sql) {
        db.query(sql, []).map_err(|e| e.to_string())
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    }
}

fn sq_run(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Sv>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| format!("prepare: {}", e))?;
    let ncols = stmt.column_count();
    if ncols == 0 {
        return stmt
            .execute([])
            .map(|_| Vec::new())
            .map_err(|e| format!("execute: {}", e));
    }
    let mut rows = stmt.query([]).map_err(|e| format!("query: {}", e))?;
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
            Err(e) => return Err(format!("query: {}", e)),
        }
    }
    Ok(out)
}

fn norm_ours(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => format!("I{}", i),
        Value::Real(r) => format!("R{:?}", r),
        Value::Text(t) => format!("T{:?}", t.as_str()),
        Value::Blob(b) => format!("B{:?}", b),
    }
}

fn norm_sq(v: &Sv) -> String {
    match v {
        Sv::Null => "NULL".into(),
        Sv::Integer(i) => format!("I{}", i),
        Sv::Real(r) => format!("R{:?}", r),
        Sv::Text(t) => format!("T{:?}", t),
        Sv::Blob(b) => format!("B{:?}", b),
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let verbose = std::env::var("V").is_ok();
    let text = std::fs::read_to_string(path).unwrap();
    let mut ours = Database::open_in_memory().unwrap();
    let sq = rusqlite::Connection::open_in_memory().unwrap();
    for (i, line) in text.lines().enumerate() {
        let sql = line.trim();
        if sql.is_empty() || sql.starts_with("--") {
            continue;
        }
        let a = our_run(&mut ours, sql);
        let b = sq_run(&sq, sql);
        let same_ok = a.is_ok() == b.is_ok();
        let show = verbose || !same_ok || is_queryish(sql);
        if show {
            println!("{:4}: {}", i, sql);
            match &a {
                Ok(r) => println!("   ours  : {:?}", r),
                Err(e) => println!("   ours  : ERR {}", e),
            }
            match &b {
                Ok(r) => println!("   sqlite: {:?}", r),
                Err(e) => println!("   sqlite: ERR {}", e),
            }
            if !same_ok {
                println!("   ^^^ SUCCESS DIVERGENCE");
            }
        }
        if let Ok(probe) = std::env::var("PROBE") {
            for p in probe.split(';') {
                let a = format!(
                    "{:?}",
                    our_run(&mut ours, p)
                        .map(|r| r
                            .iter()
                            .map(|row| row.iter().map(norm_ours).collect::<Vec<_>>())
                            .collect::<Vec<_>>())
                        .map_err(|_| "ERR")
                );
                let b = format!(
                    "{:?}",
                    sq_run(&sq, p)
                        .map(|r| r
                            .iter()
                            .map(|row| row.iter().map(norm_sq).collect::<Vec<_>>())
                            .collect::<Vec<_>>())
                        .map_err(|_| "ERR")
                );
                if a != b {
                    println!(
                        "PROBE DIVERGES after stmt {}: {}\n  probe: {}\n  ours  : {}\n  sqlite: {}",
                        i, sql, p, a, b
                    );
                    return;
                }
            }
        }
    }
}
