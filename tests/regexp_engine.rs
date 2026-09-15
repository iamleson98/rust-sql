//! End-to-end regular-expression surface (PostgreSQL / SQLite REGEXP).
//!
//! Covers the `x REGEXP y` operator (real POSIX ERE engine — leftmost-
//! longest, linear-time NFA), the SQLite-convention `regexp(pattern,
//! string)` scalar, PostgreSQL 15's `regexp_like` / `regexp_replace` /
//! `regexp_substr` / `regexp_instr` / `regexp_count` family with flags,
//! NULL three-valued logic, error surfaces, and the pg_trgm-borrowed
//! `similarity` / `word_similarity` / `show_trgm` functions.

use rustqlite::{Database, Value};

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn one(db: &Database, sql: &str) -> Value {
    let rows = db.query(sql, []).unwrap();
    rows.first()
        .and_then(|r| r.first().cloned())
        .unwrap_or(Value::Null)
}

fn text(db: &Database, sql: &str) -> String {
    match one(db, sql) {
        Value::Text(s) => s.to_string(),
        v => panic!("expected TEXT from `{sql}`, got {v:?}"),
    }
}

fn int(db: &Database, sql: &str) -> i64 {
    match one(db, sql) {
        Value::Integer(i) => i,
        v => panic!("expected INTEGER from `{sql}`, got {v:?}"),
    }
}

fn real(db: &Database, sql: &str) -> f64 {
    match one(db, sql) {
        Value::Real(f) => f,
        v => panic!("expected REAL from `{sql}`, got {v:?}"),
    }
}

#[test]
fn regexp_operator_basics() {
    let d = db();
    assert_eq!(int(&d, "SELECT 'hello 123' REGEXP '[0-9]+'"), 1);
    assert_eq!(int(&d, "SELECT 'hello' REGEXP '[0-9]+'"), 0);
    // POSIX leftmost-longest: 'a|ab' on 'ab' takes the LONGER alternative.
    assert_eq!(int(&d, "SELECT 'ab' REGEXP 'a|ab'"), 1);
    // Anchors
    assert_eq!(int(&d, "SELECT 'abc' REGEXP '^ab'"), 1);
    assert_eq!(int(&d, "SELECT 'xabc' REGEXP '^ab'"), 0);
    assert_eq!(int(&d, "SELECT 'abc' REGEXP 'bc$'"), 1);
    // NOT REGEXP
    assert_eq!(int(&d, "SELECT 'hello' NOT REGEXP '[0-9]+'"), 1);
    // NULL three-valued logic
    assert_eq!(one(&d, "SELECT NULL REGEXP 'x'"), Value::Null);
    assert_eq!(one(&d, "SELECT 'x' REGEXP NULL"), Value::Null);
    // No longer LIKE-shaped: REGEXP honors regex metacharacters — `.` is
    // the ANY wildcard in a regex, so escaped literals differ.
    assert_eq!(int(&d, "SELECT 'abc' REGEXP 'a.c'"), 1);
    assert_eq!(int(&d, "SELECT 'abc' REGEXP 'a\\.c'"), 0);
    assert_eq!(int(&d, "SELECT 'a.c' REGEXP 'a\\.c'"), 1);
}

#[test]
fn regexp_operator_in_where() {
    let mut d = db();
    d.execute("CREATE TABLE t(name TEXT)", []).unwrap();
    for n in ["alice", "bob", "carol", "dave42"] {
        d.execute("INSERT INTO t VALUES (?)", [Value::Text(n.into())])
            .unwrap();
    }
    let rows = d
        .query(
            "SELECT name FROM t WHERE name REGEXP '^[bc]' ORDER BY name",
            [],
        )
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r[0].as_text().to_string()).collect();
    assert_eq!(names, vec!["bob", "carol"]);

    let rows = d
        .query(
            "SELECT name FROM t WHERE name REGEXP '[0-9]+' ORDER BY name",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "dave42");

    // word-ish pattern with quantifier: alice is 5 chars -> a + .{4} + end
    let rows = d
        .query("SELECT name FROM t WHERE name REGEXP '^a.{4}$'", [])
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "alice");
}

#[test]
fn regexp_operator_invalid_pattern_errors() {
    let d = db();
    let err = d.query("SELECT 'x' REGEXP 'a('", []).unwrap_err();
    assert!(err.to_string().contains("regular expression"), "{}", err);
    let err = d.query("SELECT 'x' REGEXP 'a**'", []).unwrap_err();
    assert!(err.to_string().contains("regular expression"), "{}", err);
}

#[test]
fn sqlite_regexp_function() {
    let d = db();
    // SQLite convention: pattern FIRST.
    assert_eq!(int(&d, "SELECT regexp('^[0-9]+$', '12345')"), 1);
    assert_eq!(int(&d, "SELECT regexp('^[0-9]+$', '12a45')"), 0);
    assert_eq!(one(&d, "SELECT regexp(NULL, 'x')"), Value::Null);
    assert_eq!(one(&d, "SELECT regexp('x', NULL)"), Value::Null);
}

#[test]
fn pg_regexp_like_flags() {
    let d = db();
    assert_eq!(int(&d, "SELECT regexp_like('Hello World', 'world')"), 0);
    assert_eq!(
        int(&d, "SELECT regexp_like('Hello World', 'world', 'i')"),
        1
    );
    // 'c' resets 'i'
    assert_eq!(
        int(&d, "SELECT regexp_like('Hello World', 'world', 'ic')"),
        0
    );
    // unknown flag errors
    let err = d
        .query("SELECT regexp_like('x', 'x', 'q')", [])
        .unwrap_err();
    assert!(err.to_string().contains("flag"), "{}", err);
    // NULL source / pattern / flags
    assert_eq!(one(&d, "SELECT regexp_like(NULL, 'x')"), Value::Null);
    assert_eq!(one(&d, "SELECT regexp_like('x', NULL)"), Value::Null);
    assert_eq!(one(&d, "SELECT regexp_like('x', 'x', NULL)"), Value::Null);
}

#[test]
fn pg_regexp_replace() {
    let d = db();
    // default: first occurrence only (PG)
    assert_eq!(
        text(&d, "SELECT regexp_replace('a-b-c', 'b', 'X')"),
        "a-X-c"
    );
    // 'g' global
    assert_eq!(
        text(&d, "SELECT regexp_replace('a-b-c', '[bc]', 'X', 'g')"),
        "a-X-X"
    );
    // case-insensitive global
    assert_eq!(
        text(&d, "SELECT regexp_replace('AbAb', 'ab', 'x', 'gi')"),
        "xx"
    );
    // backrefs + whole-match ref
    assert_eq!(
        text(
            &d,
            "SELECT regexp_replace('2024-05-01', '^([0-9]{4})-([0-9]{2})', '\\2/\\1')"
        ),
        "05/2024-01"
    );
    assert_eq!(
        text(&d, "SELECT regexp_replace('ab', 'b', '<\\&>')"),
        "a<b>"
    );
    // no match: source unchanged
    assert_eq!(text(&d, "SELECT regexp_replace('abc', 'z', 'X')"), "abc");
    // NULL propagation
    assert_eq!(
        one(&d, "SELECT regexp_replace(NULL, 'a', 'b')"),
        Value::Null
    );
}

#[test]
fn pg_regexp_substr_and_instr() {
    let d = db();
    assert_eq!(
        text(&d, "SELECT regexp_substr('2024-05-01', '[0-9]+')"),
        "2024"
    );
    // start + occurrence
    assert_eq!(
        text(&d, "SELECT regexp_substr('2024-05-01', '[0-9]+', 6, 2)"),
        "01"
    );
    // no match -> NULL (PG)
    assert_eq!(
        one(&d, "SELECT regexp_substr('abc', '[0-9]+')"),
        Value::Null
    );
    // instr: 1-based char position, 0 when absent
    assert_eq!(int(&d, "SELECT regexp_instr('xxabcxx', 'abc')"), 3);
    assert_eq!(int(&d, "SELECT regexp_instr('xxabcxx', 'zzz')"), 0);
    // second occurrence
    assert_eq!(int(&d, "SELECT regexp_instr('a-b-a-b', 'b', 1, 2)"), 7);
}

#[test]
fn pg_regexp_count() {
    let d = db();
    assert_eq!(int(&d, "SELECT regexp_count('a1b22c333', '[0-9]+')"), 3);
    assert_eq!(int(&d, "SELECT regexp_count('abc', '[0-9]+')"), 0);
    // start offset skips earlier matches
    assert_eq!(int(&d, "SELECT regexp_count('a1b22c333', '[0-9]+', 3)"), 2);
    assert_eq!(one(&d, "SELECT regexp_count(NULL, 'a')"), Value::Null);
}

#[test]
fn regexp_unicode() {
    let d = db();
    // char-based positions, not bytes
    assert_eq!(int(&d, "SELECT regexp_instr('€b', 'b')"), 2);
    assert_eq!(text(&d, "SELECT regexp_substr('x=€5', '€[0-9]')"), "€5");
    // non-ASCII literals in patterns (café is 4 chars)
    assert_eq!(int(&d, "SELECT 'café' REGEXP 'caf'"), 1);
    assert_eq!(int(&d, "SELECT 'café' REGEXP 'caf.'"), 1);
    assert_eq!(int(&d, "SELECT 'café' REGEXP 'caf..'"), 0);
}

#[test]
fn regexp_linearity_guard() {
    let d = db();
    // The catastrophic-backtracking classic must complete (NFA, no
    // backtracking) — guarded by a timeout via the test harness itself.
    let n = 40;
    let sql = format!("SELECT '{}' REGEXP '(a+)+b'", "a".repeat(n));
    assert_eq!(int(&d, &sql), 0);
}

#[test]
fn trgm_similarity_family() {
    let d = db();
    // identical -> 1, disjoint -> 0
    assert_eq!(real(&d, "SELECT similarity('word', 'word')"), 1.0);
    assert_eq!(real(&d, "SELECT similarity('aaa', 'zzz')"), 0.0);
    // partial in (0,1)
    let s = real(&d, "SELECT similarity('hello', 'hallo')");
    assert!(s > 0.0 && s < 1.0, "similarity = {}", s);
    // word_similarity: word inside a longer string beats unrelated
    let inside = real(&d, "SELECT word_similarity('word', 'a word here')");
    let outside = real(&d, "SELECT word_similarity('word', 'zzzz unrelated')");
    assert!(inside > outside);
    assert!(inside > 0.5);
    assert!(outside < 0.2);
    // show_trgm: JSON array (pg returns text[]; this engine surfaces
    // arrays as JSON)
    assert_eq!(
        text(&d, "SELECT show_trgm('cat')"),
        r#"["  c"," ca","at ","cat"]"#
    );
    // NULLs
    assert_eq!(one(&d, "SELECT similarity(NULL, 'x')"), Value::Null);
    assert_eq!(one(&d, "SELECT word_similarity('x', NULL)"), Value::Null);
    assert_eq!(one(&d, "SELECT show_trgm(NULL)"), Value::Null);
}

#[test]
fn trgm_ordering_use_case() {
    let mut d = db();
    d.execute("CREATE TABLE products(name TEXT)", []).unwrap();
    for n in ["phone case", "phone charger", "laptop bag", "phone stand"] {
        d.execute("INSERT INTO products VALUES (?)", [Value::Text(n.into())])
            .unwrap();
    }
    // pg_trgm's classic ordering query: best matches first.
    let rows = d
        .query(
            "SELECT name FROM products
             WHERE similarity(name, 'phone') > 0.3
             ORDER BY similarity(name, 'phone') DESC, name",
            [],
        )
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r[0].as_text().to_string()).collect();
    assert!(!names.is_empty());
    assert!(names.iter().all(|n| n.contains("phone")));
    // 'laptop bag' must not survive the 0.3 threshold
    assert!(!names.contains(&"laptop bag".to_string()));
}

#[test]
fn regexp_recognized_builtins() {
    let d = db();
    // The names are recognized built-ins (dispatched without
    // registration; override rejection is covered by the C-ABI tests).
    assert_eq!(int(&d, "SELECT regexp_like('a', 'a')"), 1);
    assert_eq!(int(&d, "SELECT regexp('a', 'a')"), 1);
    assert!(real(&d, "SELECT similarity('a', 'a')") == 1.0);
}

#[test]
fn regexp_with_bound_parameters() {
    let mut d = db();
    d.execute("CREATE TABLE t(code TEXT)", []).unwrap();
    for c in ["ORD-2024-a", "ORD-2023-b", "INV-2024-c"] {
        d.execute("INSERT INTO t VALUES (?)", [Value::Text(c.into())])
            .unwrap();
    }
    let rows = d
        .query(
            "SELECT code FROM t WHERE code REGEXP ? ORDER BY code",
            [Value::Text("^ORD-2024".into())],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_text(), "ORD-2024-a");
}
