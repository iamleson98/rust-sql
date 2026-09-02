//! SQLlogictest (SLT) runner for rustqlite.
//!
//! The SLT format is the industry-standard cross-engine SQL test corpus
//! (https://www.sqlite.org/sqllogictest/). It's how SQLite itself verifies
//! result-equivalence against other engines (PostgreSQL, MySQL, Oracle, etc.).
//! SQLite's own TCL test suite (~45k cases) and TH3 are not public, so SLT
//! is the closest we can get to "all tests SQLite has" without licensing
//! TH3.
//!
//! File format
//! -----------
//! Each `.test` file is a sequence of records. Each record starts with a
//! directive line:
//!
//!     statement ok       <optional sql on same line>
//!     statement error    <optional sql on same line>
//!     query TTT rowsort  <optional sql on same line>
//!     hash-threshold 8
//!     halt
//!     skip
//!
//! The directive may be followed by additional SQL lines until either:
//!   - a line containing only `----` (for queries — separates SQL from
//!     expected rows), or
//!   - a blank line (for statements — ends the record).
//!
//! For `query`, after `----`, the expected rows follow. Each row has one
//! value per column, space-separated. NULL is represented by `NULL`. The
//! row count must match the query result.
//!
//! Sort modes for `query`:
//!   - `nosort`    — compare rows in the order produced by the engine.
//!   - `rowsort`   — sort both expected and actual rows lexicographically
//!     before comparison (column-by-column, in declaration order).
//!   - `valuesort` — flatten and sort all values from both sides.
//!   - `rowid`     — sort by rowid (treat first column as rowid).
//!
//! Type codes in the type string:
//!   - `I` integer
//!   - `R` real
//!   - `T` text
//!   - `B` blob
//!   - `?` any (we just compare by string repr)
//!
//! Comment lines start with `#`. They are skipped. Blank lines outside a
//! record are skipped.
//!
//! Directives `halt` and `skip` stop processing the current file (`halt`)
//! or skip the next record (`skip`).
//!
//! For our starter, we implement: `statement ok`, `statement error`,
//! `query III rowsort/nosort/valuesort`, `hash-threshold`, `halt`, `skip`,
//! and `#` comments. Hash-threshold is honored — when the expected row
//! count exceeds it, we hash all values (concatenated with newlines) and
//! compare against the hash of the actual values, instead of listing rows.

use rustqlite::{Database, Value};
use std::path::Path;

// ===========================================================================
// Test runner: parse + execute a single .test file.
// ===========================================================================

#[derive(Debug, Clone)]
enum Directive {
    StatementOk,
    StatementError,
    Query { types: Vec<char>, sort: SortMode },
    HashThreshold(usize),
    Halt,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    NoSort,
    RowSort,
    ValueSort,
    RowId,
}

#[derive(Debug, Clone)]
enum Record {
    Statement {
        expect_error: bool,
        sql: String,
    },
    Query {
        types: Vec<char>,
        sort: SortMode,
        sql: String,
        expected: Vec<Vec<String>>,
    },
    HashThreshold(usize),
    Halt,
}

/// Parse a `.test` file into a list of records.
fn parse_test_file(content: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut skip_next = false;
    while i < lines.len() {
        let line = lines[i].trim_end();
        // Skip blank lines and comments outside a record.
        if line.is_empty() {
            i += 1;
            continue;
        }
        if line.starts_with('#') {
            i += 1;
            continue;
        }
        // Parse directive.
        let (directive, rest_of_line) = parse_directive(line);
        let record = match directive {
            Directive::HashThreshold(n) => Record::HashThreshold(n),
            Directive::Halt => {
                records.push(Record::Halt);
                break;
            }
            Directive::Skip => {
                skip_next = true;
                i += 1;
                continue;
            }
            Directive::StatementOk | Directive::StatementError => {
                // Collect SQL lines until a blank line.
                let mut sql = String::new();
                if !rest_of_line.is_empty() {
                    sql.push_str(rest_of_line);
                    sql.push('\n');
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
                    expect_error: matches!(directive, Directive::StatementError),
                    sql: sql.trim().to_string(),
                }
            }
            Directive::Query { types, sort } => {
                // Collect SQL lines until `----` or blank line.
                let mut sql = String::new();
                if !rest_of_line.is_empty() {
                    sql.push_str(rest_of_line);
                    sql.push('\n');
                }
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
                        // End of query without separator — no expected rows.
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
                        // Each value separated by spaces; quote handling for
                        // strings with spaces: `a "b c" d` => [a, "b c", d].
                        let row = parse_expected_row(l);
                        expected.push(row);
                        i += 1;
                    }
                }
                Record::Query {
                    types,
                    sort,
                    sql: sql.trim().to_string(),
                    expected,
                }
            }
        };
        if skip_next {
            skip_next = false;
            i += 1;
            continue;
        }
        records.push(record);
        // i is already advanced past the consumed lines.
    }
    records
}

/// Parse a directive line: returns (directive, remainder of line).
fn parse_directive(line: &str) -> (Directive, &str) {
    let line = line.trim();
    let mut parts = line.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim_start();
    match first {
        "statement" => {
            // Either `statement ok` or `statement error`.
            let kind = rest.split_whitespace().next().unwrap_or("");
            match kind {
                "ok" => (Directive::StatementOk, ""),
                "error" => (Directive::StatementError, ""),
                _ => (Directive::StatementOk, ""), // be lenient
            }
        }
        "query" => {
            // Format: query <types> <sortmode> [label]
            // types is a string of [IRTB?], sortmode is one of nosort/rowsort/valuesort/rowid.
            let mut tokens = rest.split_whitespace();
            let type_str = tokens.next().unwrap_or("");
            let sort_str = tokens.next().unwrap_or("nosort");
            let types: Vec<char> = type_str.chars().collect();
            let sort = match sort_str {
                "rowsort" => SortMode::RowSort,
                "valuesort" => SortMode::ValueSort,
                "rowid" => SortMode::RowId,
                _ => SortMode::NoSort,
            };
            (Directive::Query { types, sort }, "")
        }
        "hash-threshold" => {
            let n: usize = rest.trim().parse().unwrap_or(0);
            (Directive::HashThreshold(n), "")
        }
        "halt" => (Directive::Halt, ""),
        "skip" => (Directive::Skip, ""),
        _ => {
            // Unknown directive — treat as statement ok with the line as SQL.
            (Directive::StatementOk, line)
        }
    }
}

/// Parse one expected-result row. Whitespace-separated values; double-quoted
/// strings can contain spaces. `NULL` (case-insensitive) means SQL NULL.
fn parse_expected_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Consumed by value by the `for` loop below — no peeking, so no `mut`.
    let chars = line.chars().peekable();
    let mut cur = String::new();
    // 0 = unquoted, 1 = in double quotes, 2 = in single quotes.
    // Single-quoted values are verbatim — they exist so expected values
    // that CONTAIN double quotes (JSON1 output like [1,"two",3.5]) can be
    // written without escaping.
    let mut in_quotes = 0u8;
    for c in chars {
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

/// Format a rustqlite Value as SLT expected-text.
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
                    "inf".to_string()
                } else {
                    "-inf".to_string()
                };
            }
            // SQLite SLT canonical form for reals: print with at least one
            // decimal place. Use the shortest round-trippable representation.
            let s = format!("{}", f);
            if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                format!("{}.0", s)
            } else {
                s
            }
        }
        Value::Text(s) => s.as_str().to_owned(),
        Value::Blob(b) => {
            // SLT prints blobs as X'...hex...' but we don't have blob tests yet.
            let hex: String = b.iter().map(|x| format!("{:02x}", x)).collect();
            format!("X'{}'", hex)
        }
    }
}

/// Run a single record against a database. Returns Ok(()) on pass, Err(msg) on fail.
fn run_record(db: &mut Database, rec: &Record, hash_threshold: usize) -> Result<(), String> {
    match rec {
        Record::HashThreshold(_) => Ok(()), // handled at runner level
        Record::Halt => Ok(()),
        Record::Statement { expect_error, sql } => {
            let result = db.execute(sql, []);
            match (result, expect_error) {
                (Ok(()), false) => Ok(()),
                (Ok(()), true) => Err(format!(
                    "statement ok: expected error, got success: {}",
                    sql
                )),
                (Err(_), true) => Ok(()),
                (Err(e), false) => Err(format!(
                    "statement ok: expected success, got error: {}: {}",
                    e, sql
                )),
            }
        }
        Record::Query {
            types,
            sort,
            sql,
            expected,
        } => {
            let result = db.query_with_columns(sql, []);
            let (cols, rows) = match result {
                Ok(r) => r,
                Err(e) => return Err(format!("query failed: {}: {}", e, sql)),
            };
            // Format actual rows as strings.
            let actual: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(format_value).collect())
                .collect();
            // Coerce expected value types.
            let _ = cols;
            let _ = types; // currently unused; could verify column types.

            // Hash mode: if expected row count exceeds hash_threshold,
            // compare hash of all values rather than explicit rows.
            if hash_threshold > 0 && expected.len() >= hash_threshold {
                let expected_hash = hash_all_values(expected);
                let actual_hash = hash_all_values(&actual);
                if expected_hash == actual_hash {
                    Ok(())
                } else {
                    Err(format!(
                        "query hash mismatch (expected {} rows hashed to {}, got {} rows hashed to {}): {}",
                        expected.len(), expected_hash, actual.len(), actual_hash, sql
                    ))
                }
            } else {
                // Compare row counts.
                if actual.len() != expected.len() {
                    let mut diag = format!(
                        "row count mismatch: expected {}, got {} | SQL: {}\n",
                        expected.len(),
                        actual.len(),
                        sql
                    );
                    let limit = expected.len().max(actual.len()).min(10);
                    for i in 0..limit {
                        let exp = expected.get(i).cloned().unwrap_or_default().join(" ");
                        let act = actual.get(i).cloned().unwrap_or_default().join(" ");
                        diag.push_str(&format!("  row {}: expected [{}] got [{}]\n", i, exp, act));
                    }
                    return Err(diag);
                }
                // Sort both sides if sort mode requires.
                let (mut actual_sorted, mut expected_sorted) = (actual.clone(), expected.clone());
                match sort {
                    SortMode::NoSort => {} // no change
                    SortMode::RowSort => {
                        actual_sorted.sort();
                        expected_sorted.sort();
                    }
                    SortMode::ValueSort => {
                        let mut a: Vec<String> = actual_sorted.into_iter().flatten().collect();
                        a.sort();
                        let mut e: Vec<String> = expected_sorted.into_iter().flatten().collect();
                        e.sort();
                        if a == e {
                            return Ok(());
                        }
                        return Err(format!(
                            "valuesort mismatch: expected {} values, got {} values | SQL: {}\nfirst diff: exp={:?} act={:?}",
                            e.len(), a.len(), sql,
                            e.first(), a.first()
                        ));
                    }
                    SortMode::RowId => {
                        // Sort by first column.
                        actual_sorted.sort_by(|a, b| a.first().cmp(&b.first()));
                        expected_sorted.sort_by(|a, b| a.first().cmp(&b.first()));
                    }
                }
                // Compare row by row.
                for (i, (a, e)) in actual_sorted.iter().zip(expected_sorted.iter()).enumerate() {
                    if a != e {
                        return Err(format!(
                            "row {} mismatch: expected [{}], got [{}] | SQL: {}",
                            i,
                            e.join(" "),
                            a.join(" "),
                            sql
                        ));
                    }
                }
                Ok(())
            }
        }
    }
}

fn hash_all_values(rows: &[Vec<String>]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for row in rows {
        for v in row {
            v.hash(&mut h);
            0u8.hash(&mut h);
        }
        1u8.hash(&mut h);
    }
    h.finish()
}

/// Run all `.test` files under `dir`. Used by the integration test entry point.
pub fn run_dir(dir: &Path) -> Result<(usize, usize, Vec<String>), String> {
    let mut passed = 0;
    let mut failed = 0;
    let mut errors: Vec<String> = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {}", dir.display(), e))?;
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("test"))
        .collect();
    paths.sort();
    for path in paths {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{}: read error: {}", path.display(), e));
                failed += 1;
                continue;
            }
        };
        let records = parse_test_file(&content);
        let mut db = Database::open_in_memory().map_err(|e| format!("open_in_memory: {}", e))?;
        let mut hash_threshold = 0;
        let mut halted = false;
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        for rec in &records {
            if halted {
                break;
            }
            match rec {
                Record::HashThreshold(n) => {
                    hash_threshold = *n;
                    continue;
                }
                Record::Halt => {
                    halted = true;
                    continue;
                }
                _ => {}
            }
            match run_record(&mut db, rec, hash_threshold) {
                Ok(()) => passed += 1,
                Err(msg) => {
                    errors.push(format!("{}: {}", file_name, msg));
                    failed += 1;
                }
            }
        }
    }
    Ok((passed, failed, errors))
}

// ===========================================================================
// Integration test entry point.
// ===========================================================================

#[test]
fn slt_runner_runs_all_cases() {
    let dir = Path::new("tests/slt/cases");
    if !dir.exists() {
        eprintln!("slt_runner: no cases/ dir, skipping");
        return;
    }
    let (passed, failed, errors) = match run_dir(dir) {
        Ok(t) => t,
        Err(e) => {
            panic!("slt_runner fatal: {}", e);
        }
    };
    eprintln!("slt_runner: {}/{} passed", passed, passed + failed);
    if !errors.is_empty() {
        let preview: Vec<String> = errors.iter().take(20).cloned().collect();
        panic!(
            "slt_runner: {} cases failed (showing first {}):\n{}",
            failed,
            preview.len(),
            preview.join("\n")
        );
    }
}
