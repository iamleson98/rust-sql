//! Rowid-seek boundary semantics — differential against real SQLite.
//!
//! SQLite's rowid access paths use THREE different numeric conversions,
//! and each is pinned here:
//!
//! 1. OP_SeekRowid (rowid equality / IN / join-on-alias / rowid
//!    INSERT): a REAL converts through sqlite3VdbeIntegerAffinity's
//!    ticket-#3922 gate — RealToI64 round-trip EXACT and STRICTLY inside
//!    the i64 domain. `rowid = -9.2233720368547758e18` matches NOTHING
//!    even though the scalar compare says equal; numeric TEXT goes
//!    through Atoi64-first then the same gates ('-9223372036854775808'
//!    DOES seek, '-9.2233720368547758e18' does not).
//! 2. OP_SeekGE/LT/LE/GT (rowid ranges): iKey = sqlite3RealToI64 (the
//!    CLAMPING cast) with the int/float direction adjustment —
//!    `rowid >= 9.2233720368547758e18` is EMPTY (adjust GE→GT at
//!    iKey=i64::MAX) while `rowid <= …` includes the i64::MAX row.
//! 3. Index equality (a NON-alias indexed INTEGER column): the general
//!    EXACT int/float comparison — `plain.id = -2^63.0` DOES match an
//!    i64::MIN row. The rowid seek and the index equality disagree at
//!    the boundary, by design; these tests pin both.
//!
//! Joins: SQLite plans `A JOIN B ON A.x = B.id` (B.id = INTEGER PRIMARY
//! KEY) as SCAN A + per-row SeekRowid — the join key takes the SEEK
//! conversion, not the comparison. An indexed non-alias column takes
//! the comparison. LEFT/RIGHT variants exercise which side is seeked.

use rusqlite::Connection;
use rustqlite::{Database, Value};

fn engine_rows(db: &mut Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let head = sql.trim_start().to_ascii_uppercase();
    if head.starts_with("SELECT") {
        db.query(sql, [])
            .map(|r| r.into_iter().collect())
            .map_err(|e| e.to_string())
    } else {
        db.execute(sql, [])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string())
    }
}

fn sqlite_rows(rc: &Connection, sql: &str) -> Result<Vec<Vec<Sv>>, String> {
    let mut stmt = rc.prepare(sql).map_err(|e| e.to_string())?;
    let ncols = stmt.column_count();
    if ncols == 0 {
        return stmt
            .execute([])
            .map(|_| Vec::new())
            .map_err(|e| e.to_string());
    }
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let mut r = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    r.push(row.get(i).unwrap_or(Sv::Null));
                }
                out.push(r);
            }
            Ok(None) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

fn values_match(a: &Value, b: &Sv) -> bool {
    match (a, b) {
        (Value::Null, Sv::Null) => true,
        (Value::Null, _) | (_, Sv::Null) => false,
        (Value::Integer(x), Sv::Integer(y)) => x == y,
        (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() <= 1e-9 * y.abs().max(1.0),
        (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        (Value::Real(x), Sv::Real(y)) => {
            (x.is_nan() && y.is_nan())
                || x == y
                || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Text(x), Sv::Text(y)) => x.as_str() == y,
        (Value::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

fn check(db: &mut Database, rc: &Connection, sql: &str) {
    let ours = engine_rows(db, sql);
    let theirs = sqlite_rows(rc, sql);
    match (&ours, &theirs) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a.len(),
                b.len(),
                "[{sql}] row count: ours {} vs sqlite {}",
                a.len(),
                b.len()
            );
            for (i, (r1, r2)) in a.iter().zip(b.iter()).enumerate() {
                for (j, (v1, v2)) in r1.iter().zip(r2.iter()).enumerate() {
                    assert!(
                        values_match(v1, v2),
                        "[{sql}] row {i} col {j}: ours {v1:?} vs sqlite {v2:?}"
                    );
                }
            }
        }
        (Err(a), Err(_)) => {
            // Error parity: SQLite's exact message for the rowid mismatch
            // class is "datatype mismatch" — assert ours matches it when
            // SQLite rejected the statement.
            assert!(
                a.contains("datatype mismatch"),
                "[{sql}] our error should be SQLite's 'datatype mismatch', got: {a}"
            );
        }
        (a, b) => panic!("[{sql}] error parity: ours {a:?} vs sqlite {b:?}"),
    }
}

fn fresh() -> (Database, Connection) {
    (
        Database::open_in_memory().unwrap(),
        Connection::open_in_memory().unwrap(),
    )
}

fn setup_both(db: &mut Database, rc: &Connection, stmts: &[&str]) {
    for s in stmts {
        engine_rows(db, s).unwrap_or_else(|e| panic!("[{s}] engine setup failed: {e}"));
        sqlite_rows(rc, s).unwrap_or_else(|e| panic!("[{s}] sqlite setup failed: {e}"));
    }
}

#[test]
fn rowid_insert_boundary_values() {
    let inserts = [
        "INSERT INTO t (rowid) VALUES (-9.2233720368547758e18)",
        "INSERT INTO t (rowid) VALUES (9.2233720368547758e18)",
        "INSERT INTO t (rowid) VALUES (9223372036854774784.0)",
        "INSERT INTO t (rowid) VALUES (-9223372036854774784.0)",
        "INSERT INTO t (rowid) VALUES (9223372036854777856.0)",
        "INSERT INTO t (rowid) VALUES ('-9223372036854775808')",
        "INSERT INTO t (rowid) VALUES ('9223372036854775807')",
        "INSERT INTO t (rowid) VALUES ('-9.2233720368547758e18')",
        "INSERT INTO t (rowid) VALUES (5.0)",
        "INSERT INTO t (rowid) VALUES ('5')",
        "INSERT INTO t (rowid) VALUES (5.5)",
        "INSERT INTO t (rowid) VALUES (x'00')",
    ];
    for sql in inserts {
        let (mut db, rc) = fresh();
        setup_both(&mut db, &rc, &["CREATE TABLE t (v TEXT)"]);
        check(&mut db, &rc, sql);
        check(&mut db, &rc, "SELECT rowid, v FROM t ORDER BY rowid");
    }
}

#[test]
fn rowid_alias_insert_boundary_values() {
    // The alias column path: coercion + OP_MustBeInt.
    let inserts = [
        "INSERT INTO t (id) VALUES (-9.2233720368547758e18)",
        "INSERT INTO t (id) VALUES (9.2233720368547758e18)",
        "INSERT INTO t (id) VALUES ('-9223372036854775808')",
        "INSERT INTO t (id) VALUES ('9223372036854775807')",
        "INSERT INTO t (id) VALUES (5.0)",
        "INSERT INTO t (id) VALUES ('5')",
        "INSERT INTO t (id) VALUES (5.5)",
        "INSERT INTO t (id) VALUES (NULL)", // auto-assign on INSERT
        "INSERT INTO t (id) VALUES (9223372036854774784.0)",
    ];
    for sql in inserts {
        let (mut db, rc) = fresh();
        setup_both(
            &mut db,
            &rc,
            &["CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"],
        );
        check(&mut db, &rc, sql);
        check(&mut db, &rc, "SELECT id, v FROM t ORDER BY id");
    }
}

#[test]
fn rowid_equality_boundary_seeks() {
    let (mut db, rc) = fresh();
    setup_both(
        &mut db,
        &rc,
        &[
            "CREATE TABLE t2 (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t2 (id) VALUES (-9223372036854775808)",
            "INSERT INTO t2 (id) VALUES (9223372036854775807)",
            "INSERT INTO t2 (id) VALUES (9223372036854774784)",
            "INSERT INTO t2 (id) VALUES (-9223372036854774784)",
            "INSERT INTO t2 (id) VALUES (5)",
        ],
    );
    let queries = [
        // Boundary REALs seek NOTHING (ticket #3922) — despite the scalar
        // comparison saying equal.
        "SELECT count(*) FROM t2 WHERE id = -9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id = 9.2233720368547758e18",
        // In-range integral REALs and numeric text DO seek.
        "SELECT count(*) FROM t2 WHERE id = 5.0",
        "SELECT count(*) FROM t2 WHERE id = '5'",
        "SELECT count(*) FROM t2 WHERE id = 9223372036854774784.0",
        "SELECT count(*) FROM t2 WHERE id = -9223372036854774784.0",
        // Full-integer TEXT parses through Atoi64 (boundary rowids seek).
        "SELECT count(*) FROM t2 WHERE id = '-9223372036854775808'",
        "SELECT count(*) FROM t2 WHERE id = '9223372036854775807'",
        // Real-e-notation text goes through the affinity gates: boundary
        // never converts.
        "SELECT count(*) FROM t2 WHERE id = '-9.2233720368547758e18'",
        "SELECT count(*) FROM t2 WHERE id = '9.2233720368547758e18'",
        // Fractional / out-of-domain: nothing.
        "SELECT count(*) FROM t2 WHERE id = 5.5",
        "SELECT count(*) FROM t2 WHERE id = 9223372036854777856.0",
        "SELECT count(*) FROM t2 WHERE id = 'abc'",
        "SELECT count(*) FROM t2 WHERE id = x'00'",
        // IN lists fold every member through the seek conversion.
        "SELECT count(*) FROM t2 WHERE id IN (-9.2233720368547758e18, 9.2233720368547758e18, 5.0, 5.5, '5')",
        "SELECT count(*) FROM t2 WHERE id IN ('-9223372036854775808', '9223372036854775807')",
        "SELECT count(*) FROM t2 WHERE id IN (1.000, '0', 1e308)",
        // The scalar comparison is STILL exact-equal (the seek is the only
        // place the #3922 gate applies).
        "SELECT -9223372036854775808 = -9.2233720368547758e18, 9223372036854775807 = 9.2233720368547758e18",
        // DELETE / UPDATE route through the same seeks.
        "DELETE FROM t2 WHERE id IN (-9.2233720368547758e18, 5.0, '5')",
        "SELECT id FROM t2 ORDER BY id",
        "UPDATE t2 SET v = 'x' WHERE id = -9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE v = 'x'",
        "UPDATE t2 SET v = 'y' WHERE id = '5'",
        "SELECT count(*) FROM t2 WHERE v = 'y'",
    ];
    for q in queries {
        check(&mut db, &rc, q);
    }
}

#[test]
fn rowid_range_boundary_seeks() {
    let (mut db, rc) = fresh();
    setup_both(
        &mut db,
        &rc,
        &[
            "CREATE TABLE t2 (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t2 (id) VALUES (-9223372036854775808)",
            "INSERT INTO t2 (id) VALUES (9223372036854775807)",
            "INSERT INTO t2 (id) VALUES (9223372036854774784)",
            "INSERT INTO t2 (id) VALUES (-9223372036854774784)",
            "INSERT INTO t2 (id) VALUES (5)",
        ],
    );
    let queries = [
        "SELECT count(*) FROM t2 WHERE id > -9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id >= -9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id < -9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id <= -9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id > 9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id >= 9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id < 9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id <= 9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id > 9223372036854774784.0",
        "SELECT count(*) FROM t2 WHERE id < -9223372036854774784.0",
        "SELECT count(*) FROM t2 WHERE id > 5.5",
        "SELECT count(*) FROM t2 WHERE id >= 5.5",
        "SELECT count(*) FROM t2 WHERE id < 5.5",
        "SELECT count(*) FROM t2 WHERE id <= 5.5",
        "SELECT count(*) FROM t2 WHERE id > 4.9",
        "SELECT count(*) FROM t2 WHERE id >= 4.9",
        "SELECT count(*) FROM t2 WHERE id < 4.9",
        "SELECT count(*) FROM t2 WHERE id <= 4.9",
        "SELECT count(*) FROM t2 WHERE id >= 1e19",
        "SELECT count(*) FROM t2 WHERE id > 1e19",
        "SELECT count(*) FROM t2 WHERE id <= -1e19",
        "SELECT count(*) FROM t2 WHERE id < -1e19",
        "SELECT count(*) FROM t2 WHERE id BETWEEN -9.2233720368547758e18 AND 9.2233720368547758e18",
        "SELECT count(*) FROM t2 WHERE id BETWEEN 4.9 AND 5.5",
        // TEXT range bounds: Atoi64-full-parse first, then the c-adjust.
        "SELECT count(*) FROM t2 WHERE id > '5'",
        "SELECT count(*) FROM t2 WHERE id >= '5'",
        "SELECT count(*) FROM t2 WHERE id <= '5'",
        "SELECT count(*) FROM t2 WHERE id > '5.5'",
        "SELECT count(*) FROM t2 WHERE id <= '5.5'",
        "SELECT count(*) FROM t2 WHERE id >= '9.2233720368547758e18'",
    ];
    for q in queries {
        check(&mut db, &rc, q);
    }
}

#[test]
fn rowid_join_boundary_seeks() {
    // A JOIN B ON A.x = B.id: SQLite seek-plans the alias side.
    let (mut db, rc) = fresh();
    setup_both(
        &mut db,
        &rc,
        &[
            "CREATE TABLE t2 (id INTEGER PRIMARY KEY, v TEXT)",
            "CREATE TABLE k (f REAL)",
            "CREATE TABLE kt (s TEXT)",
            "INSERT INTO t2 (id) VALUES (-9223372036854775808)",
            "INSERT INTO t2 (id) VALUES (5)",
            "INSERT INTO t2 (id) VALUES (7)",
            "INSERT INTO k VALUES (-9.2233720368547758e18)",
            "INSERT INTO k VALUES (5.0)",
            "INSERT INTO k VALUES (7.5)",
            "INSERT INTO kt VALUES ('5')",
            "INSERT INTO kt VALUES ('abc')",
            "INSERT INTO kt VALUES ('-9.2233720368547758e18')",
        ],
    );
    let queries = [
        // Boundary REAL never seeks; integral REAL and numeric TEXT do.
        "SELECT k.f, t2.id FROM k JOIN t2 ON k.f = t2.id ORDER BY 1, 2",
        "SELECT k.f, t2.id FROM t2 JOIN k ON k.f = t2.id ORDER BY 1, 2",
        "SELECT kt.s, t2.id FROM kt JOIN t2 ON kt.s = t2.id ORDER BY 1, 2",
        // LEFT with the alias on the inner (right) side: same seek.
        "SELECT k.f, t2.id FROM k LEFT JOIN t2 ON k.f = t2.id ORDER BY 1, 2",
        // The IN-subquery join shape (SQLite's IN-loop drives rowid seeks).
        "SELECT count(*) FROM t2 WHERE id IN (SELECT f FROM k)",
        "SELECT count(*) FROM t2 WHERE id IN (SELECT s FROM kt)",
        "UPDATE t2 SET v = 'x' WHERE id IN (SELECT f FROM k)",
        "SELECT id, v FROM t2 ORDER BY id",
    ];
    for q in queries {
        check(&mut db, &rc, q);
    }
}

#[test]
fn index_equality_boundary_is_exact_compare() {
    // A NON-alias indexed INTEGER column: index equality uses the exact
    // int/float comparison — the boundary REAL DOES match i64::MIN here.
    // (The rowid seek and the index equality disagree at the boundary,
    // exactly as in SQLite.)
    let (mut db, rc) = fresh();
    setup_both(
        &mut db,
        &rc,
        &[
            "CREATE TABLE plain (id INTEGER, v TEXT)",
            "CREATE INDEX plain_id ON plain(id)",
            "INSERT INTO plain (id) VALUES (-9223372036854775808)",
            "INSERT INTO plain (id) VALUES (5)",
            "CREATE TABLE k (f REAL)",
            "INSERT INTO k VALUES (-9.2233720368547758e18)",
            "INSERT INTO k VALUES (5.0)",
        ],
    );
    let queries = [
        "SELECT count(*) FROM plain WHERE id = -9.2233720368547758e18",
        "SELECT count(*) FROM plain WHERE id = 5.0",
        "SELECT k.f, plain.id FROM k JOIN plain ON k.f = plain.id ORDER BY 1, 2",
    ];
    for q in queries {
        check(&mut db, &rc, q);
    }
}

#[test]
fn rowid_update_alias_moves() {
    let (mut db, rc) = fresh();
    setup_both(
        &mut db,
        &rc,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t (id, v) VALUES (1, 'a')",
        ],
    );
    let stmts = [
        "UPDATE t SET id = 5.0 WHERE id = 1",  // converts: rowid 5
        "UPDATE t SET id = '6' WHERE id = 5",  // converts: rowid 6
        "UPDATE t SET id = 6.5 WHERE id = 6",  // datatype mismatch
        "UPDATE t SET id = NULL WHERE id = 6", // datatype mismatch
        "UPDATE t SET id = 9.2233720368547758e18 WHERE id = 6", // mismatch
        "UPDATE t SET id = -9223372036854774784.0 WHERE id = 6", // converts
        "SELECT id, v FROM t ORDER BY id",
    ];
    for q in stmts {
        check(&mut db, &rc, q);
    }
}

#[test]
#[allow(clippy::excessive_precision)]
fn rowid_boundary_with_parameters() {
    // Bound parameters route through the same seek conversions.
    let (mut db, rc) = fresh();
    setup_both(
        &mut db,
        &rc,
        &[
            "CREATE TABLE t2 (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t2 (id) VALUES (-9223372036854775808)",
            "INSERT INTO t2 (id) VALUES (5)",
        ],
    );
    for v in [
        Value::Real(-9.223_372_036_854_775_8e18),
        Value::Real(5.0),
        Value::Real(5.5),
        Value::Text("5".into()),
    ] {
        let v_dbg = format!("{v:?}");
        let ours: Vec<Vec<Value>> = db
            .query("SELECT count(*) FROM t2 WHERE id = ?", vec![v.clone()])
            .unwrap();
        let sv = match &v {
            Value::Integer(i) => Sv::Integer(*i),
            Value::Real(f) => Sv::Real(*f),
            Value::Text(t) => Sv::Text(t.as_str().to_string()),
            Value::Blob(b) => Sv::Blob(b.clone()),
            Value::Null => Sv::Null,
        };
        let theirs_n: i64 = rc
            .query_row(
                "SELECT count(*) FROM t2 WHERE id = ?",
                rusqlite::params![sv],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let ours_n = match &ours[0][0] {
            Value::Integer(n) => *n,
            _ => panic!("count not integer"),
        };
        assert_eq!(
            ours_n, theirs_n,
            "param {v_dbg}: count {ours_n} vs {theirs_n}"
        );
    }
}

use rusqlite::types::Value as Sv;
