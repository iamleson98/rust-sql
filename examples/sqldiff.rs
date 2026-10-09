//! Ad-hoc A/B probe: run a `;`-separated SQL script (argv[1], or stdin)
//! on rustqlite and on the bundled real SQLite, statement by statement,
//! and print both answers side by side (storage class included).
//!
//!     cargo run --example sqldiff -- "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT typeof(a) FROM t"

use rustqlite::{Database, Value};

fn show_ours(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => format!("{i}"),
        Value::Real(r) => format!("{r:?}r"),
        Value::Text(t) => format!("{:?}", t.as_str()),
        Value::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

fn show_sqlite(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef as R;
    match v {
        R::Null => "NULL".into(),
        R::Integer(i) => format!("{i}"),
        R::Real(r) => format!("{r:?}r"),
        R::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => format!("{s:?}"),
            Err(_) => format!("text{t:02x?}"),
        },
        R::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

fn main() {
    let script = match std::env::args().nth(1) {
        Some(s) => s,
        None => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).unwrap();
            s
        }
    };
    let mut ours = Database::open_in_memory().unwrap();
    let theirs = rusqlite::Connection::open_in_memory().unwrap();
    for stmt in script.split(";\n").flat_map(|s| s.split("; ")) {
        let stmt = stmt.trim().trim_end_matches(';');
        if stmt.is_empty() {
            continue;
        }
        let up = stmt.to_ascii_uppercase();
        // Row-returning iff SQLite's prepared statement has columns (a
        // `WITH ... INSERT` is DML); keyword sniffing when it won't prepare.
        let is_query = match theirs.prepare(stmt) {
            Ok(st) => st.column_count() > 0,
            Err(_) => ["SELECT", "WITH", "VALUES", "PRAGMA", "EXPLAIN"]
                .iter()
                .any(|k| up.starts_with(k)),
        };
        let a = if is_query {
            match ours.query_with_columns(stmt, []) {
                Ok((cols, _)) if std::env::var("SQLDIFF_COLS").is_ok() => format!("{cols:?}"),
                Ok((_, rows)) => format!(
                    "{:?}",
                    rows.iter()
                        .map(|r| r.iter().map(show_ours).collect::<Vec<_>>().join(", "))
                        .collect::<Vec<_>>()
                ),
                Err(e) => format!("ERROR {e}"),
            }
        } else {
            match ours.execute(stmt, []) {
                Ok(()) => "ok".into(),
                Err(e) => format!("ERROR {e}"),
            }
        };
        let b = (|| -> Result<String, rusqlite::Error> {
            let mut st = theirs.prepare(stmt)?;
            let n = st.column_count();
            if std::env::var("SQLDIFF_COLS").is_ok() && n > 0 {
                return Ok(format!("{:?}", st.column_names()));
            }
            if n == 0 {
                st.raw_execute()?;
                return Ok("ok".into());
            }
            let mut rows = st.raw_query();
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push(
                    (0..n)
                        .map(|i| show_sqlite(r.get_ref_unwrap(i)))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            Ok(format!("{out:?}"))
        })()
        .unwrap_or_else(|e| format!("ERROR {e}"));
        let mark = if a == b { "  " } else { "!!" };
        println!("{mark} {stmt}\n     ours:   {a}\n     sqlite: {b}");
    }
}
