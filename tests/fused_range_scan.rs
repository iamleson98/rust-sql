//! Fused range-probe scan (FilteredScanDriver + probe_int_column)
//! correctness regressions: BETWEEN / AND-chains / equality / rowid-alias
//! ranges / mixed-type payloads / NULL / REAL / TEXT — plus the
//! bounds-typing rule (REAL/TEXT bounds DECLINE the fused path and take
//! the general predicate evaluator with full mixed-type semantics).
use rustqlite::{Database, Value};

fn mixed_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, name TEXT)",
        [],
    )
    .unwrap();
    // Mixed-type column `a`: integers, REALs inside/outside ranges, NULL,
    // TEXT below/above numbers, a negative, a duplicate.
    let vals: Vec<Value> = vec![
        Value::Integer(5),
        Value::Integer(10),
        Value::Integer(15),
        Value::Integer(20),
        Value::Real(12.5), // REAL inside [10, 15]
        Value::Real(99.5), // REAL outside
        Value::Null,
        Value::Text("text-low".into()),
        Value::Text("zzz".into()),
        Value::Integer(-3),
        Value::Integer(5), // duplicate
    ];
    let names = [
        "five", "ten", "fifteen", "twenty", "real125", "real995", "null", "textlow", "textzzz",
        "neg", "five2",
    ];
    for (v, n) in vals.into_iter().zip(names) {
        db.execute(
            "INSERT INTO t (a, name) VALUES (?, ?)",
            [v, Value::Text(n.into())],
        )
        .unwrap();
    }
    db
}

fn ids(db: &Database, sql: &str) -> Vec<String> {
    db.query(sql, [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => i.to_string(),
            v => panic!("{sql}: unexpected {v:?}"),
        })
        .collect()
}

fn count(db: &Database, sql: &str, params: impl IntoIterator<Item = Value>) -> i64 {
    let params: Vec<Value> = params.into_iter().collect();
    match db
        .query(sql, params)
        .unwrap()
        .first()
        .and_then(|r| r.first().cloned())
    {
        Some(Value::Integer(n)) => n,
        other => panic!("{sql}: expected INTEGER count, got {other:?}"),
    }
}

#[test]
fn fused_between_matches_in_rowid_order() {
    let db = mixed_db();
    // REAL payload 12.5 matches the INTEGER-bounded range via the exact
    // cross-class comparator; NULL/TEXT/BLOB never match; rows in rowid order.
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE a BETWEEN 8 AND 16"),
        ["2", "3", "5"]
    );
}

#[test]
fn fused_and_chain_two_sided() {
    let db = mixed_db();
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE a >= 10 AND a <= 20"),
        ["2", "3", "4", "5"]
    );
    // Constant-on-left form fuses too.
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE 10 <= a AND a <= 15"),
        ["2", "3", "5"]
    );
}

#[test]
fn fused_equality_degenerate_range() {
    let db = mixed_db();
    assert_eq!(ids(&db, "SELECT id FROM t WHERE a = 5"), ["1", "11"]);
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE 5 = a"),
        ["1", "11"],
        "constant-on-left equality"
    );
}

#[test]
fn fused_range_skips_text_and_null() {
    let db = mixed_db();
    // 8 numeric rows (incl. 12.5, 99.5, -3); NULL/TEXT/BLOB never match.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN -100 AND 100",
            []
        ),
        8
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN 1 AND 1000000",
            []
        ),
        7
    );
    // Left-of-range negative.
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE a BETWEEN -10 AND -1"),
        ["10"]
    );
    // Inverted bounds: false for every value class.
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM t WHERE a BETWEEN 20 AND 5", []),
        0
    );
}

#[test]
fn fused_rowid_alias_range_is_a_seek() {
    let db = mixed_db();
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE id BETWEEN 2 AND 4"),
        ["2", "3", "4"]
    );
    assert_eq!(ids(&db, "SELECT id FROM t WHERE id = 7"), ["7"]);
    // Mixed: rowid range fused + general rowid NOT IN residual.
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE id BETWEEN 1 AND 5 AND id != 3"),
        ["1", "2", "4", "5"]
    );
}

#[test]
fn non_fused_shapes_still_correct() {
    let db = mixed_db();
    // One-sided: TEXT rows match per type order (numbers < TEXT).
    assert_eq!(count(&db, "SELECT COUNT(*) FROM t WHERE a > 20", []), 3);
    // NOT BETWEEN declines the fused path; NULL stays excluded.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a NOT BETWEEN 5 AND 20",
            []
        ),
        4
    );
}

#[test]
fn real_and_text_bounds_decline_fused() {
    let db = mixed_db();
    // REAL bounds must NOT truncate into the fused range: 8.5/15.5 keep
    // fractional semantics (10, 12.5, 15 match).
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN 8.5 AND 15.5",
            []
        ),
        3
    );
    // REAL literal mixed with INTEGER bound (AND-chain): declines.
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM t WHERE a >= 8.5 AND a <= 15", []),
        3
    );
    // INTEGER params fuse: 10, 12.5, 15 in [10, 15].
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN ? AND ?",
            [Value::Integer(10), Value::Integer(15)]
        ),
        3
    );
    // REAL params decline — same answer as REAL literals.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN ? AND ?",
            [Value::Real(8.5), Value::Real(15.5)]
        ),
        3
    );
    // TEXT bounds: numbers < TEXT, so only TEXT rows can match;
    // 'text-low' ∈ ['5', 'z'] and 'zzz' > 'z'.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN '5' AND 'z'",
            []
        ),
        1
    );
    // TEXT param bounds decline as well.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a >= ? AND a <= ?",
            [Value::Text("5".into()), Value::Text("z".into())]
        ),
        1
    );
    // NULL bound: NULL comparisons never match — fused must decline, not
    // treat NULL as 0.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN ? AND ?",
            [Value::Null, Value::Integer(100)]
        ),
        0
    );
}

#[test]
fn fused_range_batches_and_empty_table() {
    // The fused path feeds the batching cursor: query more rows than one
    // batch, and an empty table must stream cleanly.
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..5000i64 {
        db.execute("INSERT INTO t (a) VALUES (?)", [Value::Integer(i % 1000)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let n = count(
        &db,
        "SELECT COUNT(*) FROM t WHERE a BETWEEN 100 AND 199",
        [],
    );
    assert_eq!(n, 500, "5 hits per value × 100 values");
    // Empty table: fused walk over an empty B+tree.
    db.execute("CREATE TABLE empty (id INTEGER PRIMARY KEY, a INTEGER)", [])
        .unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM empty WHERE a BETWEEN 1 AND 10",
            []
        ),
        0
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM empty WHERE a = 1", []), 0);
}

#[test]
fn fused_range_with_index_present_uses_index() {
    // An index on the range column supersedes the fused scan; the answers
    // must agree either way (index order: value, then rowid).
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX ia ON t(a)", []).unwrap();
    for i in 0..200i64 {
        db.execute("INSERT INTO t (a) VALUES (?)", [Value::Integer(i % 50)])
            .unwrap();
    }
    // 1-based rowids: a in 10..=19 -> rowids 11..=20, 61..=70, 111..=120,
    // 161..=170 (40 rows), emitted in INDEX order (a, then rowid).
    let want: Vec<String> = {
        let mut v = Vec::new();
        for a in 10..=19i64 {
            for r in 1..=200i64 {
                if (r - 1) % 50 == a {
                    v.push(r.to_string());
                }
            }
        }
        v
    };
    assert_eq!(ids(&db, "SELECT id FROM t WHERE a BETWEEN 10 AND 19"), want);
    // rowid pseudo-column through the SAME fused index-range path.
    let got: Vec<i64> = db
        .query("SELECT rowid FROM t WHERE a BETWEEN 10 AND 19", [])
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Integer(i) => *i,
            v => panic!("rowid gave {v:?}"),
        })
        .collect();
    let want_ids: Vec<i64> = want.iter().map(|s| s.parse().unwrap()).collect();
    assert_eq!(got, want_ids, "rowid via index range");
}

#[test]
fn rowid_pseudo_column_through_index_range() {
    // Regression: `SELECT rowid FROM t WHERE indexed_col BETWEEN ...`
    // returned NULL — the materialized Project(IndexRange) chain had no
    // rowid in its name-lookup space (the fused executor now resolves it
    // via the ROWID_PROJ sentinel). Covers alias and no-alias tables,
    // all three spellings, params, residuals, and the wide-range merge
    // scan (emission order stays index order).
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, name TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ia ON t(a)", []).unwrap();
    for i in 0..20i64 {
        db.execute(
            "INSERT INTO t (a, name) VALUES (?, 'n')",
            [Value::Integer(i % 10)],
        )
        .unwrap();
    }
    let one = |db: &Database, sql: &str, params: &[Value]| -> Vec<i64> {
        db.query(sql, params.to_vec())
            .unwrap()
            .into_iter()
            .map(|r| match &r[0] {
                Value::Integer(i) => *i,
                v => panic!("{sql}: {v:?}"),
            })
            .collect()
    };
    // rowids 3,13 carry a=2; 4,14 carry a=3 — index order (a, rowid).
    assert_eq!(
        one(&db, "SELECT rowid FROM t WHERE a BETWEEN 2 AND 3", &[]),
        vec![3, 13, 4, 14]
    );
    assert_eq!(
        one(&db, "SELECT oid FROM t WHERE a BETWEEN 2 AND 3", &[]),
        vec![3, 13, 4, 14]
    );
    assert_eq!(
        one(&db, "SELECT _rowid_ FROM t WHERE a BETWEEN 2 AND 3", &[]),
        vec![3, 13, 4, 14]
    );
    assert_eq!(
        one(
            &db,
            "SELECT rowid FROM t WHERE a BETWEEN ? AND ?",
            &[Value::Integer(2), Value::Integer(3)]
        ),
        vec![3, 13, 4, 14]
    );
    // Residual on the rowid-alias column.
    assert_eq!(
        one(
            &db,
            "SELECT rowid FROM t WHERE a BETWEEN 2 AND 3 AND id > 12",
            &[]
        ),
        vec![13, 14]
    );
    // No-alias table: the rowid is purely the B+tree cell key.
    db.execute("CREATE TABLE u (a INTEGER)", []).unwrap();
    db.execute("CREATE INDEX ua ON u(a)", []).unwrap();
    for i in 0..10i64 {
        db.execute("INSERT INTO u (a) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    assert_eq!(
        one(&db, "SELECT rowid FROM u WHERE a BETWEEN 2 AND 3", &[]),
        vec![3, 4]
    );
    // Wide selection (>= 25%): merge-scan fetch, index-order emission.
    assert_eq!(
        one(&db, "SELECT rowid FROM u WHERE a BETWEEN 0 AND 9", &[]),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    );
    // SELECT * through the fused path: full rows, declared column shape.
    let rows = db
        .query("SELECT * FROM u WHERE a BETWEEN 2 AND 3", [])
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 1, "SELECT * arity: no hidden rowid column");
}

#[test]
fn fused_range_wide_integers() {
    // i64 extremes flow through the probe's width tags exactly.
    let mut db = Database::open_in_memory().unwrap();
    db.set_deferred_flush(true);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", [])
        .unwrap();
    for v in [
        i64::MIN,
        i64::MIN + 1,
        -2_147_483_649i64,
        -32_769i64,
        -129i64,
        -1i64,
        0i64,
        1i64,
        127i64,
        128i64,
        32_768i64,
        2_147_483_648i64,
        i64::MAX - 1,
        i64::MAX,
    ] {
        db.execute("INSERT INTO t (a) VALUES (?)", [Value::Integer(v)])
            .unwrap();
    }
    // Inserts in order: 1=MIN, 2=MIN+1, 3=-2^31-1, 4=-32769, 5=-129,
    // 6=-1, 7=0, 8=1, 9=127, 10=128, 11=32768, 12=2^31, 13=MAX-1, 14=MAX.
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE a BETWEEN -33000 AND -120"),
        ["4", "5"]
    );
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE a BETWEEN 32000 AND 2147483648"),
        ["11", "12"]
    );
    assert_eq!(ids(&db, "SELECT id FROM t WHERE a = 0"), ["7"]);
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM t WHERE a BETWEEN -9223372036854775808 AND 9223372036854775807",
            []
        ),
        14
    );
}
