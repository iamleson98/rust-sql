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
