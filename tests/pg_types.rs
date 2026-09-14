//! PostgreSQL-borrowed static typing behaviors: NUMERIC affinity,
//! DECIMAL(p,s) scale enforcement, pg_typeof, and CAST semantics.
//!
//! Pinned against real SQLite 3.53 (via examples/probe_numeric_affinity.rs)
//! for the SQLite-visible parts: columns declared DECIMAL / NUMERIC /
//! DATE / BOOLEAN / MONEY are NUMERIC-affine (NOT blob-affine), and
//! numeric-looking TEXT coerces on write. The DECIMAL(p,s) rounding is an
//! intentional, documented divergence toward PostgreSQL semantics.

use rusqlite::Connection;
use rustqlite::{Database, Value};

fn rq() -> Database {
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

/// Cross-check our NUMERIC-affinity storage classes against real SQLite.
fn sqlite_typeof(create: &str, insert: &str) -> Vec<String> {
    let sq = Connection::open_in_memory().unwrap();
    sq.execute_batch(&format!("{create}; {insert};")).unwrap();
    let mut out = Vec::new();
    let mut stmt = sq.prepare("SELECT typeof(v) FROM t ORDER BY rowid").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
    for r in rows {
        out.push(r.unwrap());
    }
    out
}

fn our_typeof(create: &str, insert: &str) -> Vec<String> {
    let mut d = rq();
    d.execute(&format!("{create}; {insert};"), []).unwrap();
    d.query("SELECT typeof(v) FROM t ORDER BY rowid", [])
        .unwrap()
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.to_string(),
            v => panic!("expected TEXT typeof, got {v:?}"),
        })
        .collect()
}

#[test]
fn numeric_affinity_matches_sqlite() {
    for decl in ["NUMERIC", "DECIMAL", "DEC", "FIXED", "MONEY", "BOOLEAN", "DATE", "DATETIME"] {
        let create = format!("CREATE TABLE t(v {decl})");
        // numeric-looking text coerces; integer preferred
        let insert = "INSERT INTO t VALUES ('123'), ('1.5'), (5.0), ('abc'), (x'41')";
        let expected = sqlite_typeof(&create, insert);
        let got = our_typeof(&create, insert);
        assert_eq!(
            got, expected,
            "{decl}: typeof mismatch — ours {got:?} vs sqlite {expected:?}"
        );
        // the concrete expectations (also guards SQLite changes):
        assert_eq!(got, vec!["integer", "real", "integer", "text", "blob"], "{decl}");
    }
}

#[test]
fn numeric_affinity_integer_squeeze() {
    // inserting 5.0 into a NUMERIC column stores INTEGER 5 (SQLite rule)
    let mut d = rq();
    d.execute("CREATE TABLE t(v NUMERIC); INSERT INTO t VALUES (5.0)", [])
        .unwrap();
    assert_eq!(one(&d, "SELECT v FROM t"), Value::Integer(5));
    assert_eq!(text(&d, "SELECT typeof(v) FROM t"), "integer");
    // but a true REAL (1.5) stays REAL
    d.execute("INSERT INTO t VALUES (1.5)", []).unwrap();
    assert_eq!(one(&d, "SELECT v FROM t WHERE rowid = 2"), Value::Real(1.5));
    // non-numeric text keeps TEXT storage class
    d.execute("INSERT INTO t VALUES ('hello')", []).unwrap();
    assert_eq!(one(&d, "SELECT v FROM t WHERE rowid = 3"), Value::Text("hello".into()));
    // blobs are never converted by any affinity
    d.execute("INSERT INTO t VALUES (x'42')", []).unwrap();
    assert!(matches!(one(&d, "SELECT v FROM t WHERE rowid = 4"), Value::Blob(_)));
}

#[test]
fn decimal_scale_enforcement() {
    // DECIMAL(10,2): writes are ROUNDED half-away-from-zero to 2 places
    // (PG numeric behavior; SQLite ignores the spec — documented
    // intentional divergence).
    let mut d = rq();
    d.execute("CREATE TABLE prices(id INTEGER PRIMARY KEY, v DECIMAL(10,2))", [])
        .unwrap();
    d.execute(
        "INSERT INTO prices VALUES (1, 1.555), (2, 2.554), (3, -1.555), (4, 3.0), (5, '4.126')",
        [],
    )
    .unwrap();
    let v = |id: i64| one(&d, &format!("SELECT v FROM prices WHERE id = {id}"));
    assert_eq!(v(1), Value::Real(1.56), "1.555 rounds half away from zero");
    assert_eq!(v(2), Value::Real(2.55));
    assert_eq!(v(3), Value::Real(-1.56), "negative half rounds away from zero");
    assert_eq!(v(4), Value::Real(3.0), "integral value keeps scale");
    assert_eq!(v(5), Value::Real(4.13), "numeric text is coerced THEN rounded");

    // DECIMAL(5,0) / plain INTEGER-style: scale 0 stores INTEGER
    d.execute("CREATE TABLE units(v DECIMAL(5,0))", []).unwrap();
    d.execute("INSERT INTO units VALUES (2.6), (2.4)", []).unwrap();
    assert_eq!(one(&d, "SELECT v FROM units WHERE rowid = 1"), Value::Integer(3));
    assert_eq!(one(&d, "SELECT v FROM units WHERE rowid = 2"), Value::Integer(2));

    // NUMERIC(8,3) gets the same treatment via the NUMERIC alias
    d.execute("CREATE TABLE m(v NUMERIC(8,3))", []).unwrap();
    d.execute("INSERT INTO m VALUES (1.2345)", []).unwrap();
    assert_eq!(one(&d, "SELECT v FROM m"), Value::Real(1.235));

    // plain DECIMAL (no parens) is NOT enforced — SQLite semantics
    d.execute("CREATE TABLE plain(v DECIMAL)", []).unwrap();
    d.execute("INSERT INTO plain VALUES (1.55555)", []).unwrap();
    assert_eq!(one(&d, "SELECT v FROM plain"), Value::Real(1.55555));

    // UPDATE re-applies the scale
    d.execute("UPDATE prices SET v = 9.999 WHERE id = 1", []).unwrap();
    assert_eq!(one(&d, "SELECT v FROM prices WHERE id = 1"), Value::Real(10.0));
}

#[test]
fn pg_typeof_storage_classes() {
    let d = rq();
    assert_eq!(text(&d, "SELECT pg_typeof(NULL)"), "unknown");
    assert_eq!(text(&d, "SELECT pg_typeof(1)"), "integer");
    assert_eq!(text(&d, "SELECT pg_typeof(1.5)"), "double precision");
    assert_eq!(text(&d, "SELECT pg_typeof('x')"), "text");
    assert_eq!(text(&d, "SELECT pg_typeof(x'41')"), "bytea");
    // typeof() (SQLite-native) keeps its storage-class names
    assert_eq!(text(&d, "SELECT typeof(1.5)"), "real");
    assert_eq!(text(&d, "SELECT typeof(NULL)"), "null");
}

#[test]
fn cast_numeric_semantics() {
    let d = rq();
    // longest numeric prefix, CAST never squeezes REAL down to INTEGER
    assert_eq!(one(&d, "SELECT CAST('123' AS NUMERIC)"), Value::Integer(123));
    assert_eq!(one(&d, "SELECT CAST('1.5' AS NUMERIC)"), Value::Real(1.5));
    assert_eq!(one(&d, "SELECT CAST('abc' AS NUMERIC)"), Value::Integer(0));
    assert_eq!(one(&d, "SELECT CAST(12.0 AS NUMERIC)"), Value::Real(12.0));
    assert_eq!(one(&d, "SELECT CAST(12.0 AS DECIMAL)"), Value::Real(12.0));
    // trailing junk after the numeric prefix is dropped
    assert_eq!(one(&d, "SELECT CAST('123abc' AS NUMERIC)"), Value::Integer(123));
    // NULL casts to NULL
    assert_eq!(one(&d, "SELECT CAST(NULL AS NUMERIC)"), Value::Null);
}

#[test]
fn cast_numeric_sqlite_parity() {
    // CAST AS NUMERIC must be bit-identical with real SQLite (the
    // divergence is confined to DECIMAL(p,s) column writes).
    for expr in [
        "CAST('123' AS NUMERIC)",
        "CAST('1.5' AS NUMERIC)",
        "CAST('abc' AS NUMERIC)",
        "CAST(12.0 AS NUMERIC)",
        "CAST(' 42 ' AS NUMERIC)",
        "CAST(9223372036854775807 AS NUMERIC)",
        "CAST(-1 AS NUMERIC)",
    ] {
        let sq = Connection::open_in_memory().unwrap();
        let expect: rusqlite::types::Value = {
            use rusqlite::types::ValueRef;
            let mut stmt = sq.prepare(&format!("SELECT {expr}")).unwrap();
            let mut rows = stmt.query([]).unwrap();
            let v = rows.next().unwrap().unwrap().get_ref(0).unwrap();
            match v {
                ValueRef::Null => rusqlite::types::Value::Null,
                ValueRef::Integer(i) => rusqlite::types::Value::Integer(i),
                ValueRef::Real(f) => rusqlite::types::Value::Real(f),
                ValueRef::Text(t) => {
                    rusqlite::types::Value::Text(String::from_utf8_lossy(t).into_owned())
                }
                ValueRef::Blob(b) => rusqlite::types::Value::Blob(b.to_vec()),
            }
        };
        let got = one(&rq(), &format!("SELECT {expr}"));
        let same = match (&got, &expect) {
            (Value::Null, rusqlite::types::Value::Null) => true,
            (Value::Integer(a), rusqlite::types::Value::Integer(b)) => a == b,
            (Value::Real(a), rusqlite::types::Value::Real(b)) => a.to_bits() == b.to_bits(),
            (Value::Text(a), rusqlite::types::Value::Text(b)) => a.as_str() == b.as_str(),
            _ => false,
        };
        assert!(same, "{expr}: got {got:?}, sqlite {expect:?}");
    }
}

#[test]
fn boolean_column_and_cast() {
    // BOOLEAN columns are NUMERIC-affine (SQLite bucket) — values store
    // as their numeric selves; 1/0 work as true/false.
    let mut d = rq();
    d.execute(
        "CREATE TABLE flags(id INTEGER PRIMARY KEY, active BOOLEAN);
         INSERT INTO flags VALUES (1, 1), (2, 0), (3, 'true'), (4, NULL)",
        [],
    )
    .unwrap();
    let n = int(&d, "SELECT COUNT(*) FROM flags WHERE active = 1");
    assert_eq!(n, 1);
    assert_eq!(one(&d, "SELECT active FROM flags WHERE id = 4"), Value::Null);
    // pg_typeof of a numeric-affine boolean column holding 1
    assert_eq!(text(&d, "SELECT pg_typeof(active) FROM flags WHERE id = 1"), "integer");

    // CAST AS BOOLEAN is PostgreSQL-borrowed: the PG boolean literal
    // words map to 1/0; numbers map by truthiness; unrecognized text
    // falls back to the SQLite numeric-prefix cast (0).
    let d2 = rq();
    assert_eq!(one(&d2, "SELECT CAST('true' AS BOOLEAN)"), Value::Integer(1));
    assert_eq!(one(&d2, "SELECT CAST('FALSE' AS BOOLEAN)"), Value::Integer(0));
    assert_eq!(one(&d2, "SELECT CAST('t' AS BOOLEAN)"), Value::Integer(1));
    assert_eq!(one(&d2, "SELECT CAST('yes' AS BOOLEAN)"), Value::Integer(1));
    assert_eq!(one(&d2, "SELECT CAST('off' AS BOOLEAN)"), Value::Integer(0));
    assert_eq!(one(&d2, "SELECT CAST(' 1 ' AS BOOLEAN)"), Value::Integer(1));
    assert_eq!(one(&d2, "SELECT CAST(0 AS BOOLEAN)"), Value::Integer(0));
    assert_eq!(one(&d2, "SELECT CAST(2.5 AS BOOLEAN)"), Value::Integer(1));
    assert_eq!(one(&d2, "SELECT CAST('maybe' AS BOOLEAN)"), Value::Integer(0));
    assert_eq!(one(&d2, "SELECT CAST(NULL AS BOOLEAN)"), Value::Null);
    // BOOL is the PG short alias
    assert_eq!(one(&d2, "SELECT CAST('true' AS BOOL)"), Value::Integer(1));
    // truthiness flows into WHERE
    assert_eq!(int(&d2, "SELECT COUNT(*) FROM (SELECT 1 x) WHERE CAST('yes' AS BOOLEAN)"), 1);
}

#[test]
fn date_column_stays_numeric_affine() {
    // DATE columns: numeric-looking text coerces (SQLite semantics —
    // dates are NOT special-cased, matching the probe-pinned behavior).
    let mut d = rq();
    d.execute(
        "CREATE TABLE ev(v DATE);
         INSERT INTO ev VALUES ('2026-09-14'), (20260914)",
        [],
    )
    .unwrap();
    assert_eq!(one(&d, "SELECT v FROM ev WHERE rowid = 1"), Value::Text("2026-09-14".into()));
    assert_eq!(one(&d, "SELECT v FROM ev WHERE rowid = 2"), Value::Integer(20260914));
}

// small helper used above
fn int(db: &Database, sql: &str) -> i64 {
    match one(db, sql) {
        Value::Integer(i) => i,
        v => panic!("expected INTEGER from `{sql}`, got {v:?}"),
    }
}
