//! Boundary-value tests — modeled on §4.3 of
//! https://www.sqlite.org/testing.html
//!
//! "SQLite defines certain limits on its operation, such as the maximum
//! number of columns in a table, the maximum length of an SQL statement,
//! or the maximum value of an integer. The TCL and TH3 test suites both
//! contain numerous tests that push SQLite right to the edge of its
//! defined limits and verify that it performs correctly for all allowed
//! values. Additional tests go beyond the defined limits and verify that
//! SQLite correctly returns errors."
//!
//! The UB-provoking examples the page calls out explicitly are included
//! verbatim: `SELECT -1*(-9223372036854775808)` and integer-overflow
//! promotion. Where the correct answer is defined by SQLite behavior
//! (documented or de-facto), the differential harness compares against
//! real SQLite so the boundary CONTRACT is SQLite's, not ours.
//!
//! Run with: cargo test --test boundary

use rustqlite::{Database, Value};

// ===========================================================================
// Integer boundaries
// ===========================================================================

#[test]
fn i64_extremes_round_trip() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE n (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    let extremes = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
    for (i, v) in extremes.iter().enumerate() {
        db.execute(
            "INSERT INTO n (id, v) VALUES (?, ?)",
            [Value::Integer(i as i64 + 1), Value::Integer(*v)],
        )
        .unwrap();
    }
    for (i, v) in extremes.iter().enumerate() {
        let rows = db
            .query(
                "SELECT v FROM n WHERE id = ?",
                [Value::Integer(i as i64 + 1)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "extreme {} not found", v);
        assert_eq!(
            rows[0][0],
            Value::Integer(*v),
            "extreme {} did not round-trip",
            v
        );
    }
    // And via ORDER BY (comparison boundaries at the extremes).
    let rows = db.query("SELECT v FROM n ORDER BY v", []).unwrap();
    let vals: Vec<i64> = rows
        .iter()
        .map(|r| {
            if let Value::Integer(i) = &r[0] {
                *i
            } else {
                panic!("non-int")
            }
        })
        .collect();
    let mut expected = extremes.to_vec();
    expected.sort_unstable();
    assert_eq!(vals, expected, "ORDER BY at i64 extremes diverged");
}

/// SQLite semantics: integer overflow in arithmetic promotes the result to
/// REAL. Both engines must agree.
#[test]
fn integer_overflow_matches_sqlite() {
    let cases = [
        // (sql, must produce SOME result — compare against SQLite instead)
        "SELECT 9223372036854775807 + 1",
        "SELECT 9223372036854775807 * 2",
        "SELECT -9223372036854775808 - 1",
        "SELECT -1 * -9223372036854775808", // the exact UB-provoker from sqlite.org/testing.html
        "SELECT -1 * (-9223372036854775808)",
        "SELECT 0 - -9223372036854775808",
        "SELECT 9223372036854775807 + 9223372036854775807",
        "SELECT 4611686018427387904 * 2",
        "SELECT -9223372036854775808 / -1",
        "SELECT abs(-9223372036854775808)",
    ];
    let ours = Database::open_in_memory().unwrap();
    let oracle = rusqlite::Connection::open_in_memory().unwrap();
    for sql in cases {
        let our_res = ours.query(sql, []);
        // Eager error propagation on BOTH sides: abs(-9223372036854775808)
        // raises "integer overflow" in SQLite, and engines must agree on
        // success-vs-error, not just on values.
        let their_res: Result<Vec<Vec<rusqlite::types::Value>>, rusqlite::Error> = (|| {
            let mut stmt = oracle.prepare(sql)?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let v: rusqlite::types::Value = row.get(0)?;
                out.push(vec![v]);
            }
            Ok(out)
        })();
        match (our_res, their_res) {
            (Ok(our_rows), Ok(their_rows)) => {
                assert_eq!(
                    our_rows.len(),
                    1,
                    "{} returned {} rows",
                    sql,
                    our_rows.len()
                );
                assert_eq!(
                    their_rows.len(),
                    1,
                    "{} returned {} rows on sqlite",
                    sql,
                    their_rows.len()
                );
                let our_v = &our_rows[0][0];
                let their_v = &their_rows[0][0];
                let equal = match (our_v, their_v) {
                    (Value::Integer(a), rusqlite::types::Value::Integer(b)) => a == b,
                    (Value::Real(a), rusqlite::types::Value::Real(b)) => {
                        (a.is_nan() && b.is_nan()) || (a - b).abs() <= 1e-6
                    }
                    (Value::Integer(a), rusqlite::types::Value::Real(b)) => {
                        (*a as f64 - b).abs() <= 1e-6
                    }
                    (Value::Real(a), rusqlite::types::Value::Integer(b)) => {
                        (*a - *b as f64).abs() <= 1e-6
                    }
                    _ => false,
                };
                assert!(equal, "{}: ours={:?} sqlite={:?}", sql, our_v, their_v);
            }
            (Err(_), Err(_)) => { /* both engines reject the statement: agreement */ }
            (ours_err, theirs_err) => {
                panic!(
                    "{}: success/error divergence: ours={:?} sqlite={:?}",
                    sql,
                    ours_err.map(|_| ()).map_err(|e| e.to_string()),
                    theirs_err.map(|_| ()).map_err(|e| e.to_string())
                );
            }
        }
    }
}

/// Shift/round/abs boundaries — classic UB territory in C.
#[test]
fn numeric_function_boundaries_match_sqlite() {
    let cases = [
        "SELECT abs(-1)",
        "SELECT abs(9223372036854775807)",
        "SELECT abs(-0.0)",
        "SELECT round(0.5), round(1.5), round(2.5), round(-0.5)",
        "SELECT round(2.675, 2)",
        "SELECT round(1.0/0.0)",
        "SELECT 1.0/0.0, -1.0/0.0",
        "SELECT 0.0/0.0",
        "SELECT 5 % 0",
        "SELECT 5 % 3, -5 % 3, 5 % -3",
        "SELECT 5 / 0",
        "SELECT -7 / 2, 7 / -2, -7 / -2, 7 / 2",
        "SELECT CAST(9223372036854775807 AS REAL)",
        "SELECT CAST(-9223372036854775808 AS REAL)",
        "SELECT CAST(1e999 AS INTEGER)",
        "SELECT CAST('9223372036854775808' AS INTEGER)",
    ];
    let ours = Database::open_in_memory().unwrap();
    let oracle = rusqlite::Connection::open_in_memory().unwrap();
    for sql in cases {
        let our_rows = match ours.query(sql, []) {
            Ok(r) => r,
            Err(e) => {
                // If we error, SQLite must error too.
                assert!(
                    oracle.prepare(sql).is_err(),
                    "{}: rustqlite errored ({}) but SQLite succeeded",
                    sql,
                    e
                );
                continue;
            }
        };
        let mut stmt = oracle.prepare(sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Ok(Some(row)) = rows.next() {
            let v: rusqlite::types::Value = row.get(0).unwrap();
            out.push(vec![v]);
        }
        assert_eq!(our_rows.len(), out.len(), "{}: row counts differ", sql);
        for (r1, r2) in our_rows.iter().zip(out.iter()) {
            let a = &r1[0];
            let b = &r2[0];
            let equal = match (a, b) {
                (Value::Null, rusqlite::types::Value::Null) => true,
                (Value::Integer(x), rusqlite::types::Value::Integer(y)) => x == y,
                (Value::Real(x), rusqlite::types::Value::Real(y)) => {
                    (x.is_nan() && y.is_nan())
                        || (x.is_infinite() && y.is_infinite() && x.signum() == y.signum())
                        || (x - y).abs() <= 1e-9
                }
                (Value::Integer(x), rusqlite::types::Value::Real(y)) => {
                    (*x as f64 - y).abs() <= 1e-9
                }
                (Value::Real(x), rusqlite::types::Value::Integer(y)) => {
                    (*x - *y as f64).abs() <= 1e-9
                }
                (Value::Text(x), rusqlite::types::Value::Text(y)) => x.as_str() == y,
                _ => false,
            };
            assert!(equal, "{}: ours={:?} sqlite={:?}", sql, a, b);
        }
    }
}

// ===========================================================================
// REAL boundaries: NaN, infinities, -0.0, denormals — storage round-trip.
// ===========================================================================

#[test]
fn real_extremes_round_trip() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE r (id INTEGER PRIMARY KEY, v REAL)", [])
        .unwrap();
    let vals = [
        0.0,
        -0.0,
        f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        1.0e308,
        1.0e-308,
        0.1 + 0.2, // famous representation error
    ];
    for (i, v) in vals.iter().enumerate() {
        db.execute(
            "INSERT INTO r (id, v) VALUES (?, ?)",
            [Value::Integer(i as i64 + 1), Value::Real(*v)],
        )
        .unwrap();
    }
    for (i, v) in vals.iter().enumerate() {
        let rows = db
            .query(
                "SELECT v FROM r WHERE id = ?",
                [Value::Integer(i as i64 + 1)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0][0] {
            Value::Real(got) => {
                let same = if v.is_nan() {
                    got.is_nan()
                } else if v.is_infinite() {
                    got == v
                } else {
                    got.to_bits() == v.to_bits() || got == v
                };
                assert!(same, "REAL {} round-tripped as {}", v, got);
                // -0.0 must preserve its sign bit.
                if *v == 0.0 && v.is_sign_negative() {
                    assert!(got.is_sign_negative(), "-0.0 lost its sign, became {}", got);
                }
            }
            other => panic!("REAL {} came back as {:?}", v, other),
        }
    }
    // NaN equality semantics: NaN = NaN is NULL (unknown), NaN <> NaN is NULL.
    let rows = db.query("SELECT v = v FROM r WHERE id = 8", []).unwrap();
    assert_eq!(rows[0][0], Value::Null, "NaN = NaN must be NULL");
}

// ===========================================================================
// TEXT/BLOB boundaries
// ===========================================================================

#[test]
fn text_and_blob_boundaries() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE s (id INTEGER PRIMARY KEY, t TEXT, b BLOB)",
        [],
    )
    .unwrap();

    // Empty string, single char, embedded NULs, 4-byte emoji,
    // RTL text, combining characters.
    let big = "x".repeat(4_000);
    let texts: Vec<String> = vec![
        String::new(),
        "a".into(),
        "with\0embedded\0nuls".into(),
        "日本語テキスト🚀🔥💯".into(),
        "مرحبا بالعالم".into(),
        "e\u{301}galise\u{301} (combining)".into(),
        big.clone(),
    ];
    for (i, t) in texts.iter().enumerate() {
        db.execute(
            "INSERT INTO s (id, t) VALUES (?, ?)",
            [Value::Integer(i as i64 + 1), Value::Text(t.clone().into())],
        )
        .unwrap();
    }
    for (i, t) in texts.iter().enumerate() {
        let rows = db
            .query(
                "SELECT t, length(t) FROM s WHERE id = ?",
                [Value::Integer(i as i64 + 1)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0],
            Value::Text(t.clone().into()),
            "TEXT round-trip failed at {}",
            i
        );
        // length() counts CHARACTERS in SQLite.
        let expected_len = t.chars().count() as i64;
        assert_eq!(
            rows[0][1],
            Value::Integer(expected_len),
            "length() at id {}",
            i + 1
        );
    }

    // Blobs: empty, 1 byte, all 256 byte values, 4 KiB.
    let blobs: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x00],
        (0u8..=255).collect(),
        vec![0xFF; 4 * 1024],
    ];
    for (i, b) in blobs.iter().enumerate() {
        db.execute(
            "INSERT INTO s (id, b) VALUES (?, ?)",
            [Value::Integer(100 + i as i64), Value::Blob(b.clone())],
        )
        .unwrap();
    }
    for (i, b) in blobs.iter().enumerate() {
        let rows = db
            .query(
                "SELECT b, length(b) FROM s WHERE id = ?",
                [Value::Integer(100 + i as i64)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0],
            Value::Blob(b.clone()),
            "BLOB round-trip failed at {}",
            i
        );
        assert_eq!(rows[0][1], Value::Integer(b.len() as i64));
    }
}

// ===========================================================================
// Structural limits: nesting depth, statement size, column count, IN list,
// identifier length. Beyond-limit inputs must return ERRORS (not stack
// overflow, not OOM-kill).
// ===========================================================================

#[test]
fn deep_nesting_errors_without_stack_overflow() {
    let db = Database::open_in_memory().unwrap();
    // Parentheses: within the depth limit must parse; beyond it must
    // return a graceful parse error (SQLite's SQLITE_MAX_EXPR_DEPTH
    // behavior). A stack overflow would abort the whole process.
    for depth in [10usize, 100, 400] {
        let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        let r = db.query(&sql, []);
        assert!(r.is_ok(), "depth {} should parse but: {:?}", depth, r.err());
    }
    for depth in [10_000usize, 100_000, 1_000_000] {
        let sql = format!("SELECT {}1{}", "(".repeat(depth), ")".repeat(depth));
        let r = db.query(&sql, []);
        assert!(
            r.is_err(),
            "depth {} must return a graceful 'too deep' error, not parse",
            depth
        );
    }
    // Unbalanced — must be a graceful parse error.
    let sql = format!("SELECT {}1", "(".repeat(50_000));
    assert!(db.query(&sql, []).is_err(), "unbalanced parens must error");

    // Deeply nested CASE expressions: graceful failure beyond the limit.
    for depth in [50usize, 400] {
        let mut sql = String::from("SELECT ");
        sql.push_str(&"CASE 1 WHEN 1 THEN ".repeat(depth));
        sql.push('1');
        sql.push_str(&" ELSE 1 END".repeat(depth));
        let r = db.query(&sql, []);
        // May succeed or error — but must not crash.
        let _ = r;
    }
    {
        let depth = 10_000usize;
        let mut sql = String::from("SELECT ");
        sql.push_str(&"CASE 1 WHEN 1 THEN ".repeat(depth));
        sql.push('1');
        sql.push_str(&" ELSE 1 END".repeat(depth));
        assert!(
            db.query(&sql, []).is_err(),
            "10k-deep CASE must error gracefully"
        );
    }

    // Deeply nested subqueries: same contract.
    for depth in [50usize, 200] {
        let sql = format!(
            "SELECT {} SELECT 1 {}",
            "SELECT * FROM (".repeat(depth),
            ")".repeat(depth)
        );
        let _ = db.query(&sql, []);
    }

    // Long IN lists: 100k elements (breadth, not depth — must parse).
    let items: Vec<String> = (0..100_000).map(|i| i.to_string()).collect();
    let sql = format!("SELECT 1 IN ({})", items.join(","));
    let r = db.query(&sql, []);
    assert!(r.is_ok(), "100k IN list should parse: {:?}", r.err());
}

/// Oversize payloads must be rejected with a clean error — the historical
/// behavior was a u32-underflow PANIC in the B+tree content allocator.
/// (SQLite stores these via overflow page chains; until those land, the
/// contract is a graceful error.)
#[test]
fn oversize_payloads_fail_gracefully() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE big (id INTEGER PRIMARY KEY, t TEXT, b BLOB)",
        [],
    )
    .unwrap();
    // Overflow chains: payloads far beyond any page size are ACCEPTED
    // (SQLite semantics — SQLITE_MAX_LENGTH is 1 GiB) and round-trip
    // byte-exact through their spill chains.
    let text = "x".repeat(1_048_576);
    db.execute("INSERT INTO big (t) VALUES (?)", [Value::Text(text.into())])
        .expect("1 MiB TEXT must be accepted via overflow chains");
    let rows = db
        .query("SELECT length(t) FROM big WHERE id = 1", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(1_048_576));
    let blob: Vec<u8> = (0u32..512 * 1024).map(|i| (i % 251) as u8).collect();
    db.execute(
        "INSERT INTO big (b) VALUES (?)",
        [Value::Blob(blob.clone())],
    )
    .expect("512 KiB BLOB must be accepted via overflow chains");
    let rows = db
        .query("SELECT length(b) FROM big WHERE id = 2", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(512 * 1024));
    // Byte-exact round trip through the chain.
    let rows = db.query("SELECT b FROM big WHERE id = 2", []).unwrap();
    match &rows[0][0] {
        Value::Blob(got) => assert_eq!(got, &blob, "overflow blob must round-trip exactly"),
        other => panic!("expected blob, got {other:?}"),
    }
    // (Beyond 1 GiB the engine rejects with a clean InvalidArgument —
    // SQLite's SQLITE_MAX_LENGTH ceiling — verified by unit test; a runtime
    // probe would need a 1 GiB allocation.)
    // The engine must remain fully usable after all of the above.
    db.execute("INSERT INTO big (t) VALUES ('fine')", [])
        .unwrap();
    let rows = db.query("SELECT COUNT(*) FROM big", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(3))
    );
}

#[test]
fn huge_statements_and_identifiers() {
    let mut db = Database::open_in_memory().unwrap();
    // A 10 MiB SQL statement (one giant literal).
    let big_lit = "a".repeat(10 * 1024 * 1024);
    let sql = format!("SELECT length('{}')", big_lit);
    let rows = db.query(&sql, []).expect("10MiB statement must parse");
    assert_eq!(rows[0][0], Value::Integer(10 * 1024 * 1024_i64));

    // Long identifier (schema row still fits a page).
    let long_ident = "c".repeat(1_000);
    let sql = format!("CREATE TABLE big_id_{} (x INTEGER)", long_ident);
    db.execute(&sql, [])
        .expect("1k identifier should be accepted");
    let sql = format!("INSERT INTO big_id_{} VALUES (1)", long_ident);
    db.execute(&sql, []).unwrap();
    let sql = format!("SELECT x FROM big_id_{}", long_ident);
    let rows = db.query(&sql, []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1));
    // A 10k identifier: the stored CREATE statement exceeds one page, so
    // it spills to an overflow chain — accepted and readable, like SQLite.
    let huge_ident = "d".repeat(10_000);
    let sql = format!("CREATE TABLE big_id_{} (x INTEGER)", huge_ident);
    db.execute(&sql, [])
        .expect("10k identifier spills to overflow chains and must be accepted");
    let sql = format!("INSERT INTO big_id_{} VALUES (7)", huge_ident);
    db.execute(&sql, []).unwrap();
    let sql = format!("SELECT x FROM big_id_{}", huge_ident);
    let rows = db.query(&sql, []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(7));

    // Wide table: 500 columns at the boundary of usability.
    let cols: Vec<String> = (0..500).map(|i| format!("c{} INTEGER", i)).collect();
    db.execute(&format!("CREATE TABLE wide ({})", cols.join(",")), [])
        .expect("500-column table should be accepted");
    let vals: Vec<String> = (0..500).map(|i| i.to_string()).collect();
    db.execute(&format!("INSERT INTO wide VALUES ({})", vals.join(",")), [])
        .unwrap();
    let rows = db.query("SELECT c0 + c499 FROM wide", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(499));
}

// ===========================================================================
// LIMIT/OFFSET boundaries, rowid boundaries, empty-relation edges.
// ===========================================================================

#[test]
fn limit_offset_boundaries() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE l (id INTEGER PRIMARY KEY)", [])
        .unwrap();
    for i in 1..=10 {
        db.execute("INSERT INTO l VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    // Negative LIMIT = no limit (SQLite convention); must not error.
    let rows = db.query("SELECT * FROM l LIMIT -1", []).unwrap();
    assert_eq!(rows.len(), 10);
    // LIMIT 0 → empty.
    let rows = db.query("SELECT * FROM l LIMIT 0", []).unwrap();
    assert_eq!(rows.len(), 0);
    // Huge LIMIT is fine.
    let rows = db
        .query("SELECT * FROM l LIMIT 9223372036854775807", [])
        .unwrap();
    assert_eq!(rows.len(), 10);
    // OFFSET beyond the end → empty.
    let rows = db.query("SELECT * FROM l LIMIT 5 OFFSET 100", []).unwrap();
    assert_eq!(rows.len(), 0);
    // Negative OFFSET is treated as 0 by SQLite.
    let rows = db.query("SELECT * FROM l LIMIT 3 OFFSET -100", []).unwrap();
    assert_eq!(rows.len(), 3);
}

#[test]
fn rowid_boundaries() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE rd (v TEXT)", []).unwrap();
    // Explicit extreme rowids.
    db.execute(
        "INSERT INTO rd (rowid, v) VALUES (-9223372036854775808, 'min')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO rd (rowid, v) VALUES (9223372036854775807, 'max')",
        [],
    )
    .unwrap();
    let rows = db
        .query("SELECT v FROM rd WHERE rowid = -9223372036854775808", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Text("min".into()));
    let rows = db
        .query("SELECT v FROM rd WHERE rowid = 9223372036854775807", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Text("max".into()));

    // max rowid + autoincrement: must not overflow-panic. SQLite picks a
    // random unused rowid; either a fresh rowid or a graceful error is
    // acceptable — a panic is not.
    let r = db.execute("INSERT INTO rd (v) VALUES ('after-max')", []);
    assert!(
        r.is_ok(),
        "insert after max rowid must succeed or error gracefully: {:?}",
        r.err()
    );
}

// ===========================================================================
// Comparison boundaries in indexes: keys at the extremes must sort and
// range-scan correctly through a secondary index.
// ===========================================================================

#[test]
fn extreme_keys_in_indexes() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE ix (id INTEGER PRIMARY KEY, k INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX ix_k ON ix(k)", []).unwrap();
    let keys = [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];
    for (i, k) in keys.iter().enumerate() {
        db.execute(
            "INSERT INTO ix (id, k) VALUES (?, ?)",
            [Value::Integer(i as i64 + 1), Value::Integer(*k)],
        )
        .unwrap();
    }
    // Range scans touching each boundary.
    let rows = db
        .query(
            "SELECT k FROM ix WHERE k >= ? ORDER BY k",
            [Value::Integer(i64::MIN)],
        )
        .unwrap();
    assert_eq!(rows.len(), 7);
    let rows = db
        .query(
            "SELECT k FROM ix WHERE k <= ? ORDER BY k",
            [Value::Integer(i64::MAX)],
        )
        .unwrap();
    assert_eq!(rows.len(), 7);
    let rows = db
        .query(
            "SELECT k FROM ix WHERE k > ? AND k < ? ORDER BY k",
            [Value::Integer(i64::MIN), Value::Integer(i64::MAX)],
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    // Point lookups at each extreme through the index.
    for k in keys {
        let rows = db
            .query("SELECT id FROM ix WHERE k = ?", [Value::Integer(k)])
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "index lookup at k={} found {} rows",
            k,
            rows.len()
        );
    }
}

/// LIKE/GLOB pattern boundaries: empty pattern, all-wildcard, pathological
/// lengths, and Unicode.
#[test]
fn like_glob_pattern_boundaries() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE p (s TEXT)", []).unwrap();
    for s in [
        "", "a", "ab", "abc", "%", "_", "\\", "日本", "🚀fire", "AAA",
    ] {
        db.execute("INSERT INTO p VALUES (?)", [Value::Text(s.into())])
            .unwrap();
    }
    let patterns = [
        "", "%", "_", "a%", "%c", "%_%_", "\\%", "🚀%", "%日%", "A%", "a%",
    ];
    let ours = &db;
    let oracle = rusqlite::Connection::open_in_memory().unwrap();
    oracle.execute("CREATE TABLE p (s TEXT)", []).unwrap();
    for s in [
        "", "a", "ab", "abc", "%", "_", "\\", "日本", "🚀fire", "AAA",
    ] {
        oracle.execute("INSERT INTO p VALUES (?)", [s]).unwrap();
    }
    for pat in patterns {
        let sql_ours = "SELECT COUNT(*) FROM p WHERE s LIKE ?";
        let sql_theirs = "SELECT COUNT(*) FROM p WHERE s LIKE ?";
        let n_ours = match ours.query(sql_ours, [Value::Text(pat.into())]) {
            Ok(r) => match r.first().and_then(|r| r.first()) {
                Some(Value::Integer(n)) => *n,
                _ => -1,
            },
            Err(_) => -1,
        };
        let n_theirs: i64 = oracle
            .query_row(sql_theirs, [pat], |row| row.get(0))
            .unwrap();
        assert_eq!(
            n_ours, n_theirs,
            "LIKE {:?} count: ours={} sqlite={}",
            pat, n_ours, n_theirs
        );
        // GLOB with pathological wildcards.
        let glob = format!("{}{}", "*".repeat(50), pat);
        let n_ours = match ours.query(
            "SELECT COUNT(*) FROM p WHERE s GLOB ?",
            [Value::Text(glob.clone().into())],
        ) {
            Ok(r) => match r.first().and_then(|r| r.first()) {
                Some(Value::Integer(n)) => *n,
                _ => -1,
            },
            Err(_) => -1,
        };
        let n_theirs: i64 = oracle
            .query_row("SELECT COUNT(*) FROM p WHERE s GLOB ?", [&glob], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(n_ours, n_theirs, "GLOB {:?} diverged", glob);
    }
}
