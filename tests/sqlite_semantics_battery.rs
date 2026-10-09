//! SQLite-semantics regression battery: every `.sql` fixture under
//! `tests/fixtures/semantics/` runs statement by statement on rustqlite
//! AND on the bundled real SQLite, and every statement must agree:
//!
//! * same success/failure (error TEXT is not compared — only the class);
//! * same result rows, compared STRICTLY by storage class and value
//!   (REAL bit-exact modulo the sign of zero), as a multiset — or as an
//!   exact sequence when the statement is prefixed with `/*ordered*/`.
//!
//! The fixtures pin the divergence classes found by the strict
//! differential fuzzer (tests/strict_differential_fuzz.rs): numeric text
//! conversion and BLOB truthiness, arithmetic/bitwise/shift edge cases,
//! comparison affinity in every evaluation context, the func.c scalar
//! library, LIKE/GLOB, positional GROUP BY / ORDER BY, row-dependent
//! access-path bounds, NULL keys in index ranges and index joins.
//!
//! Add a statement here whenever a divergence is fixed.

use rusqlite::types::ValueRef;
use rustqlite::{Database, Value};

#[derive(Clone, PartialEq, Debug)]
enum V {
    Null,
    Int(i64),
    Real(u64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

fn bits(r: f64) -> u64 {
    if r == 0.0 {
        0
    } else {
        r.to_bits()
    }
}

fn ours(v: &Value) -> V {
    match v {
        Value::Null => V::Null,
        Value::Integer(i) => V::Int(*i),
        Value::Real(r) => V::Real(bits(*r)),
        Value::Text(t) => V::Text(t.as_bytes().to_vec()),
        Value::Blob(b) => V::Blob(b.clone()),
    }
}

fn theirs(v: ValueRef<'_>) -> V {
    match v {
        ValueRef::Null => V::Null,
        ValueRef::Integer(i) => V::Int(i),
        ValueRef::Real(r) => V::Real(bits(r)),
        ValueRef::Text(t) => V::Text(t.to_vec()),
        ValueRef::Blob(b) => V::Blob(b.to_vec()),
    }
}

fn sort_key(row: &[V]) -> String {
    format!("{row:?}")
}

/// Row-returning iff SQLite's prepared statement has result columns (DML
/// with RETURNING included); keyword sniffing only when it won't prepare.
fn returns_rows(conn: &rusqlite::Connection, sql: &str) -> bool {
    match conn.prepare(sql) {
        Ok(st) => st.column_count() > 0,
        Err(_) => {
            let up = sql.trim_start().to_ascii_uppercase();
            ["SELECT", "WITH", "VALUES", "PRAGMA", "EXPLAIN"]
                .iter()
                .any(|k| up.starts_with(k))
        }
    }
}

fn run_ours(db: &mut Database, sql: &str, is_query: bool) -> Result<Vec<Vec<V>>, String> {
    if is_query {
        db.query(sql, [])
            .map(|rows| rows.iter().map(|r| r.iter().map(ours).collect()).collect())
            .map_err(|e| e.to_string())
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    }
}

fn run_theirs(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<V>>, String> {
    let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
    let n = st.column_count();
    if n == 0 {
        st.raw_execute().map_err(|e| e.to_string())?;
        return Ok(Vec::new());
    }
    let mut rows = st.raw_query();
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        out.push((0..n).map(|i| theirs(r.get_ref_unwrap(i))).collect());
    }
    Ok(out)
}

fn statements(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in script.lines() {
        let t = line.trim();
        if t.starts_with("--") || t.is_empty() {
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
        if t.ends_with(';') {
            out.push(cur.trim().trim_end_matches(';').to_string());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

#[test]
fn semantics_battery_matches_sqlite() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/semantics");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        // SEMANTICS_FIXTURE=<substring> narrows a local run to matching
        // fixture files (CI sets nothing and runs them all).
        .filter(|p| match std::env::var("SEMANTICS_FIXTURE") {
            Ok(want) => p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().contains(&want)),
            Err(_) => true,
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures found in {}", dir.display());
    let mut failures = Vec::new();
    let mut checked = 0usize;
    for f in &files {
        let script = std::fs::read_to_string(f).unwrap();
        let mut db = Database::open_in_memory().unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for stmt in statements(&script) {
            let (ordered, sql) = match stmt.strip_prefix("/*ordered*/") {
                Some(rest) => (true, rest.trim().to_string()),
                None => (false, stmt.clone()),
            };
            let a = run_ours(&mut db, &sql, returns_rows(&conn, &sql));
            let b = run_theirs(&conn, &sql);
            checked += 1;
            let same = match (&a, &b) {
                (Ok(x), Ok(y)) => {
                    if ordered {
                        x == y
                    } else {
                        let mut xs: Vec<_> = x.iter().map(|r| sort_key(r)).collect();
                        let mut ys: Vec<_> = y.iter().map(|r| sort_key(r)).collect();
                        xs.sort();
                        ys.sort();
                        xs == ys
                    }
                }
                (Err(_), Err(_)) => true,
                _ => false,
            };
            if !same {
                failures.push(format!(
                    "{}: {}\n    ours:   {:?}\n    sqlite: {:?}",
                    f.file_name().unwrap().to_string_lossy(),
                    sql,
                    a,
                    b
                ));
            }
        }
    }
    for f in &failures {
        eprintln!("{f}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} statements diverge from SQLite (listed above)",
        failures.len(),
        checked
    );
}
