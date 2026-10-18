//! Database server for the SQLite TCL test harness (`tests/tcl/shim.tcl`):
//! the shim's `sqlite3` command forwards each handle's SQL here over a
//! pipe, and this process runs it on rustqlite (`ours`) or on the bundled
//! SQLite (`sqlite`) — the same test files run against both, so a test
//! that fails only on `ours` is an engine divergence.
//!
//! Protocol (binary, length-prefixed): a request is a line `CMD handle
//! nfields` followed by `nfields` fields, each `len\n<bytes>`. A reply is
//! `OK nfields` or `ERR 1` followed by fields the same way.
//!
//! - `OPEN h [file]`, `CLOSE h`
//! - `PARAMS h [sql]` → the TCL variable names the script binds (sigil
//!   stripped; `@name` comes back as `@name` — bound as a BLOB)
//! - `EVAL h [sql, (name, type, value)*]` → `ncols`, column names, then
//!   typed values (`i` / `r` / `t` / `b` / `n` + payload)
//! - `CHANGES h`, `TOTAL h`, `ROWID h`, `AUTOCOMMIT h`

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};

use rustqlite::{Database, Value};

enum Conn {
    Ours(Box<Database>),
    Sqlite(rusqlite::Connection),
}

fn main() {
    let backend = std::env::args().nth(1).unwrap_or_else(|| "ours".into());
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let mut conns: HashMap<String, Conn> = HashMap::new();
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let (cmd, handle) = (parts[0], parts[1].to_string());
        let n: usize = parts[2].parse().unwrap_or(0);
        let mut fields: Vec<Vec<u8>> = Vec::with_capacity(n);
        for _ in 0..n {
            let mut l = String::new();
            input.read_line(&mut l).unwrap_or(0);
            let len: usize = l.trim().parse().unwrap_or(0);
            let mut buf = vec![0u8; len];
            input.read_exact(&mut buf).unwrap_or(());
            fields.push(buf);
        }
        let text =
            |i: usize| String::from_utf8_lossy(fields.get(i).map_or(&[][..], |v| v)).into_owned();
        // TCL_DB_LOG=<file>: append every request (debugging the harness).
        if let Ok(log) = std::env::var("TCL_DB_LOG") {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
            {
                let _ = writeln!(f, "{cmd} {handle} {}", text(0).replace('\n', " "));
            }
        }
        let reply: Result<Vec<Vec<u8>>, String> = match cmd {
            "OPEN" => {
                let file = text(0);
                let c = if backend == "sqlite" {
                    rusqlite::Connection::open(&file)
                        .map(Conn::Sqlite)
                        .map_err(|e| sq_err(&e))
                } else if file.is_empty() || file == ":memory:" {
                    Database::open_in_memory()
                        .map(|d| Conn::Ours(Box::new(d)))
                        .map_err(|e| e.sqlite_message())
                } else {
                    Database::open(&file)
                        .map(|d| Conn::Ours(Box::new(d)))
                        .map_err(|e| e.sqlite_message())
                };
                c.map(|c| {
                    conns.insert(handle.clone(), c);
                    Vec::new()
                })
            }
            "CLOSE" => {
                conns.remove(&handle);
                Ok(Vec::new())
            }
            "PARAMS" => {
                let sql = text(0);
                let mut names: Vec<String> = Vec::new();
                for st in split(&sql) {
                    for p in scan_params(&st) {
                        if let Some(n) = p.tcl_name() {
                            if !names.contains(&n) {
                                names.push(n);
                            }
                        }
                    }
                }
                Ok(names.into_iter().map(|n| n.into_bytes()).collect())
            }
            "EVAL" => match conns.get_mut(&handle) {
                None => Err(format!("no such database handle: {handle}")),
                Some(c) => {
                    let sql = text(0);
                    let mut vars: HashMap<String, Value> = HashMap::new();
                    let mut i = 1;
                    while i + 2 < fields.len() {
                        let name = text(i);
                        let ty = text(i + 1);
                        let raw = fields.get(i + 2).cloned().unwrap_or_default();
                        let v = match ty.as_str() {
                            "i" => String::from_utf8_lossy(&raw)
                                .parse::<i64>()
                                .map(Value::Integer)
                                .unwrap_or(Value::Null),
                            "r" => String::from_utf8_lossy(&raw)
                                .parse::<f64>()
                                .map(Value::Real)
                                .unwrap_or(Value::Null),
                            "b" => Value::Blob(raw),
                            "n" => Value::Null,
                            _ => Value::Text(String::from_utf8_lossy(&raw).into_owned().into()),
                        };
                        vars.insert(name, v);
                        i += 3;
                    }
                    eval(c, &sql, &vars)
                }
            },
            "CHANGES" | "TOTAL" | "ROWID" | "AUTOCOMMIT" => match conns.get(&handle) {
                None => Err(format!("no such database handle: {handle}")),
                Some(Conn::Ours(d)) => Ok(vec![match cmd {
                    "CHANGES" => d.changes(),
                    "TOTAL" => d.total_changes(),
                    "ROWID" => d.last_insert_rowid(),
                    _ => d.is_autocommit() as i64,
                }
                .to_string()
                .into_bytes()]),
                Some(Conn::Sqlite(s)) => Ok(vec![match cmd {
                    "CHANGES" => s.changes() as i64,
                    "TOTAL" => s.total_changes() as i64,
                    "ROWID" => s.last_insert_rowid(),
                    _ => s.is_autocommit() as i64,
                }
                .to_string()
                .into_bytes()]),
            },
            _ => Err(format!("unknown command {cmd}")),
        };
        match reply {
            Ok(f) => {
                let _ = writeln!(out, "OK {}", f.len());
                for x in f {
                    let _ = writeln!(out, "{}", x.len());
                    let _ = out.write_all(&x);
                }
            }
            Err(e) => {
                let _ = writeln!(out, "ERR 1\n{}", e.len());
                let _ = out.write_all(e.as_bytes());
            }
        }
        let _ = out.flush();
    }
}

fn sq_err(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqliteFailure(f, Some(m)) => {
            let _ = f;
            m.clone()
        }
        rusqlite::Error::SqliteFailure(f, None) => f.to_string(),
        rusqlite::Error::SqlInputError { msg, .. } => msg.clone(),
        other => other.to_string(),
    }
}

/// Statements that produce a result set: queries, PRAGMAs, EXPLAIN, and
/// DML with RETURNING (sqlite3_column_count > 0). Unparseable text runs
/// through `execute`, which reports the error.
/// A DML statement (INSERT / UPDATE / DELETE) carrying a RETURNING
/// clause — those may target views, which need the mutable query path.
fn is_dml_returning(sql: &str) -> bool {
    use rustqlite::sql::ast::Statement as S;
    match rustqlite::sql::parser::parse(sql) {
        Ok(S::Insert(i)) => i.returning.is_some(),
        Ok(S::Update(u)) => u.returning.is_some(),
        Ok(S::Delete(d)) => d.returning.is_some(),
        _ => false,
    }
}

fn returns_rows(sql: &str) -> bool {
    use rustqlite::sql::ast::Statement as S;
    match rustqlite::sql::parser::parse(sql) {
        Ok(S::Select(_)) | Ok(S::Pragma(_)) | Ok(S::Explain { .. }) => true,
        Ok(S::Insert(i)) => i.returning.is_some(),
        Ok(S::Update(u)) => u.returning.is_some(),
        Ok(S::Delete(d)) => d.returning.is_some(),
        _ => false,
    }
}

fn split(sql: &str) -> Vec<String> {
    let out = rustqlite::sql::parser::split_script(sql);
    if let Ok(log) = std::env::var("TCL_DB_SPLIT") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            for (i, s) in out.iter().enumerate() {
                let _ = writeln!(f, "[{i}] {}", s.replace('\n', " "));
            }
        }
    }
    out
}

/// One parameter token of a statement.
struct Param {
    /// As written: `?`, `?3`, `:x`, `@x`, `$x`.
    token: String,
    start: usize,
    end: usize,
}

impl Param {
    /// The TCL variable a named parameter reads (tclsqlite strips the
    /// sigil; `@` binds the value as a BLOB).
    fn tcl_name(&self) -> Option<String> {
        let t = &self.token;
        match t.as_bytes()[0] {
            b'$' | b':' => Some(t[1..].to_string()),
            b'@' => Some(t.clone()),
            _ => None,
        }
    }
}

/// The parameter tokens of one statement, outside literals and comments.
fn scan_params(sql: &str) -> Vec<Param> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                let q = c;
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'?' => {
                let s = i;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Param {
                    token: sql[s..i].to_string(),
                    start: s,
                    end: i,
                });
            }
            b':' | b'@' | b'$' => {
                let s = i;
                i += 1;
                let name_char = |x: u8| x.is_ascii_alphanumeric() || x == b'_' || x >= 0x80;
                loop {
                    if i < b.len() && name_char(b[i]) {
                        i += 1;
                    } else if c == b'$' && b.get(i) == Some(&b':') && b.get(i + 1) == Some(&b':') {
                        i += 2;
                    } else {
                        break;
                    }
                }
                // TCL array element: `$a(key)`.
                if c == b'$' && b.get(i) == Some(&b'(') {
                    while i < b.len() && b[i] != b')' {
                        i += 1;
                    }
                    i += 1;
                }
                if i - s > 1 {
                    out.push(Param {
                        token: sql[s..i.min(b.len())].to_string(),
                        start: s,
                        end: i.min(b.len()),
                    });
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Rewrite every parameter to `?N` with SQLite's numbering (`?` takes the
/// next index, `?N` is N, a name keeps its first index) and return the
/// values by index.
fn bind(sql: &str, vars: &HashMap<String, Value>) -> (String, Vec<Value>) {
    let params = scan_params(sql);
    if params.is_empty() {
        return (sql.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(sql.len());
    let mut values: Vec<Value> = Vec::new();
    let mut named: HashMap<String, usize> = HashMap::new();
    let mut last = 0;
    let mut max = 0usize;
    for p in &params {
        out.push_str(&sql[last..p.start]);
        let idx = if p.token == "?" {
            max + 1
        } else if let Some(n) = p.token.strip_prefix('?') {
            n.parse().unwrap_or(max + 1)
        } else if let Some(&i) = named.get(&p.token) {
            i
        } else {
            let i = max + 1;
            named.insert(p.token.clone(), i);
            i
        };
        max = max.max(idx);
        if values.len() < idx {
            values.resize(idx, Value::Null);
        }
        if let Some(n) = p.tcl_name() {
            values[idx - 1] = vars.get(&n).cloned().unwrap_or(Value::Null);
        }
        out.push_str(&format!("?{idx}"));
        last = p.end;
    }
    out.push_str(&sql[last..]);
    (out, values)
}

fn eval(c: &mut Conn, script: &str, vars: &HashMap<String, Value>) -> Result<Vec<Vec<u8>>, String> {
    let mut cols: Vec<String> = Vec::new();
    let mut vals: Vec<Value> = Vec::new();
    for st in split(script) {
        let (sql, params) = bind(&st, vars);
        match c {
            Conn::Ours(db) => {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // DML with RETURNING may target a VIEW (INSTEAD OF
                    // triggers fire — a write) while still producing
                    // rows: the mutable entry routes both cases.
                    if is_dml_returning(&sql) {
                        db.query_dml_returning_mut(&sql, params.clone())
                    } else if returns_rows(&sql) {
                        match db.query_with_columns(&sql, params.clone()) {
                            // PRAGMA assignments and the like run through
                            // execute (no result set here).
                            Err(rustqlite::Error::Unsupported(_)) => db
                                .execute(&sql, params.clone())
                                .map(|_| (Vec::new(), Vec::new())),
                            other => other,
                        }
                    } else {
                        db.execute(&sql, params.clone())
                            .map(|_| (Vec::new(), Vec::new()))
                    }
                }));
                match r {
                    Ok(Ok((c2, rows))) => {
                        if !c2.is_empty() {
                            cols = c2;
                        }
                        for r in rows {
                            vals.extend(r);
                        }
                    }
                    Ok(Err(e)) => return Err(e.sqlite_message()),
                    Err(_) => return Err("PANIC".into()),
                }
            }
            Conn::Sqlite(s) => {
                let mut stmt = s.prepare(&sql).map_err(|e| sq_err(&e))?;
                for (i, v) in params.iter().enumerate() {
                    let r = match v {
                        Value::Null => stmt.raw_bind_parameter(i + 1, rusqlite::types::Null),
                        Value::Integer(x) => stmt.raw_bind_parameter(i + 1, *x),
                        Value::Real(x) => stmt.raw_bind_parameter(i + 1, *x),
                        Value::Text(t) => stmt.raw_bind_parameter(i + 1, t.as_str()),
                        Value::Blob(b) => stmt.raw_bind_parameter(i + 1, b.as_slice()),
                    };
                    r.map_err(|e| sq_err(&e))?;
                }
                let n = stmt.column_count();
                if n > 0 {
                    cols = stmt.column_names().iter().map(|s| s.to_string()).collect();
                }
                let mut rows = stmt.raw_query();
                loop {
                    match rows.next() {
                        Ok(Some(r)) => {
                            for i in 0..n {
                                use rusqlite::types::ValueRef;
                                vals.push(match r.get_ref(i).map_err(|e| sq_err(&e))? {
                                    ValueRef::Null => Value::Null,
                                    ValueRef::Integer(x) => Value::Integer(x),
                                    ValueRef::Real(x) => Value::Real(x),
                                    ValueRef::Text(t) => {
                                        Value::Text(String::from_utf8_lossy(t).into_owned().into())
                                    }
                                    ValueRef::Blob(b) => Value::Blob(b.to_vec()),
                                });
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(sq_err(&e)),
                    }
                }
            }
        }
    }
    let mut f: Vec<Vec<u8>> = Vec::with_capacity(1 + cols.len() + 2 * vals.len());
    f.push(cols.len().to_string().into_bytes());
    for c in &cols {
        f.push(c.as_bytes().to_vec());
    }
    for v in vals {
        match v {
            Value::Null => {
                f.push(b"n".to_vec());
                f.push(Vec::new());
            }
            Value::Integer(i) => {
                f.push(b"i".to_vec());
                f.push(i.to_string().into_bytes());
            }
            Value::Real(r) => {
                f.push(b"r".to_vec());
                f.push(format!("{r:e}").into_bytes());
            }
            Value::Text(t) => {
                f.push(b"t".to_vec());
                f.push(t.as_bytes().to_vec());
            }
            Value::Blob(b) => {
                f.push(b"b".to_vec());
                f.push(b);
            }
        }
    }
    Ok(f)
}
