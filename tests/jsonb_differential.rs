//! Byte-identical JSONB differential tests against real SQLite.
//!
//! rusqlite (bundled real SQLite 3.46) is the oracle. Every case compares
//! the FULL observable behavior: `hex(jsonb(...))` for byte identity,
//! `json()` text for raw preservation, `quote(...)` for SQL-value
//! semantics, and complete `json_each`/`json_tree` rows (including the
//! `id`/`parent` columns, which are byte offsets in the canonical JSONB —
//! they only line up when the encoder is byte-identical).
//!
//! Cross-interop is covered too: JSONB blobs produced by rustqlite are
//! decoded by real SQLite, and vice versa.

use rusqlite::Connection;
use rustqlite::{Database, Value};

fn engine() -> Database {
    Database::open_in_memory().unwrap()
}

fn oracle() -> Connection {
    Connection::open_in_memory().unwrap()
}

/// Run one scalar SQL on both engines and compare the TEXT rendering.
/// `quote()` normalizes NULLs and type distinctions into comparable text.
fn cmp1(sql: &str) {
    let con = oracle();
    let want: String = con
        .query_row(&format!("select quote(({sql}))"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("sqlite failed: {sql} — {e}"));
    let db = engine();
    let rows = db
        .query(&format!("select quote(({sql}))"), ())
        .unwrap_or_else(|e| panic!("engine failed: {sql} — {e}"));
    let got = match rows.first().and_then(|r| r.first()) {
        Some(Value::Text(t)) => t.to_string(),
        other => panic!("engine non-text result for {sql}: {other:?}"),
    };
    assert_eq!(got, want, "mismatch for {sql}");
}

/// Multi-row comparison: both sides return rows of TEXT cells.
fn cmp_rows(sql: &str) {
    let con = oracle();
    let mut stmt = con.prepare(sql).unwrap();
    let ncol = stmt.column_count();
    let want: Vec<Vec<String>> = stmt
        .query_map([], move |r| {
            (0..ncol)
                .map(|i| r.get::<_, String>(i))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let db = engine();
    let rows = db.query(sql, ()).unwrap();
    let got: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Text(t) => t.to_string(),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect();
    assert_eq!(got, want, "row mismatch for {sql}");
}

// ---------------------------------------------------------------------------
// jsonb() byte identity — every element type and size class
// ---------------------------------------------------------------------------

#[test]
fn jsonb_byte_identity() {
    let docs = [
        "null",
        "true",
        "false",
        "0",
        "1",
        "-1",
        "127",
        "128",
        "-128",
        "300",
        "32767",
        "65536",
        "-9223372036854775808",
        "9223372036854775807",
        "9223372036854775808",
        "100000000000000000000",
        "12345678901234567890123",
        "0.0",
        "0.5",
        "1.5",
        "-0.0",
        "1e2",
        "1E2",
        "1e+2",
        "1e02",
        "0.0e0",
        "1e300",
        "1.5e-10",
        "1.50",
        "2.0",
        "3.141592653589793238",
        "'\"\"'",
        "'\"a\"'",
        "'\"hello\"'",
        "'\"héllo\"'",
        "'\"😀\"'",
        "'\"a\\\"b\"'",
        "'\"a\\\\b\"'",
        "'\"a\\nb\"'",
        "'\"a\\u0001b\"'",
        "'\"a\\u0062c\"'",
        "'\"a\\/b\"'",
        "'[]'",
        "'[1]'",
        "'[1,2,3]'",
        "'[1.5,\"x\",true,null]'",
        "'{}'",
        "'{\"a\":1}'",
        "'{\"a\":1,\"b\":\"x\"}'",
        "'{\"n\":-3,\"r\":2.5e3}'",
        "'{\"a\":[1,{\"b\":null}]}'",
        "'[[1],[2]]'",
        "'[+1]'",
        "'[.5]'",
        "'[1.]'",
        "'[0x10]'",
        "'[Infinity]'",
        "'[NaN]'",
        "'[.5e1]'",
        "'{\"a\":1,}'",
        "'[1,]'",
        "'{a:1}'",
        "json_array(1, 1.0, 'x')",
        "json_object('k', 1)",
        "json_object('k', 'v')",
        "json_quote(1e300)",
        "json_quote(0.1)",
        "json_quote(2.5)",
        "'  {\"a\" : 1}  '",
        "'\"aaaaaaaaaa\"'",
        "'\"aaaaaaaaaaa\"'",
        "'\"aaaaaaaaaaaa\"'",
    ];
    for doc in docs {
        cmp1(&format!("hex(jsonb({doc}))"));
    }
    // Size-class boundaries: strings of 11/12, 255/256, 65535/65536 bytes.
    for n in [11usize, 12, 255, 256, 65535, 65536] {
        let doc = format!("'\"{}\"'", "a".repeat(n));
        cmp1(&format!("hex(jsonb({doc}))"));
    }
    // Containers crossing every header size class.
    for n in [5usize, 6, 11, 12, 100, 1000, 33000, 70000] {
        let body = format!("[{}]", (0..n).map(|_| "0").collect::<Vec<_>>().join(","));
        let doc = format!("'{}'", body);
        cmp1(&format!("hex(jsonb({doc}))"));
    }
}

// ---------------------------------------------------------------------------
// json() raw preservation
// ---------------------------------------------------------------------------

#[test]
fn json_text_identity() {
    let docs = [
        "'[1e2, 1.50]'",
        "'\"a\\u0062c\"'",
        "'\"a\\/b\"'",
        "'\"a\\\\b\"'",
        "'  {\"a\" : 1}  '",
        "'{\"a\":1,}'",
        "'[1,]'",
        "'[+1]'",
        "'[.5]'",
        "'[0x10]'",
        "'[Infinity]'",
        "'[NaN]'",
        "'[.5e1]'",
        "'[+0.5]'",
        "'[-.5]'",
        "'[0.0e0]'",
        "'[1E2]'",
        "'[1e02]'",
        "'{\"a\":1,\"a\":2}'",
        "'{\"a\":{\"b\":[10,20,30]}}'",
        "'[1e+2]'",
        "'[1.]'",
    ];
    for doc in docs {
        cmp1(&format!("json({doc})"));
    }
}

// ---------------------------------------------------------------------------
// json_quote REAL rendering (SQLite's %!.15g + guaranteed decimal point)
// ---------------------------------------------------------------------------

#[test]
fn json_quote_reals() {
    for v in [
        "1",
        "1.0",
        "0.5",
        "1.5",
        "100.0",
        "2.5",
        "0.1",
        "0.0002",
        "2.0e-4",
        "1e15",
        "1e16",
        "1e10",
        "1e300",
        "1.0e10",
        "1e-5",
        "1e21",
        "4.9e-7",
        "123456789.12345678",
        "123456789012345678.0",
        "-1.5",
        "-0.0",
        "0.0",
    ] {
        cmp1(&format!("json_quote({v})"));
        cmp1(&format!("hex(jsonb(json_quote({v})))"));
    }
}

// ---------------------------------------------------------------------------
// -> and ->> operators
// ---------------------------------------------------------------------------

#[test]
fn arrow_operators() {
    let doc = "'{\"a\":{\"b\":[10,20,30]}}'";
    let arr = "'[10,20,30]'";
    let cases = [
        format!("{doc} -> '$.a'"),
        format!("{doc} -> '$.a.b'"),
        format!("{doc} -> '$.a.b[1]'"),
        format!("{doc} ->> '$.a.b[1]'"),
        format!("{doc} -> 'a'"),
        format!("{doc} ->> 'a'"),
        format!("{arr} -> 0"),
        format!("{arr} ->> 0"),
        format!("{arr} -> 1"),
        format!("{arr} -> '[1]'"),
        format!("{arr} ->> '[1]'"),
        format!("{arr} -> '$[0]'"),
        format!("{arr} ->> '$[#-1]'"),
        format!("{arr} ->> '$[#-2]'"),
        "'{\"a\":2}' -> '$.a'".to_string(),
        "'{\"a\":2}' ->> '$.a'".to_string(),
        "'{\"a\":\"x\"}' ->> '$.a'".to_string(),
        "'\"str\"' -> '$'".to_string(),
        "'\"str\"' ->> '$'".to_string(),
        "'1e2' -> '$'".to_string(),
        "'1e2' ->> '$'".to_string(),
        "'{\"a\":null}' -> 'a'".to_string(),
        "'{\"a\":null}' ->> 'a'".to_string(),
        "'[1,2]' ->> '$'".to_string(),
        "'{\"a\":1}' -> '$.a' + 1".to_string(),
        "'[1]' -> 0 * 5".to_string(),
        "'[1]' -> 0 || 'x'".to_string(),
        "'{\"a\":{\"b\":2}}' -> '$.a' -> 'b'".to_string(),
        "'{\"a\":1}' -> 'a.b'".to_string(),
        "'{\"a\":1}' -> 'bad path!'".to_string(),
        "jsonb('{\"a\":5}') -> 'a'".to_string(),
        "jsonb('{\"a\":5}') ->> 'a'".to_string(),
        "jsonb('[10,20]') ->> 1".to_string(),
        "NULL -> 'a'".to_string(),
        "'{\"a\":1}' -> NULL".to_string(),
    ];
    for case in cases {
        cmp1(&case);
    }
}

// ---------------------------------------------------------------------------
// jsonb_* mutator byte identity
// ---------------------------------------------------------------------------

#[test]
fn jsonb_mutators() {
    let cases = [
        "jsonb_set('{\"a\":1}', '$.b', 2)",
        "jsonb_set('{\"a\":1}', '$.a', 9)",
        "jsonb_set('[1]', '$[1]', 2)",
        "jsonb_set('[1]', '$[3]', 9)",
        "jsonb_insert('[1]', '$[1]', 2)",
        "jsonb_insert('[1]', '$[10]', 9)",
        "jsonb_replace('[1,2,3]', '$[1]', 9)",
        "jsonb_replace('{\"a\":1}', '$', 5)",
        "jsonb_remove('[1,2,3]', '$[1]')",
        "jsonb_remove('[1,2,3]', '$[0]', '$[2]')",
        "jsonb_remove('{\"a\":1,\"a\":2}', '$.a')",
        "jsonb_patch('{\"a\":1}', '{\"b\":2}')",
        "jsonb_patch('{\"x\":1e2}', '{\"y\":3}')",
        "jsonb_patch('{\"x\":1e2}', '{\"x\":null}')",
        "jsonb_set('{\"a\":1e2,\"b\":\"q\\\"s\"}', '$.c', 'new\"val')",
        "jsonb_extract('{\"a\":[1,2]}', '$.a')",
        "jsonb_extract('{\"a\":[1,2]}', '$.a[1]')",
    ];
    for case in cases {
        cmp1(&format!("hex({case})"));
    }
    // TEXT mutators preserve raw of untouched parts.
    let text_cases = [
        "json_set('{\"x\":1e2}', '$.y', 3)",
        "json_patch('{\"x\":1e2}', '{\"y\":3}')",
        "json_remove('{\"x\":1e2,\"y\":1}', '$.y')",
        "json_insert('{\"x\":1e2}', '$.y', 3)",
        "json_replace('{\"x\":1e2}', '$.x', 3)",
        "json_set('{\"a\":1,\"a\":2}', '$.a', 9)",
        "json_set('[1]', '$[3]', 9)",
        "json_remove('{\"a\":1}', '$')",
    ];
    for case in text_cases {
        cmp1(case);
    }
}

// ---------------------------------------------------------------------------
// json_extract / json_type / json_array_length semantics
// ---------------------------------------------------------------------------

#[test]
fn extract_semantics() {
    let cases = [
        "json_extract('{\"a\":1}', '$.a')",
        "json_extract('{\"a\":\"x\"}', '$.a')",
        "json_extract('{\"a\":null}', '$.a')",
        "json_extract('[1,2,3]', '$[1]')",
        "json_extract('[1,2,3]', '$[#-1]')",
        "json_extract('{\"a\":{\"b\":2}}', '$.a.b')",
        "json_extract('{\"a\":1}', '$.b')",
        "json_extract('[1,2]', '$')",
        "json_extract('[1.5,2]', '$[0]', '$[1]')",
        "json_extract('{\"a\":1,\"a\":2}', '$.a')",
        "json_extract(jsonb('{\"a\":1}'), '$.a')",
        "json_type('{\"a\":[1]}', '$.a')",
        "json_type('\"x\"')",
        "json_type('1.5')",
        "json_type('true')",
        "json_array_length('[1,2,3]')",
        "json_array_length('{\"a\":[1,2]}', '$.a')",
        "json_array_length('\"x\"')",
        "json(jsonb('1.0'))",
        "json(jsonb('1'))",
        "json(jsonb('[1e2]'))",
        "typeof(json_extract('[1]', '$'))",
        "typeof(json_extract('1.5', '$'))",
        "typeof(json_extract('\"x\"', '$'))",
        "typeof(json_extract('null', '$'))",
        "json_quote(NULL)",
        "json_quote(1)",
        "json_quote('a\"b')",
        "json_quote(jsonb('[1]'))",
    ];
    for case in cases {
        cmp1(case);
    }
}

// ---------------------------------------------------------------------------
// json_each / json_tree: full rows including id/parent byte offsets
// ---------------------------------------------------------------------------

#[test]
fn json_each_rows_identity() {
    for doc in [
        "'{\"a\":[1,2]}'",
        "'[1,2]'",
        "'5'",
        "'\"x\"'",
        "'[]'",
        "'{}'",
        "'{\"a\":{\"b\":1}}'",
        "'{\"a\":1,\"b\":null}'",
        "'[1.5,\"s\",true,null]'",
        "'{\"x\":1e2}'",
        "'{\"a\":[{\"b\":[1]}]}'",
    ] {
        for func in ["json_each", "json_tree"] {
            cmp_rows(&format!(
                "select quote(key), quote(value), quote(type), quote(atom), quote(id), \
                 quote(coalesce(parent, -1)), quote(fullkey), quote(path) from {func}({doc})"
            ));
        }
    }
    // Second path argument.
    cmp_rows(
        "select quote(key), quote(value), quote(type), quote(atom), quote(id), \
         quote(coalesce(parent, -1)), quote(fullkey), quote(path) from json_each('{\"a\":[1,2]}', '$.a')",
    );
    cmp_rows(
        "select quote(key), quote(value), quote(type), quote(atom), quote(id), \
         quote(coalesce(parent, -1)), quote(fullkey), quote(path) from json_tree('{\"a\":{\"b\":1}}', '$.a')",
    );
    cmp_rows(
        "select quote(key), quote(value), quote(type), quote(atom), quote(id), \
         quote(coalesce(parent, -1)), quote(fullkey), quote(path) from json_each('[1]', '$[0]')",
    );
    // Blob input walks the JSONB directly.
    cmp_rows(
        "select quote(key), quote(value), quote(type), quote(atom), quote(id), \
         quote(coalesce(parent, -1)), quote(fullkey), quote(path) from json_each(jsonb('{\"a\":1}'))",
    );
}

// ---------------------------------------------------------------------------
// json_valid flags
// ---------------------------------------------------------------------------

#[test]
fn json_valid_flags() {
    let inputs = [
        "'[1]'",
        "'[+1]'",
        "'[.5]'",
        "'[0x10]'",
        "'[1,]'",
        "'{\"a\":1,}'",
    ];
    for input in inputs {
        for flags in [1, 2, 3, 4, 5, 6, 8, 12, 15] {
            cmp1(&format!("json_valid({input}, {flags})"));
        }
    }
    cmp1("json_valid('[1]')");
    cmp1("json_valid('[+1]')");
    cmp1("json_valid(jsonb('[1]'))");
}

// ---------------------------------------------------------------------------
// json_pretty (4-space indent)
// ---------------------------------------------------------------------------

#[test]
fn json_pretty_identity() {
    for doc in [
        "'{\"a\":1}'",
        "'[]'",
        "'{}'",
        "'1'",
        "'\"x\"'",
        "'{\"a\":[1,{\"b\":2}]}'",
        "'[1.50]'",
        "'{\"a\":1e2, \"b\":\"x\\u0062\"}'",
    ] {
        cmp1(&format!("json_pretty({doc})"));
    }
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

#[test]
fn json_aggregates() {
    cmp1(
        "select json_group_array(x) from (select 1 as x union all select null union all select 2)",
    );
    cmp1("select hex(jsonb_group_array(x)) from (select 1 x union all select null union all select 2.5)");
    cmp1("select hex(jsonb_group_array(x)) from (select 'a\"b' x)");
    cmp1("select json_group_object(k, v) from (select 'a' k, null v union all select 'b', 2)");
    // (SQLite 3.46 had a bug where a NULL key produced "{:1}"; modern
    // SQLite skips the pair — pinned in turso_compat.rs instead.)
    cmp1(
        "select hex(jsonb_group_object(k, v)) from (select 'a' k, 1 v union all select 'b', 'x' v)",
    );
    cmp1("select json_group_array(x) from (select 1.5 x union all select 'a\"b')");
    cmp1("select json_group_array(x) from (select x'2b1331' x)");
    cmp1("select json_group_array(x) from (select 1 x where 0)");
    cmp1("select json_group_object('a', 1) from (select 1 where 0)");
}

// ---------------------------------------------------------------------------
// Cross-interop: each engine's JSONB decoded by the other
// ---------------------------------------------------------------------------

#[test]
fn jsonb_blob_cross_interop() {
    let docs = [
        "{\"a\":[1,2,{\"b\":null}]}",
        "[1.5,\"x\",true,null]",
        "{\"n\":-3,\"r\":2.5e3}",
        "\"héllo 😀\"",
        "1e300",
        "9223372036854775808",
    ];
    for doc in docs {
        // Engine produces the blob.
        let db = engine();
        let rows = db
            .query("select jsonb(?)", vec![Value::Text(doc.into())])
            .unwrap();
        let blob = match rows[0][0].clone() {
            Value::Blob(b) => b,
            other => panic!("engine jsonb not a blob: {other:?}"),
        };
        // Real SQLite decodes it and agrees on json().
        let con = oracle();
        let want: String = con
            .query_row("select json(?)", rusqlite::params![blob], |r| r.get(0))
            .unwrap_or_else(|e| panic!("sqlite cannot decode engine jsonb for {doc}: {e}"));
        // And SQLite's own jsonb() of the doc decodes on the engine.
        let sqlite_blob: Vec<u8> = con
            .query_row("select jsonb(?)", rusqlite::params![doc], |r| r.get(0))
            .unwrap();
        assert_eq!(blob, sqlite_blob, "jsonb bytes diverge for {doc}");
        let rows = db
            .query(
                "select json(jsonb(?))",
                vec![Value::Blob(want.as_bytes().to_vec())],
            )
            .unwrap();
        let got = match &rows[0][0] {
            Value::Text(t) => t.to_string(),
            other => panic!("engine json() of blob not text: {other:?}"),
        };
        assert_eq!(got, want, "engine json(jsonb) mismatch for {doc}");
        // json_extract over SQLite-produced JSONB.
        let rows = db
            .query(
                "select json_extract(jsonb(?), '$')",
                vec![Value::Blob(sqlite_blob)],
            )
            .unwrap();
        let _ = rows;
    }
}

// ---------------------------------------------------------------------------
// Error parity: malformed input raises on both engines
// ---------------------------------------------------------------------------

#[test]
fn error_parity() {
    // The engine's query() must FAIL on malformed JSON like SQLite does.
    let db = engine();
    for sql in [
        "select json('nope')",
        "select json_extract('[1', '$[0]')",
        "select jsonb('{')",
        "select json_object('a')",
        "select json_type('[1]', 'bad')",
        "select json_extract('[1]', '$bad')",
        "select '[1]' -> '$bad'",
        "select '[1]' -> '[-1]'",
        "select '[1]' -> ''",
    ] {
        let result = db.query(sql, ());
        assert!(result.is_err(), "engine should raise: {sql}");
    }
    // json_error_position parity.
    for doc in ["'nope'", "'[1,2'", "'{\"a\":}'", "'[1]'"] {
        cmp1(&format!("json_error_position({doc})"));
    }
}
