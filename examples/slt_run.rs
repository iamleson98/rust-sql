//! sqllogictest runner (https://sqlite.org/sqllogictest): runs `.test`
//! scripts against this engine and checks every result against the
//! expected output the corpus recorded from SQLite. Formatting, sorting
//! and hashing follow the reference driver (sqllogictest.c +
//! slt_sqlite.c) exactly.
//!
//! ```text
//! cargo run --release --example slt_run -- FILE_OR_DIR...
//! V=1 ...      # print every failure (default: the first 5 per file)
//! ```

use rustqlite::{Database, Value};
use std::panic::AssertUnwindSafe;

fn main() {
    let verbose = std::env::var("V").is_ok();
    let mut files = Vec::new();
    for a in std::env::args().skip(1) {
        collect(std::path::Path::new(&a), &mut files);
    }
    files.sort();
    let (mut total_q, mut total_s, mut total_fail, mut bad_files) =
        (0usize, 0usize, 0usize, 0usize);
    for f in &files {
        let t0 = std::time::Instant::now();
        let r = run_file(f, verbose);
        total_q += r.queries;
        total_s += r.statements;
        total_fail += r.failures;
        if r.failures > 0 {
            bad_files += 1;
        }
        println!(
            "{} {}: {} statements, {} queries, {} failures, {:.1}s",
            if r.failures == 0 { "PASS" } else { "FAIL" },
            f.display(),
            r.statements,
            r.queries,
            r.failures,
            t0.elapsed().as_secs_f64()
        );
    }
    println!(
        "TOTAL: {} files ({} failing), {} statements, {} queries, {} failures",
        files.len(),
        bad_files,
        total_s,
        total_q,
        total_fail
    );
}

fn collect(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if p.is_dir() {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                collect(&e.path(), out);
            }
        }
    } else if p.extension().is_some_and(|e| e == "test") {
        out.push(p.to_path_buf());
    }
}

struct FileResult {
    statements: usize,
    queries: usize,
    failures: usize,
}

fn run_file(path: &std::path::Path, verbose: bool) -> FileResult {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut db = Database::open_in_memory().expect("open");
    let sq = rusqlite::Connection::open_in_memory().expect("open sqlite");
    let mut res = FileResult {
        statements: 0,
        queries: 0,
        failures: 0,
    };
    let mut hash_threshold = 8usize;
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut i = 0usize;
    let fail = |line: usize, msg: String, res: &mut FileResult| {
        res.failures += 1;
        if verbose || res.failures <= 5 {
            println!("  {}:{}: {}", path.display(), line + 1, msg);
        }
    };
    while i < lines.len() {
        // Skip blank lines and comments between records.
        if lines[i].trim().is_empty() || lines[i].starts_with('#') {
            i += 1;
            continue;
        }
        let start = i;
        let mut skip = false;
        // Condition lines.
        loop {
            let toks: Vec<&str> = lines[i].split_whitespace().collect();
            match toks.first().copied() {
                Some("skipif") => {
                    if toks.get(1) == Some(&"sqlite") {
                        skip = true;
                    }
                }
                Some("onlyif") => {
                    if toks.get(1) != Some(&"sqlite") {
                        skip = true;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        let head: Vec<&str> = lines[i].split_whitespace().collect();
        i += 1;
        // The record body runs to the next blank line.
        let body_start = i;
        while i < lines.len() && !lines[i].trim().is_empty() {
            i += 1;
        }
        let body = &lines[body_start..i];
        if skip {
            continue;
        }
        match head.first().copied() {
            Some("statement") => {
                res.statements += 1;
                let sql = body.join("\n");
                let r = guarded(|| db.execute(&sql, []).map(|_| ()).map_err(|e| e.to_string()));
                let sq_ok = sq_statement(&sq, &sql);
                // The reference is today's SQLite where the (older) corpus
                // disagrees with it.
                let want_ok = head.get(1).copied() == Some("ok");
                if r.is_ok() != want_ok && r.is_ok() != sq_ok {
                    match r {
                        Err(e) => {
                            fail(start, format!("statement failed: {e}\n    {sql}"), &mut res)
                        }
                        Ok(()) => fail(
                            start,
                            format!("statement should have failed\n    {sql}"),
                            &mut res,
                        ),
                    }
                }
            }
            Some("query") => {
                res.queries += 1;
                let types: Vec<char> = head.get(1).copied().unwrap_or("").chars().collect();
                let sort = head.get(2).copied().unwrap_or("nosort");
                let label = head.get(3).copied();
                let sep = body.iter().position(|l| *l == "----").unwrap_or(body.len());
                let sql = body[..sep].join("\n");
                let expected: Vec<&str> = body.get(sep + 1..).unwrap_or(&[]).to_vec();
                let ncol = types.len().max(1);
                let sq_rows = sq_query(&sq, &sql);
                let rows = match guarded(|| db.query(&sql, []).map_err(|e| e.to_string())) {
                    Ok(r) => r,
                    Err(e) => {
                        if sq_rows.is_ok() {
                            fail(start, format!("query failed: {e}\n    {sql}"), &mut res);
                        }
                        continue;
                    }
                };
                let sq_vals = sq_rows.ok().map(|rows| {
                    let mut v: Vec<String> = Vec::new();
                    for r in &rows {
                        for (x, t) in r.iter().zip(&types) {
                            v.push(format_value(&sq, x, *t));
                        }
                    }
                    finish(v, sort, ncol)
                });
                if let Some(r) = rows.first() {
                    if r.len() != types.len() {
                        fail(
                            start,
                            format!(
                                "expected {} columns, got {}\n    {sql}",
                                types.len(),
                                r.len()
                            ),
                            &mut res,
                        );
                        continue;
                    }
                }
                let mut vals: Vec<String> = Vec::new();
                for r in &rows {
                    for (v, t) in r.iter().zip(&types) {
                        vals.push(format_value(&sq, v, *t));
                    }
                }
                let vals = finish(vals, sort, ncol);
                let matches_sqlite = sq_vals.as_ref() == Some(&vals);
                let mut ctx = Vec::new();
                for v in &vals {
                    ctx.extend_from_slice(v.as_bytes());
                    ctx.push(b'\n');
                }
                let hash = md5_hex(&ctx);
                // Queries sharing a label must agree.
                if let Some(l) = label {
                    match labels.get(l) {
                        Some(h) if *h != hash => {
                            fail(
                                start,
                                format!("label {l} disagrees with an earlier query\n    {sql}"),
                                &mut res,
                            );
                            continue;
                        }
                        Some(_) => {}
                        None => {
                            labels.insert(l.to_string(), hash.clone());
                        }
                    }
                }
                let got: Vec<String> = if hash_threshold > 0 && vals.len() > hash_threshold {
                    vec![format!("{} values hashing to {}", vals.len(), hash)]
                } else {
                    vals
                };
                if expected.is_empty() {
                    continue; // no recorded result
                }
                if !matches_sqlite
                    && (got.len() != expected.len()
                        || got.iter().zip(&expected).any(|(g, e)| g != e))
                {
                    let show =
                        |v: &[String]| v.iter().take(12).cloned().collect::<Vec<_>>().join(" | ");
                    let exp: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
                    fail(
                        start,
                        format!(
                            "wrong result\n    {sql}\n    expected: {}\n    got:      {}",
                            show(&exp),
                            show(&got)
                        ),
                        &mut res,
                    );
                }
            }
            Some("hash-threshold") => {
                hash_threshold = head.get(1).and_then(|v| v.parse().ok()).unwrap_or(8);
            }
            Some("halt") => break,
            _ => {}
        }
    }
    res
}

fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(p) => Err(format!(
            "PANIC: {}",
            p.downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        )),
    }
}

/// slt_sqlite.c's rendering: NULL; 'T' = sqlite3_column_text ("(empty)"
/// for "", non-printables as '@'); 'I' = sqlite3_column_int ("%d");
/// 'R' = sqlite3_column_double ("%.3f").
fn finish(mut vals: Vec<String>, sort: &str, ncol: usize) -> Vec<String> {
    match sort {
        "rowsort" => {
            let mut chunks: Vec<Vec<String>> = vals.chunks(ncol).map(|c| c.to_vec()).collect();
            chunks.sort();
            vals = chunks.concat();
        }
        "valuesort" => vals.sort(),
        _ => {}
    }
    vals
}

fn sq_statement(sq: &rusqlite::Connection, sql: &str) -> bool {
    sq.execute_batch(sql).is_ok()
}

fn sq_query(sq: &rusqlite::Connection, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    use rusqlite::types::ValueRef;
    let mut st = sq.prepare(sql).map_err(|e| e.to_string())?;
    let n = st.column_count();
    let mut rows = st.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let mut row = Vec::with_capacity(n);
        for i in 0..n {
            row.push(match r.get_ref(i).map_err(|e| e.to_string())? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => Value::Integer(i),
                ValueRef::Real(f) => Value::Real(f),
                ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned().into()),
                ValueRef::Blob(b) => Value::Blob(b.to_vec()),
            });
        }
        out.push(row);
    }
    Ok(out)
}

fn format_value(sq: &rusqlite::Connection, v: &Value, t: char) -> String {
    if matches!(v, Value::Null) {
        return "NULL".into();
    }
    match t {
        'I' => (as_i64(v) as i32).to_string(),
        // sqlite3_snprintf's "%.3f" (16 significant digits at most).
        'R' => sq
            .query_row("SELECT printf('%.3f', ?1)", [as_f64(v)], |r| {
                r.get::<_, String>(0)
            })
            .unwrap_or_default(),
        _ => {
            let s = v.as_text();
            if s.is_empty() {
                return "(empty)".into();
            }
            s.chars()
                .map(|c| if (' '..='~').contains(&c) { c } else { '@' })
                .collect()
        }
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        Value::Real(f) => {
            if f.is_nan() {
                0
            } else if *f <= i64::MIN as f64 {
                i64::MIN
            } else if *f >= i64::MAX as f64 {
                i64::MAX
            } else {
                *f as i64
            }
        }
        Value::Text(_) | Value::Blob(_) => {
            // sqlite3Atoi64 over the text: leading integer prefix.
            let s = v.as_text();
            let s = s.trim_start();
            let mut end = 0;
            let b = s.as_bytes();
            if end < b.len() && (b[end] == b'-' || b[end] == b'+') {
                end += 1;
            }
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            s[..end].parse::<i64>().unwrap_or(0)
        }
        Value::Null => 0,
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Real(f) => *f,
        Value::Text(_) | Value::Blob(_) => {
            let s = v.as_text();
            let s = s.trim_start();
            // Longest numeric prefix.
            let mut best = 0.0;
            for end in (1..=s.len()).rev() {
                if !s.is_char_boundary(end) {
                    continue;
                }
                if let Ok(f) = s[..end].parse::<f64>() {
                    best = f;
                    break;
                }
            }
            best
        }
        Value::Null => 0.0,
    }
}

/// MD5 (RFC 1321), lowercase hex — the reference driver's result hash.
fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect()
}
