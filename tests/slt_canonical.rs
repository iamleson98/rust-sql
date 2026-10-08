//! Canonical SQLite sqllogictest corpus audit (REPORT-ONLY, `#[ignore]`d).
//!
//! The corpus: https://github.com/gregrahn/sqllogictest — SQLite's own
//! test suite transcribed into the SLT format by D. Richard Hipp (the
//! deterministic `select1-5.test`, the `evidence/` language-evidence
//! files, `index/*`, and the `random/` combinatorial farms).
//!
//! This is the compatibility-surface AUDIT: it runs every record the
//! corpus marks applicable to the SQLite dialect (`onlyif sqlite` /
//! `skipif <other>`) and reports per-file pass/fail counts with the
//! first failing SQL, expected and actual rows. It is a report tool,
//! not a gate: `cargo test --release --test slt_canonical -- --ignored`
//! with:
//!   - SLT_DIR corpus `test/` directory
//!     (default /tmp/sqllogictest-master/test)
//!   - SLT_FILTER  substring filter on file paths
//!   - SLT_MAXFAIL per-file failure details cap (default 3)
//!   - SLT_STRICT  1 = fail the test when any file failed

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use rustqlite::{Database, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // nosort/rowsort/valuesort are the SLT format's own record labels
enum SortMode {
    NoSort,
    RowSort,
    ValueSort,
}

#[derive(Debug, Clone)]
enum Record {
    Statement {
        expect_error: bool,
        sql: String,
    },
    Query {
        sort: SortMode,
        sql: String,
        /// Explicit expected rows (when given).
        expected: Vec<Vec<String>>,
        /// `N values hashing to <hash>` — row-count verification only
        /// (the corpus's MD5 hash is not reproduced here).
        hashed: Option<usize>,
    },
}

/// A file's records, with onlyif/skipif already applied.
fn parse_file(content: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    // Pending engine condition from `onlyif`/`skipif` directives:
    // None = include, Some(false) = exclude.
    let mut include_next: Option<bool> = None;
    let mut skip_records = 0usize;
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        match first {
            "onlyif" | "skipif" => {
                let engine = line.split_whitespace().nth(1).unwrap_or("");
                let is_us = engine.eq_ignore_ascii_case("sqlite");
                let include = if first == "onlyif" { is_us } else { !is_us };
                include_next = Some(include);
                i += 1;
                continue;
            }
            "halt" => break,
            "hash-threshold" | "mode" => {
                i += 1;
                continue;
            }
            "skip" => {
                skip_records = 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        let record = if first == "statement" {
            let kind = line.split_whitespace().nth(1).unwrap_or("ok");
            let mut sql = String::new();
            let rest = line.splitn(3, char::is_whitespace).nth(2);
            if let Some(r) = rest {
                if !r.trim().is_empty() {
                    sql.push_str(r.trim());
                    sql.push('\n');
                }
            }
            i += 1;
            while i < lines.len() {
                let l = lines[i].trim_end();
                if l.is_empty() {
                    break;
                }
                if l.starts_with('#') {
                    i += 1;
                    continue;
                }
                sql.push_str(l);
                sql.push('\n');
                i += 1;
            }
            Record::Statement {
                expect_error: kind == "error",
                sql: sql.trim().to_string(),
            }
        } else if first == "query" {
            let mut tokens = line.split_whitespace().skip(1);
            let _types = tokens.next().unwrap_or("");
            let sort_str = tokens.next().unwrap_or("nosort");
            let sort = match sort_str {
                "rowsort" => SortMode::RowSort,
                "valuesort" => SortMode::ValueSort,
                _ => SortMode::NoSort,
            };
            // The canonical corpus NEVER carries SQL on a query's
            // directive line: the remainder (when present) is a record
            // LABEL (`label-all`, `x0`, `join-4-1`, ...). The SQL starts
            // on the following line, always.
            let mut sql = String::new();
            i += 1;
            let mut found_sep = false;
            while i < lines.len() {
                let l = lines[i].trim_end();
                if l == "----" {
                    found_sep = true;
                    i += 1;
                    break;
                }
                if l.is_empty() {
                    break;
                }
                if l.starts_with('#') {
                    i += 1;
                    continue;
                }
                sql.push_str(l);
                sql.push('\n');
                i += 1;
            }
            let mut expected: Vec<Vec<String>> = Vec::new();
            let mut hashed: Option<usize> = None;
            if found_sep {
                while i < lines.len() {
                    let l = lines[i].trim_end();
                    if l.is_empty() {
                        break;
                    }
                    if l.starts_with('#') {
                        i += 1;
                        continue;
                    }
                    // `N values hashing to <md5>` — the hash-compressed
                    // result block (random/*'s big farms).
                    if let Some(pos) = l.find(" values hashing to ") {
                        if let Ok(n) = l[..pos].parse::<usize>() {
                            hashed = Some(n);
                            i += 1;
                            continue;
                        }
                    }
                    expected.push(parse_row(l));
                    i += 1;
                }
            }
            Record::Query {
                sort,
                sql: sql.trim().to_string(),
                expected,
                hashed,
            }
        } else {
            // Unknown directive: stop the file (faithful halt on foreign
            // syntax beats mis-parsing SQL).
            eprintln!("slt_canonical: unknown directive, halting file: {line}");
            break;
        };
        let include = include_next.take().unwrap_or(true);
        if skip_records > 0 {
            skip_records -= 1;
            continue;
        }
        if include {
            records.push(record);
        }
    }
    let _ = &mut skip_records;
    records
}

fn parse_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = 0u8;
    for c in line.chars() {
        if in_quotes == 1 {
            if c == '"' {
                in_quotes = 0;
                out.push(std::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        } else if in_quotes == 2 {
            if c == '\'' {
                in_quotes = 0;
                out.push(std::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = 1;
        } else if c == '\'' {
            in_quotes = 2;
        } else if c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => {
            if f.is_nan() {
                return "NULL".to_string();
            }
            if f.is_infinite() {
                return if *f > 0.0 {
                    "inf".into()
                } else {
                    "-inf".into()
                };
            }
            let s = format!("{}", f);
            if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                format!("{s}.0")
            } else {
                s
            }
        }
        Value::Text(s) => s.as_str().to_owned(),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("X'{hex}'")
        }
    }
}

fn run_record(db: &mut Database, rec: &Record) -> Result<(), String> {
    match rec {
        Record::Statement { expect_error, sql } => {
            let result = db.execute(sql, []);
            match (result, expect_error) {
                (Ok(_), false) => Ok(()),
                (Ok(_), true) => Err(format!("expected error, got success | {sql}")),
                (Err(_), true) => Ok(()),
                (Err(e), false) => Err(format!("expected success, got error: {e} | {sql}")),
            }
        }
        Record::Query {
            sort,
            sql,
            expected,
            hashed,
        } => {
            let rows = match db.query(sql, []) {
                Ok(r) => r,
                Err(e) => return Err(format!("query failed: {e} | {sql}")),
            };
            let actual: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(format_value).collect())
                .collect();
            if let Some(n) = hashed {
                // Hash-compressed block: verify the VALUE count only (the
                // corpus's MD5 is not reproduced here).
                let got: usize = actual.iter().map(|r| r.len()).sum();
                if got == *n {
                    return Ok(());
                }
                return Err(format!(
                    "hashed block value count: expected {n}, got {got} | {sql}"
                ));
            }
            let exp_flat: usize = expected.iter().map(|r| r.len()).sum();
            let _act_flat: usize = actual.iter().map(|r| r.len()).sum();
            if exp_flat == 0 && actual.is_empty() {
                return Ok(());
            }
            match sort {
                SortMode::NoSort => {
                    if actual.len() != expected.len() {
                        return Err(format!(
                            "row count: expected {}, got {} | {sql}",
                            expected.len(),
                            actual.len()
                        ));
                    }
                    for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
                        if a != e {
                            return Err(format!(
                                "row {i}: expected [{}], got [{}] | {sql}",
                                e.join(" "),
                                a.join(" ")
                            ));
                        }
                    }
                    Ok(())
                }
                SortMode::RowSort => {
                    let mut a = actual.clone();
                    let mut e = expected.clone();
                    a.sort();
                    e.sort();
                    if a == e {
                        Ok(())
                    } else {
                        let n = a.len().max(e.len());
                        let da = a.first().cloned().unwrap_or_default().join(" ");
                        let de = e.first().cloned().unwrap_or_default().join(" ");
                        Err(format!(
                            "rowsort mismatch ({n} rows): first exp [{de}] act [{da}] | {sql}"
                        ))
                    }
                }
                SortMode::ValueSort => {
                    let mut a: Vec<String> = actual.iter().flatten().cloned().collect();
                    let mut e: Vec<String> = expected.iter().flatten().cloned().collect();
                    a.sort();
                    e.sort();
                    if a == e {
                        Ok(())
                    } else {
                        Err(format!(
                            "valuesort mismatch ({} values): first exp {:?} act {:?} | {sql}",
                            e.len(),
                            e.first(),
                            a.first()
                        ))
                    }
                }
            }
        }
    }
}

fn walk_tests(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk_tests(&p, out);
            } else if p.extension().map(|e| e == "test").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

#[test]
#[ignore = "audit tool: needs the canonical corpus + is report-only"]
fn slt_canonical_audit() {
    let dir =
        std::env::var("SLT_DIR").unwrap_or_else(|_| "/tmp/sqllogictest-master/test".to_string());
    let filter = std::env::var("SLT_FILTER").unwrap_or_default();
    let maxfail: usize = std::env::var("SLT_MAXFAIL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let strict = std::env::var("SLT_STRICT").unwrap_or_default() == "1";

    let mut files = Vec::new();
    walk_tests(Path::new(&dir), &mut files);
    files.sort();
    if !filter.is_empty() {
        files.retain(|p| p.to_string_lossy().contains(&filter));
    }
    assert!(
        !files.is_empty(),
        "no .test files under {dir} (set SLT_DIR)"
    );

    let mut total_ok = 0usize;
    let mut total_fail = 0usize;
    let mut n_files = 0usize;
    let mut files_failed = Vec::new();
    let queue: VecDeque<PathBuf> = files.into_iter().collect();
    for path in queue {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP {}: {e}", path.display());
                continue;
            }
        };
        n_files += 1;
        let records = parse_file(&content);
        let mut db = Database::open_in_memory().unwrap();
        let mut ok = 0usize;
        let mut fails: Vec<String> = Vec::new();
        for rec in &records {
            match run_record(&mut db, rec) {
                Ok(()) => ok += 1,
                Err(msg) => {
                    if fails.len() < maxfail {
                        fails.push(msg);
                    }
                    // A failed statement can cascade; keep going (the
                    // corpus's own runner stops at first failure — the
                    // audit wants the breadth).
                }
            }
        }
        let nfail = records.len().saturating_sub(ok);
        total_ok += ok;
        total_fail += nfail;
        let rel = path
            .strip_prefix(&dir)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        let status = if nfail == 0 { "PASS" } else { "FAIL" };
        if nfail > 0 {
            files_failed.push(rel.clone());
        }
        println!("slt {status} {rel}: {ok}/{} records", records.len());
        for f in &fails {
            println!("  | {f}");
        }
    }
    println!(
        "slt_canonical: {} files, {} records ok, {} failed, {} files failing",
        n_files,
        total_ok,
        total_fail,
        files_failed.len()
    );
    if strict && !files_failed.is_empty() {
        panic!("slt_canonical: {} files failed", files_failed.len());
    }
}
