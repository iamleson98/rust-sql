//! A join whose inner side is keyed on the INTEGER PRIMARY KEY (rowid
//! alias) must probe the table by rowid per outer row — SQLite's `SEARCH t
//! USING INTEGER PRIMARY KEY (rowid=?)` — instead of scanning / hashing the
//! whole inner table. The planner used to rewrite a hash join into an
//! index nested-loop join only for SECONDARY indexes; the rowid alias has
//! no index entry in the catalog, so `u JOIN t ON t.id = u.t_id WHERE u.id
//! BETWEEN ? AND ?` decoded every row of `t` for each query (90 q/s vs
//! SQLite's 65,900 q/s).
//!
//! Every case runs on both engines: the row stream and the EXPLAIN QUERY
//! PLAN lines must agree with bundled SQLite.

use rustqlite::{Database, Value};

const SETUP: &[&str] = &[
    "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, name TEXT)",
    "CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, k INTEGER, amt INTEGER)",
    "CREATE INDEX u_t ON u(t_id)",
    "CREATE TABLE w (id INTEGER PRIMARY KEY, u_id INTEGER)",
    "CREATE TABLE tw (id INTEGER PRIMARY KEY, name TEXT) WITHOUT ROWID",
    "CREATE TABLE ts (id INTEGER PRIMARY KEY, code INTEGER, name TEXT)",
    "CREATE INDEX ts_code ON ts(code)",
    "CREATE TABLE us (id INTEGER PRIMARY KEY, code INTEGER)",
    "CREATE TABLE ux (id INTEGER PRIMARY KEY, ref ANY)",
];

fn seed_sql() -> Vec<String> {
    let mut v = Vec::new();
    v.push("BEGIN".to_string());
    for i in 1..=2000i64 {
        v.push(format!("INSERT INTO t VALUES ({i}, {}, 'n{i}')", i % 7));
        v.push(format!("INSERT INTO tw VALUES ({i}, 'w{i}')"));
        v.push(format!("INSERT INTO ts VALUES ({i}, {i}, 's{i}')"));
    }
    for i in 1..=6000i64 {
        // Every 10th reference dangles (no matching t row); every 13th is NULL.
        let tid = if i % 13 == 0 {
            "NULL".to_string()
        } else if i % 10 == 0 {
            (5000 + i).to_string()
        } else {
            ((i * 17) % 2000 + 1).to_string()
        };
        v.push(format!(
            "INSERT INTO u VALUES ({i}, {tid}, {}, {})",
            i % 7,
            i % 100
        ));
        v.push(format!("INSERT INTO w VALUES ({i}, {i})"));
        v.push(format!(
            "INSERT INTO us VALUES ({i}, {})",
            (i * 3) % 2500 + 1
        ));
    }
    // A key column of mixed classes: integer, integral REAL, numeric TEXT,
    // fractional REAL, non-numeric TEXT, BLOB, NULL.
    v.push("INSERT INTO ux VALUES (1, 5), (2, 6.0), (3, '7'), (4, 8.5), (5, 'abc'), (6, x'09'), (7, NULL), (8, '10.0'), (9, ' 11'), (10, 2000)".to_string());
    v.push("COMMIT".to_string());
    v
}

fn fmt_engine(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    Value::Null => "~".to_string(),
                    Value::Integer(v) => v.to_string(),
                    Value::Real(f) => format!("{f:?}"),
                    Value::Text(t) => format!("'{}'", String::from_utf8_lossy(t.as_bytes())),
                    Value::Blob(b) => format!("x{b:02x?}"),
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

fn fmt_sqlite(rc: &rusqlite::Connection, q: &str) -> Vec<String> {
    use rusqlite::types::Value as Sv;
    let mut stmt = rc.prepare(q).unwrap();
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push(
            (0..n)
                .map(|i| match r.get::<_, Sv>(i).unwrap() {
                    Sv::Null => "~".to_string(),
                    Sv::Integer(v) => v.to_string(),
                    Sv::Real(f) => format!("{f:?}"),
                    Sv::Text(t) => format!("'{t}'"),
                    Sv::Blob(b) => format!("x{b:02x?}"),
                })
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    out
}

fn engine_plan(db: &Database, q: &str) -> Vec<String> {
    db.query(&format!("EXPLAIN QUERY PLAN {q}"), [])
        .unwrap()
        .iter()
        .map(|r| r[3].to_string())
        .collect()
}

fn sqlite_plan(rc: &rusqlite::Connection, q: &str) -> Vec<String> {
    let mut stmt = rc.prepare(&format!("EXPLAIN QUERY PLAN {q}")).unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        out.push(r.get::<_, String>(3).unwrap());
    }
    out
}

fn open_pair() -> (Database, rusqlite::Connection) {
    let mut db = Database::open_in_memory().unwrap();
    let rc = rusqlite::Connection::open_in_memory().unwrap();
    for s in SETUP {
        db.execute(s, []).unwrap();
        rc.execute_batch(s).unwrap();
    }
    for s in seed_sql() {
        db.execute(&s, []).unwrap();
        rc.execute_batch(&s).unwrap();
    }
    (db, rc)
}

/// Rows must agree as a multiset; `ordered` additionally pins the stream
/// order (nested-loop joins driven by the same outer emit the same order).
fn check(db: &Database, rc: &rusqlite::Connection, q: &str, ordered: bool) -> Vec<String> {
    let mut e = fmt_engine(&db.query(q, []).unwrap());
    let mut s = fmt_sqlite(rc, q);
    if !ordered {
        e.sort();
        s.sort();
    }
    assert_eq!(e, s, "row mismatch: {q}");
    e
}

/// Rows AND plan lines agree with SQLite, and the plan has the expected
/// probe line for the inner side.
fn check_plan(db: &Database, rc: &rusqlite::Connection, q: &str, want_inner: &str) {
    let rows = check(db, rc, q, true);
    assert!(!rows.is_empty(), "case returns no rows: {q}");
    let ep = engine_plan(db, q);
    let sp = sqlite_plan(rc, q);
    assert_eq!(ep, sp, "plan diverges from SQLite: {q}");
    assert!(
        ep.iter().any(|l| l == want_inner),
        "expected `{want_inner}` in {ep:?}: {q}"
    );
}

const PK: &str = "USING INTEGER PRIMARY KEY (rowid=?)";

#[test]
fn inner_join_probes_inner_rowid() {
    let (db, rc) = open_pair();
    let range = "WHERE u.id BETWEEN 1000 AND 1049";
    for q in [
        format!("SELECT u.id, t.name, u.amt FROM u JOIN t ON t.id = u.t_id {range}"),
        // reversed equality
        format!("SELECT u.id, t.name, u.amt FROM u JOIN t ON u.t_id = t.id {range}"),
        // comma join, equality in WHERE
        "SELECT u.id, t.name FROM u, t WHERE t.id = u.t_id AND u.id BETWEEN 1000 AND 1049"
            .to_string(),
        // the probed table written FIRST: SQLite (and now we) still drive from u
        "SELECT u.id, t.name FROM t, u WHERE t.id = u.t_id AND u.id BETWEEN 1000 AND 1049"
            .to_string(),
        format!("SELECT u.id, t.name FROM t JOIN u ON t.id = u.t_id {range}"),
        // one-sided ranges and a point outer
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id < 60".to_string(),
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id > 5950".to_string(),
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id = 1002".to_string(),
        // LIMIT on top
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id < 100 LIMIT 20".to_string(),
    ] {
        check_plan(&db, &rc, &q, &format!("SEARCH t {PK}"));
    }
}

#[test]
fn star_projection_keeps_column_order() {
    let (db, rc) = open_pair();
    for q in [
        "SELECT * FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049",
        "SELECT * FROM t JOIN u ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049",
        "SELECT t.*, u.amt FROM t JOIN u ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049",
        "SELECT u.id, t.rowid, t.id FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049",
        // the swapped probe through a SECONDARY index keeps the order too
        "SELECT * FROM ts JOIN us ON ts.code = us.code WHERE us.id BETWEEN 1000 AND 1049",
    ] {
        check(&db, &rc, q, false);
    }
    let plan = engine_plan(
        &db,
        "SELECT * FROM t JOIN u ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049",
    );
    assert!(
        plan.iter().any(|l| l == &format!("SEARCH t {PK}")),
        "{plan:?}"
    );
}

#[test]
fn left_join_probes_inner_rowid() {
    let (db, rc) = open_pair();
    // Ids 1..40 include dangling references (i % 10 == 0) and NULL keys
    // (i % 13 == 0): the NULL-extended rows must survive.
    let q = "SELECT u.id, t.name, u.amt FROM u LEFT JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1 AND 60";
    let rows = check(&db, &rc, q, true);
    assert_eq!(rows.len(), 60);
    assert!(
        rows.iter().any(|r| r.contains("~,")),
        "no NULL-extended row"
    );
    check_plan(&db, &rc, q, &format!("SEARCH t {PK} LEFT-JOIN"));
    // Anti-join: keep only the unmatched.
    let q = "SELECT u.id FROM u LEFT JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1 AND 60 AND t.id IS NULL";
    check(&db, &rc, q, true);
    // Extra ON conjunct: the probe must NOT drop NULL-extension semantics.
    for q in [
        "SELECT u.id, t.name FROM u LEFT JOIN t ON t.id = u.t_id AND t.k = 3 WHERE u.id BETWEEN 1 AND 60",
        "SELECT u.id, t.name FROM u LEFT JOIN t ON t.id = u.t_id AND u.amt > 50 WHERE u.id BETWEEN 1 AND 60",
        "SELECT u.id, t.name FROM u LEFT JOIN t ON t.id = u.t_id AND t.id = u.k WHERE u.id BETWEEN 1 AND 60",
    ] {
        check(&db, &rc, q, true);
    }
    // Aggregates over the LEFT form.
    check(
        &db,
        &rc,
        "SELECT count(*), count(t.id), sum(t.k) FROM u LEFT JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1 AND 300",
        true,
    );
}

#[test]
fn chain_and_other_inner_kinds() {
    let (db, rc) = open_pair();
    // 3-table chain: both hops are rowid probes.
    let q = "SELECT w.id, u.id, t.name FROM w JOIN u ON u.id = w.u_id JOIN t ON t.id = u.t_id WHERE w.id BETWEEN 1000 AND 1049";
    let rows = check(&db, &rc, q, true);
    assert!(!rows.is_empty());
    let ep = engine_plan(&db, q);
    assert_eq!(ep, sqlite_plan(&rc, q));
    assert_eq!(
        ep.iter()
            .filter(|l| l.contains("INTEGER PRIMARY KEY (rowid=?)"))
            .count(),
        2,
        "{ep:?}"
    );
    // Secondary-indexed inner column.
    check_plan(
        &db,
        &rc,
        "SELECT us.id, ts.name FROM us JOIN ts ON ts.code = us.code WHERE us.id BETWEEN 1000 AND 1049",
        "SEARCH ts USING INDEX ts_code (code=?)",
    );
    // WITHOUT ROWID inner (no rowid: the PK b-tree serves the key).
    let q =
        "SELECT u.id, tw.name FROM u JOIN tw ON tw.id = u.t_id WHERE u.id BETWEEN 1000 AND 1049";
    check(&db, &rc, q, true);
    let ep = engine_plan(&db, q);
    assert!(
        ep.iter()
            .any(|l| l.starts_with("SEARCH tw USING") && l.contains("(id=?)")),
        "{ep:?}"
    );
    // Aggregates / DISTINCT / ORDER BY above the probe.
    for q in [
        "SELECT count(*) FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1999",
        "SELECT t.k, count(*), sum(u.amt) FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1999 GROUP BY t.k",
        "SELECT DISTINCT t.k FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1999",
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1000 AND 1099 ORDER BY t.name DESC, u.id",
        // composite ON: one key probes, the other rides as a residual
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id AND t.k = u.k WHERE u.id BETWEEN 1000 AND 1999",
        // self join
        "SELECT a.id, b.name FROM t a JOIN t b ON a.id = b.id WHERE a.id < 50",
    ] {
        check(&db, &rc, q, false);
    }
}

#[test]
fn probe_key_conversion_matches_sqlite() {
    let (db, rc) = open_pair();
    // Outer key of every storage class against the rowid alias: integral
    // REAL and numeric TEXT seek the row, fractional / non-numeric /
    // BLOB / NULL keys match nothing (OP_SeekRowid's conversion).
    for q in [
        "SELECT ux.id, t.name FROM ux JOIN t ON t.id = ux.ref WHERE ux.id BETWEEN 1 AND 10",
        "SELECT ux.id, t.name FROM ux LEFT JOIN t ON t.id = ux.ref WHERE ux.id BETWEEN 1 AND 10",
        "SELECT ux.id, t.name FROM ux JOIN t ON ux.ref = t.id WHERE ux.id <= 10",
    ] {
        check(&db, &rc, q, true);
    }
}

#[test]
fn probe_sees_writes_and_empty_outer() {
    let (mut db, rc) = open_pair();
    let q = "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 1 AND 40";
    check(&db, &rc, q, true);
    for s in [
        "DELETE FROM t WHERE id % 3 = 0",
        "UPDATE t SET name = 'z' || id WHERE id % 5 = 0",
        "INSERT INTO t VALUES (9000, 1, 'late')",
        "UPDATE u SET t_id = 9000 WHERE id = 7",
    ] {
        db.execute(s, []).unwrap();
        rc.execute_batch(s).unwrap();
    }
    check(&db, &rc, q, true);
    // An outer range with no rows.
    check(
        &db,
        &rc,
        "SELECT u.id, t.name FROM u JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 100000 AND 100010",
        true,
    );
    check(
        &db,
        &rc,
        "SELECT u.id, t.name FROM u LEFT JOIN t ON t.id = u.t_id WHERE u.id BETWEEN 100000 AND 100010",
        true,
    );
}
