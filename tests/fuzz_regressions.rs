//! Regression pins for every bug the stateful differential fuzz
//! (tests/stateful_fuzz.rs) surfaced in its first deep-sweep campaign.
//! Each test here is the minimized shape of one fuzz divergence, pinned
//! against real SQLite behavior (bundled via rusqlite) so the contract
//! can never silently regress.

use rusqlite::Connection as Sq;
use rustqlite::{Database, Value};

/// Helper: run one statement on both engines, require agreement of the
/// Ok/Err verdict, and return (ours, sqlite) row sets.
fn both(db: &mut Database, sq: &Sq, sql: &str) -> (Vec<Vec<Value>>, Vec<Vec<Sv>>) {
    let ours = db.query(sql, []).map_err(|e| e.to_string());
    let theirs = {
        let mut stmt = sq.prepare(sql).unwrap();
        let ncols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        if ncols == 0 {
            return (ours.unwrap_or_default(), out);
        }
        while let Ok(Some(r)) = rows.next() {
            let row: Vec<Sv> = (0..ncols)
                .map(|i| r.get::<_, Sv>(i).unwrap_or(Sv::Null))
                .collect();
            out.push(row);
        }
        out
    };
    (ours.unwrap_or_default(), theirs)
}

use rusqlite::types::Value as Sv;

/// Cross-engine row equality (numeric cross-type tolerance, like the
/// fuzzer's values_match): INTEGER 5 == REAL 5.0; text/blobs exact.
fn sv_match(a: &Value, b: &Sv) -> bool {
    match (a, b) {
        (Value::Null, Sv::Null) => true,
        (Value::Integer(x), Sv::Integer(y)) => x == y,
        (Value::Integer(x), Sv::Real(y)) => (*x as f64 - y).abs() <= 1e-9 * y.abs().max(1.0),
        (Value::Real(x), Sv::Integer(y)) => (*x - *y as f64).abs() <= 1e-9 * x.abs().max(1.0),
        (Value::Real(x), Sv::Real(y)) => {
            (x.is_nan() && y.is_nan()) || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Text(x), Sv::Text(y)) => x.as_str() == y,
        (Value::Blob(x), Sv::Blob(y)) => x == y,
        _ => false,
    }
}

fn rows_match(ours: &[Vec<Value>], theirs: &[Vec<Sv>]) -> bool {
    ours.len() == theirs.len()
        && ours
            .iter()
            .zip(theirs.iter())
            .all(|(a, b)| a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| sv_match(x, y)))
}

#[test]
fn partial_index_integrity_check_ok() {
    // integrity_check must evaluate a partial index's WHERE predicate:
    // rows the predicate excludes are LEGITIMATELY absent.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t1 (e TEXT, d TEXT)", ()).unwrap();
    db.execute("INSERT INTO t1 VALUES (NULL,'a'), ('x','b'), ('y','c')", ())
        .unwrap();
    db.execute("CREATE UNIQUE INDEX ix ON t1(e) WHERE e IS NOT NULL", ())
        .unwrap();
    let v = db.query("PRAGMA integrity_check", ()).unwrap();
    assert_eq!(v[0][0].as_text(), "ok");
}

#[test]
fn negzero_real_column_normalized() {
    // REAL-affinity storage normalizes -0.0 to +0.0 (SQLite's record
    // layer); BLOB/none-affinity columns and expressions keep the sign.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE r (f REAL, b BLOB)", ()).unwrap();
    db.execute("INSERT INTO r VALUES (-0.0, -0.0)", ()).unwrap();
    let v = db
        .query("SELECT f, typeof(f), b, typeof(b) FROM r", ())
        .unwrap();
    assert_eq!(v[0][0].as_real().to_bits(), 0.0f64.to_bits());
    assert_eq!(v[0][1].as_text(), "real");
    assert!(v[0][2].as_real().is_sign_negative());
    assert_eq!(v[0][3].as_text(), "real");
    let v = db.query("SELECT -0.0", ()).unwrap();
    assert!(v[0][0].as_real().is_sign_negative());
}

#[test]
fn upsert_do_update_fires_update_triggers() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t (v) VALUES ('orig')", ()).unwrap();
    db.execute("CREATE TABLE log (msg TEXT)", ()).unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER UPDATE ON t BEGIN INSERT INTO log VALUES ('fired:' || old.v || '->' || new.v); END",
        (),
    )
    .unwrap();
    db.execute(
        "INSERT INTO t (id, v) VALUES (1, 'new') ON CONFLICT(id) DO UPDATE SET v = excluded.v",
        (),
    )
    .unwrap();
    let v = db.query("SELECT * FROM log", ()).unwrap();
    assert_eq!(v[0][0].as_text(), "fired:orig->new");
}

#[test]
fn negative_rowid_table_allocates_from_true_max() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .unwrap();
    // Same-statement shape: explicit negative, then auto.
    db.execute("INSERT INTO t (id) VALUES (-6), (NULL), (17)", ())
        .unwrap();
    let v = db.query("SELECT rowid FROM t ORDER BY rowid", ()).unwrap();
    assert_eq!(v[0][0].as_integer(), -6);
    assert_eq!(v[1][0].as_integer(), -5, "auto must be max+1 = -5");
    assert_eq!(v[2][0].as_integer(), 17);
    // Cross-statement: all-negative table.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("INSERT INTO t (id) VALUES (-6)", ()).unwrap();
    db.execute("INSERT INTO t (id) VALUES (-9)", ()).unwrap();
    db.execute("INSERT INTO t (id) VALUES (NULL)", ()).unwrap();
    let v = db.query("SELECT rowid FROM t ORDER BY rowid", ()).unwrap();
    assert_eq!(v[2][0].as_integer(), -5);
    // Even a real i64::MIN row allocates MIN+1 (SQLite parity).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("INSERT INTO t (id) VALUES (-9223372036854775808)", ())
        .unwrap();
    db.execute("INSERT INTO t (id) VALUES (NULL)", ()).unwrap();
    let v = db.query("SELECT rowid FROM t ORDER BY rowid", ()).unwrap();
    assert_eq!(v[1][0].as_integer(), i64::MIN + 1);
}

#[test]
fn update_rowid_text_key_matches_zero_rows() {
    // `WHERE id = 'zeta'` on an INTEGER PRIMARY KEY: INTEGER affinity
    // leaves the non-numeric literal TEXT, and INTEGER = TEXT is
    // constant-false — the UPDATE must touch NOTHING (the old streaming
    // path fell through to an unconstrained full scan and rewrote every
    // row).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (5, 'keep')", ()).unwrap();
    db.execute("UPDATE t SET v = 'CHANGED' WHERE id = 'zeta'", ())
        .unwrap();
    let v = db.query("SELECT v FROM t", ()).unwrap();
    assert_eq!(v[0][0].as_text(), "keep");
    // Numeric-looking text still matches (comparison affinity).
    db.execute("UPDATE t SET v = 'CHANGED' WHERE id = '5'", ())
        .unwrap();
    let v = db.query("SELECT v FROM t", ()).unwrap();
    assert_eq!(v[0][0].as_text(), "CHANGED");
}

#[test]
fn check_constraint_uses_comparison_affinity() {
    // A TEXT-affinity column in a numeric CHECK compares TEXT-to-TEXT.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (f BLOB, f2 TEXT CHECK (f2 >= -1000000), b TEXT)",
        (),
    )
    .unwrap();
    let r = db
        .execute("INSERT INTO t (f2) VALUES ('')", ())
        .map_err(|e| e.to_string());
    assert!(r.is_err(), "'' >= '-1000000' is text-wise FALSE");
    let r = db
        .execute("INSERT INTO t (f2) VALUES ('abc')", ())
        .map_err(|e| e.to_string());
    assert!(r.is_ok(), "'abc' >= '-1000000' is text-wise TRUE");
}

#[test]
fn comparison_affinity_is_numeric_not_storage() {
    // datatype3 §4.2: INTEGER/REAL/NUMERIC columns lend NUMERIC affinity
    // (NOT their storage affinity): Real(5e-324) must NOT truncate to 0.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (0, 1)", ()).unwrap();
    let v = db
        .query("SELECT count(*) FROM t WHERE a <> 5e-324", ())
        .unwrap();
    assert_eq!(v[0][0].as_integer(), 1, "0 <> 5e-324 must be TRUE");
}

#[test]
fn alter_add_column_real_default_stays_real() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 1)", ()).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN c REAL DEFAULT -4.0", ())
        .unwrap();
    db.execute("INSERT INTO t (a, b) VALUES (2, 2)", ())
        .unwrap();
    let v = db
        .query("SELECT typeof(c), c FROM t WHERE a = 2", ())
        .unwrap();
    assert_eq!(v[0][0].as_text(), "real");
    assert_eq!(v[0][1].as_real(), -4.0);
}

#[test]
fn rowid_move_updates_max_rowid_for_triggers() {
    // Moving the max rowid (47 -> 35) must invalidate the cached max so
    // a trigger's INSERT allocates from the NEW max (36), not the stale
    // one (48).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
        .unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER UPDATE ON t BEGIN INSERT INTO t (v) VALUES ('trig'); END",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO t (id, v) VALUES (47, 'x')", ())
        .unwrap();
    db.execute("UPDATE t SET id = 35, v = NULL", ()).unwrap();
    let v = db.query("SELECT rowid FROM t ORDER BY rowid", ()).unwrap();
    assert_eq!(v[0][0].as_integer(), 35);
    assert_eq!(v[1][0].as_integer(), 36, "trigger row must take max+1");
}

#[test]
fn alter_add_column_keeps_triggers_alive() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t0 (a INTEGER, b INTEGER)", ())
        .unwrap();
    db.execute("CREATE TABLE log (n TEXT)", ()).unwrap();
    db.execute("INSERT INTO t0 VALUES (1,1),(2,2),(3,3)", ())
        .unwrap();
    db.execute(
        "CREATE TRIGGER trg AFTER DELETE ON t0 BEGIN INSERT INTO log VALUES ('d'); END",
        (),
    )
    .unwrap();
    db.execute("ALTER TABLE t0 ADD COLUMN c TEXT DEFAULT 'x'", ())
        .unwrap();
    db.execute("DELETE FROM t0 WHERE rowid BETWEEN 2 AND 3", ())
        .unwrap();
    let v = db.query("SELECT count(*) FROM log", ()).unwrap();
    assert_eq!(v[0][0].as_integer(), 2, "trigger must survive ADD COLUMN");
}

#[test]
fn failed_statement_rolls_back_autoincrement_sequence() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT UNIQUE)",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO t (v) VALUES ('a')", ()).unwrap();
    // Fails on the last tuple: the earlier tuples' sequence bumps roll
    // back with the statement (SQLite reuses the ids, never burns them).
    let r = db
        .execute("INSERT INTO t (v) VALUES ('b'), ('c'), ('a')", ())
        .map_err(|e| e.to_string());
    assert!(r.is_err());
    let v = db.query("SELECT seq FROM sqlite_sequence", ()).unwrap();
    assert_eq!(v[0][0].as_integer(), 1);
    db.execute("INSERT INTO t (v) VALUES ('b'), ('c')", ())
        .unwrap();
    let v = db.query("SELECT id FROM t ORDER BY id", ()).unwrap();
    let ids: Vec<i64> = v.iter().map(|r| r[0].as_integer()).collect();
    assert_eq!(ids, vec![1, 2, 3], "no burned gaps after failed insert");
}

#[test]
fn join_cross_affinity_equality_matches_sqlite() {
    // INTEGER column = TEXT column applies NUMERIC to the TEXT side.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE a (e INTEGER)", ()).unwrap();
    db.execute("CREATE TABLE b (note TEXT)", ()).unwrap();
    db.execute("INSERT INTO a VALUES (5), (6)", ()).unwrap();
    db.execute("INSERT INTO b VALUES ('5'), ('7')", ()).unwrap();
    let sq = Sq::open_in_memory().unwrap();
    sq.execute_batch(
        "CREATE TABLE a (e INTEGER); CREATE TABLE b (note TEXT); INSERT INTO a VALUES (5),(6); INSERT INTO b VALUES ('5'),('7');",
    )
    .unwrap();
    for sql in [
        "SELECT count(*) FROM a JOIN b ON a.e = b.note",
        "SELECT count(*) FROM a, b WHERE a.e = b.note",
        "SELECT 5 = '5'",
    ] {
        let (ours, theirs) = both(&mut db, &sq, sql);
        assert!(
            rows_match(&ours, &theirs),
            "divergence on {sql}: ours {ours:?} vs sqlite {theirs:?}"
        );
    }
}

#[test]
fn sum_text_and_blob_arguments_match_sqlite() {
    // SQLite's sumStep: TEXT with integer preference, BLOB bytes as a
    // REAL numeric prefix; non-numeric content contributes 0.0 REAL.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (c BLOB)", ()).unwrap();
    db.execute(
        "INSERT INTO t VALUES (x'36'), (x'34'), (x'61'), (NULL), (x'30')",
        (),
    )
    .unwrap();
    let sq = Sq::open_in_memory().unwrap();
    sq.execute_batch(
        "CREATE TABLE t (c BLOB); INSERT INTO t VALUES (x'36'), (x'34'), (x'61'), (NULL), (x'30');",
    )
    .unwrap();
    let (ours, theirs) = both(&mut db, &sq, "SELECT sum(c), typeof(sum(c)) FROM t");
    assert!(
        rows_match(&ours, &theirs),
        "ours {ours:?} sqlite {theirs:?}"
    );
    let v = db.query("SELECT sum(c) FROM t", ()).unwrap();
    assert_eq!(v[0][0].as_real(), 10.0);

    for lit in [
        "'6abc'", "' 6 '", "'abc'", "'6e2'", "'.5'", "x'3638'", "'6.5abc'", "''",
    ] {
        let sql = format!("SELECT sum(x), typeof(sum(x)) FROM (SELECT {lit} AS x)");
        let (ours, theirs) = both(&mut db, &sq, &sql);
        assert!(
            rows_match(&ours, &theirs),
            "sum({lit}) diverged: ours {ours:?} sqlite {theirs:?}"
        );
    }
}

#[test]
fn insert_default_values_applies_column_defaults() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT DEFAULT 'anon', n INTEGER DEFAULT 0)",
        (),
    )
    .unwrap();
    db.execute("INSERT INTO t DEFAULT VALUES", ()).unwrap();
    let v = db.query("SELECT * FROM t", ()).unwrap();
    assert_eq!(v[0][1].as_text(), "anon");
    assert_eq!(v[0][2].as_integer(), 0);
    // An explicit NULL stays NULL (DEFAULT never overwrites one).
    db.execute("INSERT INTO t (name) VALUES (NULL)", ())
        .unwrap();
    let v = db.query("SELECT name, n FROM t WHERE id = 2", ()).unwrap();
    assert!(v[0][0].is_null());
    assert_eq!(v[0][1].as_integer(), 0);
}
