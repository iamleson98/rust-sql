//! Turso COMPAT.md feature audit — the SQL-surface battery.
//!
//! Each test exercises a feature Turso's compatibility document
//! (tursodatabase/limbo COMPAT.md) lists as supported, asserting the
//! SQLite-documented behavior. New coverage should land here first, then
//! tighten.

use rustqlite::{Database, Value};

fn q1(db: &Database, sql: &str) -> Value {
    let rows = db.query(sql, ()).expect(sql);
    rows.first()
        .and_then(|r| r.first().cloned())
        .unwrap_or(Value::Null)
}

fn qi(db: &Database, sql: &str) -> i64 {
    match q1(db, sql) {
        Value::Integer(i) => i,
        v => panic!("{sql}: expected integer, got {v:?}"),
    }
}

fn qt(db: &Database, sql: &str) -> String {
    match q1(db, sql) {
        Value::Text(t) => t.as_str().to_string(),
        v => panic!("{sql}: expected text, got {v:?}"),
    }
}

#[test]
fn scalar_concat_family() {
    let db = Database::open_in_memory().unwrap();
    // concat: NULLs skipped.
    assert_eq!(qt(&db, "SELECT concat('a', NULL, 'b', 1, 2.5)"), "ab12.5");
    assert_eq!(qt(&db, "SELECT concat()"), "");
    assert_eq!(qt(&db, "SELECT concat(NULL)"), "");
    // concat_ws: NULL separator -> NULL.
    assert!(q1(&db, "SELECT concat_ws(NULL, 'a', 'b')").is_null());
    assert_eq!(qt(&db, "SELECT concat_ws('-', 'a', NULL, 'b')"), "a-b");
    assert_eq!(qt(&db, "SELECT concat_ws(',', 'x')"), "x");
}

#[test]
fn scalar_glob_and_octet_length() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(qi(&db, "SELECT glob('he*', 'hello')"), 1);
    assert_eq!(qi(&db, "SELECT glob('he*', 'bye')"), 0);
    // NULL propagation.
    assert!(q1(&db, "SELECT glob(NULL, 'x')").is_null());
    // octet_length: bytes.
    assert_eq!(qi(&db, "SELECT octet_length('héllo')"), 6); // 2-byte é
    assert_eq!(qi(&db, "SELECT octet_length(x'01020304')"), 4);
    assert_eq!(qi(&db, "SELECT octet_length(12345)"), 5);
    assert!(q1(&db, "SELECT octet_length(NULL)").is_null());
}

#[test]
fn sqlite_identity_functions() {
    let db = Database::open_in_memory().unwrap();
    let v = qt(&db, "SELECT sqlite_version()");
    assert!(!v.is_empty());
    let sid = qt(&db, "SELECT sqlite_source_id()");
    // SQLite's source_id format: "YYYY-MM-DD HH:MM:SS <hash>".
    assert_eq!(sid.len(), 19 + 1 + 40, "source_id shape: {sid}");
    assert!(sid.starts_with("20"));
}

#[test]
fn json_pretty_and_error_position() {
    let db = Database::open_in_memory().unwrap();
    let pretty = qt(&db, "SELECT json_pretty('{\"a\":[1,2],\"b\":{}}')");
    assert!(pretty.contains("\"a\": ["));
    assert!(pretty.contains("\n"));
    // Empty containers stay inline.
    assert!(pretty.contains("\"b\": {}"));
    // Valid JSON -> position 0.
    assert_eq!(qi(&db, "SELECT json_error_position('{\"a\":1}')"), 0);
    // Broken JSON -> nonzero position.
    assert!(qi(&db, "SELECT json_error_position('{\"a\":')") > 0);
    assert!(qi(&db, "SELECT json_error_position('x')") > 0);
}

#[test]
fn percentile_aggregates() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t(v)", ()).unwrap();
    for v in [1.0, 2.0, 3.0, 4.0, 10.0] {
        db.execute("INSERT INTO t VALUES (?)", vec![Value::Real(v)])
            .unwrap();
    }
    // median (odd count): 3.0
    assert_eq!(q1(&db, "SELECT median(v) FROM t"), Value::Real(3.0));
    // median (even count): mean of the middle two (3, 4 of [1,2,3,4,5,10]).
    db.execute("INSERT INTO t VALUES (?)", vec![Value::Real(5.0)])
        .unwrap();
    assert_eq!(q1(&db, "SELECT median(v) FROM t"), Value::Real(3.5));
    // percentile_cont 50 == median.
    assert_eq!(
        q1(&db, "SELECT percentile_cont(v, 50) FROM t"),
        Value::Real(3.5)
    );
    // percentile_cont 0 / 100 == min / max.
    assert_eq!(
        q1(&db, "SELECT percentile_cont(v, 0) FROM t"),
        Value::Real(1.0)
    );
    assert_eq!(
        q1(&db, "SELECT percentile_cont(v, 100) FROM t"),
        Value::Real(10.0)
    );
    // Interpolation: 25th of [1,2,3,4,5,10] -> rank 1.25 -> 2.25.
    let p25 = q1(&db, "SELECT percentile_cont(v, 25) FROM t");
    assert!(
        matches!(p25, Value::Real(r) if (r - 2.25).abs() < 1e-9),
        "got {p25:?}"
    );
    // percentile_disc: first value at/above the percentile.
    assert_eq!(
        q1(&db, "SELECT percentile_disc(v, 25) FROM t"),
        Value::Real(3.0)
    );
    assert_eq!(
        q1(&db, "SELECT percentile_disc(v, 0) FROM t"),
        Value::Real(1.0)
    );
    // stddev (sample) of [1,2,3,4,5,10]: mean=4.17, sample stddev.
    let sd = q1(&db, "SELECT stddev(v) FROM t");
    let expect = {
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0, 10.0];
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
    };
    assert!(
        matches!(sd, Value::Real(r) if (r - expect).abs() < 1e-9),
        "got {sd:?}"
    );
    // stddev_pop.
    let sdp = q1(&db, "SELECT stddev_pop(v) FROM t");
    let expect_pop = {
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0, 10.0];
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt()
    };
    assert!(matches!(sdp, Value::Real(r) if (r - expect_pop).abs() < 1e-9));
    // NULLs are skipped; all-NULL -> NULL.
    db.execute("INSERT INTO t VALUES (NULL)", ()).unwrap();
    assert_eq!(q1(&db, "SELECT median(v) FROM t"), Value::Real(3.5));
    db.execute("CREATE TABLE n(x)", ()).unwrap();
    db.execute("INSERT INTO n VALUES (NULL), (NULL)", ())
        .unwrap();
    assert!(q1(&db, "SELECT median(x) FROM n").is_null());
    assert!(q1(&db, "SELECT stddev(x) FROM n").is_null());
    // GROUP BY splits per group.
    db.execute("CREATE TABLE g(k, v)", ()).unwrap();
    for (k, v) in [
        ("a", 1.0),
        ("a", 3.0),
        ("b", 10.0),
        ("b", 20.0),
        ("b", 30.0),
    ] {
        db.execute(
            "INSERT INTO g VALUES (?, ?)",
            vec![Value::Text(k.into()), Value::Real(v)],
        )
        .unwrap();
    }
    let rows = db
        .query("SELECT k, median(v) FROM g GROUP BY k ORDER BY k", ())
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Real(2.0));
    assert_eq!(rows[1][1], Value::Real(20.0));
}

// ---------------------------------------------------------------------------
// SQLite 3.45+ JSONB family + 3.49+ unistr + soundex — pinned against
// SQLite 3.53.1 (the newer behaviors the bundled rusqlite 3.46 oracle in
// jsonb_differential.rs cannot see: negative -> indices, big-int text
// preservation, unistr, soundex).
// ---------------------------------------------------------------------------

#[test]
fn jsonb_family_pinned() {
    let db = Database::open_in_memory().unwrap();
    // jsonb(): shortest-form headers, byte-identical to SQLite.
    assert_eq!(qt(&db, "SELECT hex(jsonb('null'))"), "00");
    assert_eq!(qt(&db, "SELECT hex(jsonb('true'))"), "01");
    assert_eq!(qt(&db, "SELECT hex(jsonb('1'))"), "1331");
    assert_eq!(qt(&db, "SELECT hex(jsonb('1e2'))"), "35316532");
    assert_eq!(qt(&db, "SELECT hex(jsonb('[.5]'))"), "3B262E35");
    assert_eq!(qt(&db, "SELECT hex(jsonb('[0x10]'))"), "5B4430783130");
    assert_eq!(qt(&db, "SELECT hex(jsonb('[Infinity]'))"), "6B553965393939");
    // String size classes: inline 0-11, then 1-byte extended at 12.
    assert_eq!(
        qt(&db, "SELECT hex(jsonb('\"aaaaaaaaaa\"'))"),
        "A761616161616161616161"
    );
    assert_eq!(
        qt(&db, "SELECT hex(jsonb('\"aaaaaaaaaaaa\"'))"),
        "C70C616161616161616161616161"
    );
    // 3.47+ behavior: big integers keep their TEXT (no REAL coercion).
    assert_eq!(
        qt(&db, "SELECT hex(jsonb('9223372036854775808'))"),
        "C31339323233333732303336383534373735383038"
    );
    // jsonb() of a valid JSONB blob is the identity.
    assert_eq!(qt(&db, "SELECT hex(jsonb(jsonb('[1]')))"), "2B1331");
    // Non-shortest input headers round-trip verbatim.
    assert_eq!(qt(&db, "SELECT hex(jsonb(x'C30131'))"), "C30131");
    // jsonb_extract: containers → blob, scalars → SQL values.
    assert_eq!(
        qt(&db, "SELECT hex(jsonb_extract('{\"a\":[1,2]}', '$.a'))"),
        "4B13311332"
    );
    assert_eq!(qi(&db, "SELECT jsonb_extract('[2]', '$[0]')"), 2);
    // jsonb_set: path-origin keys and SQL-string values are TEXTRAW.
    assert_eq!(
        qt(&db, "SELECT hex(jsonb_set('{\"a\":1}', '$.b', 2))"),
        "8C176113311A621332"
    );
    // JSONB blobs flow through the text functions.
    assert_eq!(qt(&db, "SELECT json(jsonb('[1e2]'))"), "[1e2]");
    assert_eq!(qt(&db, "SELECT json_quote(jsonb('1.5'))"), "1.5");
    // json_valid flags: bit 1 = JSON, bit 2 = JSON5, bits 4|8 = JSONB.
    assert_eq!(qi(&db, "SELECT json_valid('[1]', 1)"), 1);
    assert_eq!(qi(&db, "SELECT json_valid('[+1]', 1)"), 0);
    assert_eq!(qi(&db, "SELECT json_valid('[+1]', 2)"), 1);
    assert_eq!(qi(&db, "SELECT json_valid(jsonb('[1]'), 4)"), 1);
    assert_eq!(qi(&db, "SELECT json_valid('[1]', 4)"), 0);
    // Malformed input raises (SQLite errors, it does not return NULL).
    assert!(db.query("SELECT json('nope')", ()).is_err());
    assert!(db.query("SELECT jsonb('{')", ()).is_err());
}

#[test]
fn json_arrow_operators_pinned() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(qt(&db, "SELECT '{\"a\":2}' -> '$.a'"), "2");
    assert_eq!(qi(&db, "SELECT '{\"a\":2}' ->> '$.a'"), 2);
    assert_eq!(qt(&db, "SELECT '{\"a\":\"x\"}' ->> '$.a'"), "x");
    // Chaining, field shorthand, index forms.
    assert_eq!(qi(&db, "SELECT '{\"a\":{\"b\":2}}' -> '$.a' ->> 'b'"), 2);
    assert_eq!(qi(&db, "SELECT jsonb('{\"a\":5}') ->> 'a'"), 5);
    assert_eq!(qi(&db, "SELECT '[10,20]' ->> 1"), 20);
    assert_eq!(qi(&db, "SELECT '[10,20]' ->> '[1]'"), 20);
    // 3.47+ semantics: negative integers index from the end.
    assert_eq!(qi(&db, "SELECT '[10,20]' ->> -1"), 20);
    // A text 'a.b' is a single literal key, not a nested path.
    assert!(q1(&db, "SELECT '{\"a\":{\"b\":1}}' -> 'a.b'").is_null());
    // -> returns the JSON text (strings stay quoted), ->> the SQL value.
    assert_eq!(qt(&db, "SELECT '\"str\"' -> '$'"), "\"str\"");
    assert_eq!(qt(&db, "SELECT '\"str\"' ->> '$'"), "str");
    // -> binds tighter than every binary operator but looser than unary.
    assert_eq!(qi(&db, "SELECT '[1]' -> 0 + 1"), 2);
    assert_eq!(qi(&db, "SELECT '[1]' -> 0 * 5"), 5);
    assert!(q1(&db, "SELECT -'[1]' -> 0").is_null());
    // Raw text flows through ->.
    assert_eq!(qt(&db, "SELECT '1e2' -> '$'"), "1e2");
    assert_eq!(q1(&db, "SELECT '1e2' ->> '$'"), Value::Real(100.0));
    // Bad paths raise.
    assert!(db.query("SELECT '[1]' -> '$bad'", ()).is_err());
    assert!(db.query("SELECT '[1]' -> '[-1]'", ()).is_err());
}

#[test]
fn unistr_and_soundex() {
    let db = Database::open_in_memory().unwrap();
    // unistr: \uXXXX decoding with surrogate pairs; other backslashes
    // pass through; malformed \u sequences raise.
    assert_eq!(qt(&db, "SELECT unistr('a\\u0062c')"), "abc");
    assert_eq!(qt(&db, "SELECT unistr('\\u00e9')"), "\u{e9}");
    // `\\` is an escaped backslash (one backslash out).
    assert_eq!(qt(&db, "SELECT unistr('a\\\\b')"), "a\\b");
    assert!(q1(&db, "SELECT unistr(NULL)").is_null());
    assert!(db.query("SELECT unistr('a\\qb')", ()).is_err());
    // unistr_quote: a plain literal when nothing needs escaping, else
    // wrapped in unistr('...') with \uXXXX escapes.
    assert_eq!(qt(&db, "SELECT unistr_quote('héllo')"), "'héllo'");
    assert_eq!(qt(&db, "SELECT unistr_quote('it''s')"), "'it''s'");
    assert_eq!(
        qt(&db, "SELECT unistr_quote('a\tb')"),
        "unistr('a\\u0009b')"
    );
    // soundex (classic algorithm).
    assert_eq!(qt(&db, "SELECT soundex('hello')"), "H400");
    assert_eq!(qt(&db, "SELECT soundex('Tymczak')"), "T522");
    assert_eq!(qt(&db, "SELECT soundex('')"), "?000");
    assert!(q1(&db, "SELECT soundex(NULL)").is_null());
    // sqlite_compileoption_get/used.
    assert!(qt(&db, "SELECT sqlite_compileoption_get(0)").starts_with("ATOMIC"));
    assert_eq!(
        qi(&db, "SELECT sqlite_compileoption_used('ENABLE_FTS5')"),
        1
    );
    assert_eq!(qi(&db, "SELECT sqlite_compileoption_used('NOPE')"), 0);
}

#[test]
fn json_aggregate_null_semantics() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE g(k TEXT, x)", ()).unwrap();
    for (k, x) in [
        (Some("a"), Value::Integer(1)),
        (Some("a"), Value::Null),
        (Some("a"), Value::Real(2.5)),
        (None, Value::Integer(9)),
    ] {
        let kv = k.map(|s| Value::Text(s.into())).unwrap_or(Value::Null);
        db.execute("INSERT INTO g VALUES (?, ?)", vec![kv, x])
            .unwrap();
    }
    // NULLs are INCLUDED in json_group_array ([1,null,2.5,9]).
    assert_eq!(
        qt(&db, "SELECT json_group_array(x) FROM g"),
        "[1,null,2.5,9]"
    );
    // A NULL VALUE still emits `"k":null`; a NULL KEY skips the pair
    // (modern SQLite; 3.46 produced "{:1}" — fixed upstream).
    assert_eq!(
        qt(
            &db,
            "SELECT json_group_object(k, x) FROM (SELECT k, x FROM g)"
        ),
        "{\"a\":1,\"a\":null,\"a\":2.5}"
    );
    // jsonb_group_array: same rows as raw JSONB elements.
    assert_eq!(
        qt(
            &db,
            "SELECT hex(jsonb_group_array(x)) FROM (SELECT x FROM g WHERE k = 'a')"
        ),
        "7B13310035322E35"
    );
}
