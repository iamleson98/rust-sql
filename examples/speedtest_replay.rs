//! Replay a speedtest1 `--script` capture (SQLite's own benchmark,
//! test/speedtest1.c) through rustqlite and the bundled SQLite, timing
//! every test section and comparing their results.
//!
//! ```text
//! cc -O2 -DSQLITE_ENABLE_RTREE -I vendor/libsqlite3-sys/sqlite3 \
//!    <sqlite>/test/speedtest1.c vendor/libsqlite3-sys/sqlite3/sqlite3.c -o speedtest1 -lm
//! ./speedtest1 --size 100 --script st.sql scratch.db
//! cargo run --release --example speedtest_replay -- st.sql [WORKDIR] [--only ours|sqlite]
//! ```
//!
//! Each engine runs the whole script on its own fresh database file,
//! one after the other. Every statement is re-parsed from its expanded
//! text on both sides (the script carries bound values inline). A
//! section's result hash is the sorted rows of each statement, so a
//! mismatch is a wrong answer, not an ordering detail.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

struct Section {
    set: usize,
    number: u32,
    name: String,
    stmts: Vec<String>,
    /// `--params`: the lifted literals of each statement (empty = none).
    params: Vec<Vec<Lit>>,
}

fn parse_script(text: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur: Option<Section> = None;
    let mut set = 0usize;
    let mut last_number = 0u32;
    let mut pending = String::new();
    let mut setup = Section {
        set: 0,
        number: 0,
        name: "(setup)".into(),
        stmts: Vec::new(),
        params: Vec::new(),
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("-- begin test ") {
            let (num, name) = rest.split_once(' ').unwrap_or((rest, ""));
            let number: u32 = num.parse().unwrap_or(0);
            if number <= last_number {
                set += 1;
            }
            last_number = number;
            cur = Some(Section {
                set,
                number,
                name: name.to_string(),
                stmts: Vec::new(),
                params: Vec::new(),
            });
            continue;
        }
        if line.starts_with("-- end test ") {
            if let Some(s) = cur.take() {
                out.push(s);
            }
            continue;
        }
        if !pending.is_empty() {
            pending.push('\n');
        }
        pending.push_str(line);
        let c = std::ffi::CString::new(pending.as_str()).unwrap_or_default();
        // sqlite3_complete: a whole statement (quotes, CREATE TRIGGER
        // bodies) has been read.
        if unsafe { rusqlite::ffi::sqlite3_complete(c.as_ptr()) } != 0 {
            let stmt = std::mem::take(&mut pending);
            let t = stmt.trim().trim_end_matches(';').trim();
            if !t.is_empty() {
                match cur.as_mut() {
                    Some(s) => s.stmts.push(t.to_string()),
                    None => setup.stmts.push(t.to_string()),
                }
            }
        }
    }
    out.insert(0, setup);
    out
}

/// A literal lifted out of a statement by `--params`.
#[derive(Clone)]
enum Lit {
    Int(i64),
    Real(f64),
    Text(String),
}

/// `--params`: speedtest1 prepares each statement ONCE and binds values;
/// the script carries the values inline. Lift numeric and string literals
/// of a DML / SELECT statement into `?` parameters so both engines reuse
/// one prepared statement per shape — as speedtest1 itself runs. Literals
/// in ORDER BY / GROUP BY lists stay (they can be column ordinals).
fn parameterize(sql: &str) -> (String, Vec<Lit>) {
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(
        head.as_str(),
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "REPLACE"
    ) {
        return (sql.to_string(), Vec::new());
    }
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut lits = Vec::new();
    let mut i = 0;
    let mut prev_word = String::new();
    let mut in_by = false;
    let mut depth_at_by = 0i32;
    let mut depth = 0i32;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            let mut j = i + 1;
            let mut v = String::new();
            loop {
                if j >= b.len() {
                    return (sql.to_string(), Vec::new());
                }
                if b[j] == b'\'' {
                    if j + 1 < b.len() && b[j + 1] == b'\'' {
                        v.push('\'');
                        j += 2;
                        continue;
                    }
                    break;
                }
                let ch_len = sql[j..].chars().next().map_or(1, |c| c.len_utf8());
                v.push_str(&sql[j..j + ch_len]);
                j += ch_len;
            }
            // x'..' blob literals (and ORDER / GROUP BY terms) stay inline.
            let blob = out.ends_with(['x', 'X'])
                && !out[..out.len() - 1].ends_with(|c: char| c.is_alphanumeric() || c == '_');
            if blob || in_by {
                out.push_str(&sql[i..=j]);
            } else {
                out.push('?');
                lits.push(Lit::Text(v));
            }
            i = j + 1;
            prev_word.clear();
            continue;
        }
        if c == b'"' {
            let j = sql[i + 1..]
                .find('"')
                .map(|k| i + 1 + k)
                .unwrap_or(b.len() - 1);
            out.push_str(&sql[i..=j]);
            i = j + 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let j = (i..b.len())
                .find(|&k| !(b[k].is_ascii_alphanumeric() || b[k] == b'_' || b[k] == b'$'))
                .unwrap_or(b.len());
            let w = sql[i..j].to_ascii_uppercase();
            if w == "BY" && (prev_word == "ORDER" || prev_word == "GROUP") {
                in_by = true;
                depth_at_by = depth;
            } else if in_by
                && matches!(
                    w.as_str(),
                    "LIMIT"
                        | "HAVING"
                        | "WINDOW"
                        | "UNION"
                        | "EXCEPT"
                        | "INTERSECT"
                        | "ON"
                        | "WHERE"
                )
            {
                in_by = false;
            }
            out.push_str(&sql[i..j]);
            prev_word = w;
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
                let mut k = j + 1;
                if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                    k += 1;
                }
                if k < b.len() && b[k].is_ascii_digit() {
                    j = k;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
            }
            let tok = &sql[i..j];
            let ident_tail = j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_');
            let lit = if tok.contains(['.', 'e', 'E']) {
                tok.parse::<f64>().ok().map(Lit::Real)
            } else {
                tok.parse::<i64>().ok().map(Lit::Int)
            };
            match lit {
                Some(l) if !in_by && !ident_tail && !tok.starts_with("0x") => {
                    out.push('?');
                    lits.push(l);
                }
                _ => out.push_str(tok),
            }
            i = j;
            prev_word.clear();
            continue;
        }
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if in_by && depth < depth_at_by {
                in_by = false;
            }
        } else if c == b';' {
            in_by = false;
        }
        if !c.is_ascii_whitespace() {
            prev_word.clear();
        }
        let ch_len = sql[i..].chars().next().map_or(1, |c| c.len_utf8());
        out.push_str(&sql[i..i + ch_len]);
        i += ch_len;
    }
    (out, lits)
}

fn returns_rows(sql: &str) -> bool {
    let head = sql
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        head.as_str(),
        "SELECT" | "VALUES" | "WITH" | "PRAGMA" | "EXPLAIN"
    ) || sql.to_ascii_uppercase().contains(" RETURNING ")
}

#[derive(Default, Clone)]
struct Outcome {
    time: Duration,
    hash: u64,
    errors: usize,
    first_error: Option<String>,
}

fn hash_rows(rows: &mut [Vec<String>], h: &mut std::collections::hash_map::DefaultHasher) {
    rows.sort();
    rows.hash(h);
}

fn ours_value(v: &rustqlite::Value) -> String {
    match v {
        rustqlite::Value::Null => "nil".into(),
        rustqlite::Value::Integer(i) => i.to_string(),
        rustqlite::Value::Real(f) => format!("{f:?}"),
        rustqlite::Value::Text(s) => format!("t:{s}"),
        rustqlite::Value::Blob(b) => format!("b:{b:?}"),
    }
}

fn sqlite_value(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => "nil".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => format!("{f:?}"),
        ValueRef::Text(s) => format!("t:{}", String::from_utf8_lossy(s)),
        ValueRef::Blob(b) => format!("b:{b:?}"),
    }
}

fn run_ours(sections: &[Section], dir: &std::path::Path) -> Vec<Outcome> {
    let mut db = rustqlite::Database::open(dir.join("ours.db")).unwrap();
    let mut out = Vec::with_capacity(sections.len());
    for s in sections {
        let mut o = Outcome::default();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let t0 = Instant::now();
        for (k, sql) in s.stmts.iter().enumerate() {
            let p: Vec<rustqlite::Value> = s
                .params
                .get(k)
                .map(|ls| {
                    ls.iter()
                        .map(|l| match l {
                            Lit::Int(i) => rustqlite::Value::Integer(*i),
                            Lit::Real(f) => rustqlite::Value::Real(*f),
                            Lit::Text(t) => rustqlite::Value::Text(t.clone().into()),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let r = if returns_rows(sql) {
                db.query(sql, p).map(|rows| {
                    let mut v: Vec<Vec<String>> = rows
                        .iter()
                        .map(|r| r.iter().map(ours_value).collect())
                        .collect();
                    hash_rows(&mut v, &mut h);
                })
            } else {
                db.execute(sql, p)
            };
            if let Err(e) = r {
                o.errors += 1;
                o.first_error
                    .get_or_insert_with(|| format!("{e} :: {}", &sql[..sql.len().min(120)]));
            }
        }
        o.time = t0.elapsed();
        o.hash = h.finish();
        out.push(o);
    }
    out
}

fn run_sqlite(sections: &[Section], dir: &std::path::Path) -> Vec<Outcome> {
    let db = rusqlite::Connection::open(dir.join("sqlite.db")).unwrap();
    db.set_prepared_statement_cache_capacity(256);
    let mut out = Vec::with_capacity(sections.len());
    for s in sections {
        let mut o = Outcome::default();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let t0 = Instant::now();
        for (k, sql) in s.stmts.iter().enumerate() {
            let r: rusqlite::Result<()> = (|| {
                let lits = s.params.get(k).filter(|l| !l.is_empty());
                let mut st = db.prepare_cached(sql)?;
                if let Some(ls) = lits {
                    for (i, l) in ls.iter().enumerate() {
                        match l {
                            Lit::Int(v) => st.raw_bind_parameter(i + 1, v)?,
                            Lit::Real(v) => st.raw_bind_parameter(i + 1, v)?,
                            Lit::Text(v) => st.raw_bind_parameter(i + 1, v.as_str())?,
                        }
                    }
                }
                let n = st.column_count();
                let mut rows = st.raw_query();
                let mut v: Vec<Vec<String>> = Vec::new();
                while let Some(row) = rows.next()? {
                    v.push(
                        (0..n)
                            .map(|i| sqlite_value(row.get_ref_unwrap(i)))
                            .collect(),
                    );
                }
                if returns_rows(sql) {
                    hash_rows(&mut v, &mut h);
                }
                Ok(())
            })();
            if let Err(e) = r {
                o.errors += 1;
                o.first_error
                    .get_or_insert_with(|| format!("{e} :: {}", &sql[..sql.len().min(120)]));
            }
        }
        o.time = t0.elapsed();
        o.hash = h.finish();
        out.push(o);
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let script = args
        .first()
        .expect("usage: speedtest_replay SCRIPT [WORKDIR]");
    let work = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("speedtest_replay"));
    let only = args
        .iter()
        .position(|a| a == "--only")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let text = std::fs::read_to_string(script).unwrap();
    let mut sections = parse_script(&text);
    if args.iter().any(|a| a == "--params") {
        for s in sections.iter_mut() {
            let mut params = Vec::with_capacity(s.stmts.len());
            for sql in s.stmts.iter_mut() {
                let (t, l) = parameterize(sql);
                *sql = t;
                params.push(l);
            }
            s.params = params;
        }
    }
    let n_stmts: usize = sections.iter().map(|s| s.stmts.len()).sum();
    eprintln!("{} sections, {} statements", sections.len(), n_stmts);
    let mut results: BTreeMap<&str, Vec<Outcome>> = BTreeMap::new();
    for engine in ["ours", "sqlite"] {
        if only.as_deref().is_some_and(|o| o != engine) {
            continue;
        }
        let dir = work.join(engine);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = if engine == "ours" {
            run_ours(&sections, &dir)
        } else {
            run_sqlite(&sections, &dir)
        };
        results.insert(engine, r);
    }
    let (Some(a), Some(b)) = (results.get("ours"), results.get("sqlite")) else {
        for (e, r) in &results {
            let t: Duration = r.iter().map(|o| o.time).sum();
            println!("{e}: {t:?}");
        }
        return;
    };
    println!(
        "{:>3} {:>4}  {:<58} {:>10} {:>10} {:>6}  notes",
        "set", "test", "name", "ours ms", "sqlite ms", "ratio"
    );
    let (mut ta, mut tb) = (Duration::ZERO, Duration::ZERO);
    let (mut wins, mut losses, mut mismatches) = (0, 0, 0);
    for (i, s) in sections.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        ta += x.time;
        tb += y.time;
        let ratio = y.time.as_secs_f64() / x.time.as_secs_f64().max(1e-9);
        let mut notes = String::new();
        if x.hash != y.hash {
            mismatches += 1;
            notes.push_str("RESULT MISMATCH ");
        }
        if x.errors != y.errors {
            notes.push_str(&format!(
                "errors ours={} sqlite={}: {} ",
                x.errors,
                y.errors,
                x.first_error
                    .as_deref()
                    .or(y.first_error.as_deref())
                    .unwrap_or("")
            ));
        }
        // Sections under 1 ms in both are noise; they never count.
        if x.time.max(y.time) >= Duration::from_millis(1) {
            if ratio >= 1.0 {
                wins += 1;
            } else {
                losses += 1;
                notes.push_str("LOSS ");
            }
        }
        println!(
            "{:>3} {:>4}  {:<58} {:>10.2} {:>10.2} {:>5.2}x  {}",
            s.set,
            s.number,
            &s.name[..s.name.len().min(58)],
            x.time.as_secs_f64() * 1e3,
            y.time.as_secs_f64() * 1e3,
            ratio,
            notes
        );
    }
    println!(
        "TOTAL ours {:.1} ms, sqlite {:.1} ms ({:.2}x); {} wins, {} losses, {} result mismatches",
        ta.as_secs_f64() * 1e3,
        tb.as_secs_f64() * 1e3,
        tb.as_secs_f64() / ta.as_secs_f64().max(1e-9),
        wins,
        losses,
        mismatches
    );
}
