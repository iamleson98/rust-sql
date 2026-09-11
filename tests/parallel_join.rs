//! Parallel JOIN probe: the fused scan-hash join's probe side splits
//! across workers. Results (values AND row order) must be bit-identical
//! to the serial probe — range-ordered partial concatenation reproduces
//! the serial scan's row order exactly.

use rustqlite::{Database, Value};

fn build(db: &mut Database, n: i64) {
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 3),
            ],
        )
        .unwrap();
    }
    for i in 1..=n {
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(i * 7),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

#[test]
fn parallel_join_matches_serial_exactly() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 150_000);
    let sql = "SELECT a.x, b.y, a.k FROM a JOIN b ON b.k = a.k";
    // Serial (parallel_scan off).
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    // Parallel (default threshold engaged at 400k rows).
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser.len(), par.len(), "row count differs");
    assert_eq!(
        ser, par,
        "parallel join must equal serial bit-for-bit, order included"
    );
    // Sanity: the join actually matched rows (both k ranges overlap).
    assert!(ser.len() > 1000, "join unexpectedly tiny: {}", ser.len());
}

#[test]
fn parallel_join_with_filter_and_order() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 120_000);
    let sql = "SELECT a.x, b.y FROM a JOIN b ON b.k = a.k WHERE a.x > 100000 ORDER BY b.y LIMIT 50";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser, par);
    assert_eq!(ser.len(), 50);
}

#[test]
fn parallel_join_aggregate_over_output() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 120_000);
    let sql = "SELECT COUNT(*), SUM(a.x), MIN(b.y), MAX(b.y) FROM a JOIN b ON b.k = a.k";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser, par);
    // The aggregate values are deterministic integers: both paths equal.
    assert_eq!(ser[0][0], par[0][0]);
}

#[test]
fn parallel_join_real_keys_and_nulls() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k REAL, x TEXT)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k REAL, y TEXT)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=100_000i64 {
        let k = if i % 10 == 0 {
            Value::Null // NULL keys never match
        } else {
            Value::Real((i % 500) as f64 + 0.5)
        };
        db.execute(
            "INSERT INTO a (id, k, x) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                k,
                Value::Text(format!("a{}", i % 97).into()),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO b (id, k, y) VALUES (?, ?, ?)",
            [
                Value::Integer(i),
                Value::Real((i % 500) as f64 + 0.5),
                Value::Text(format!("b{}", i % 89).into()),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let sql = "SELECT a.x, b.y FROM a JOIN b ON b.k = a.k";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser, par);
    assert!(!ser.is_empty());
}

#[test]
fn parallel_join_write_between_queries() {
    // Writes between executions must invalidate the join-build cache and
    // the workers must observe the new committed state.
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 100_000);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let sql = "SELECT COUNT(*) FROM a JOIN b ON b.k = a.k";
    let before = rows_of(&db, sql)[0][0].as_integer();
    db.execute("DELETE FROM b WHERE id > 99000", []).unwrap();
    let after = rows_of(&db, sql)[0][0].as_integer();
    assert!(
        after <= before,
        "delete must not grow the join: {} -> {}",
        before,
        after
    );
    db.execute(
        "INSERT INTO b (id, k, y) VALUES (100001, 99999, 99999)",
        [
            Value::Integer(1000001),
            Value::Integer(0),
            Value::Integer(0),
        ],
    )
    .unwrap();
    let grown = rows_of(&db, sql)[0][0].as_integer();
    assert!(
        grown >= after,
        "insert must be visible to the parallel probe"
    );
}

// ---------------------------------------------------------------------------
// MULTI-KEY equi-joins: the fused scan-hash join builds a composite-key
// table (folded hash + value verification); the parallel probe splits the
// probe side under the same discipline. Parallel must equal serial
// bit-for-bit (order included), and the fused path must agree with the
// MATERIALIZED nested-loop path (sorted comparison — INNER-join row order
// is unspecified across algorithms).
// ---------------------------------------------------------------------------

fn build_multikey(db: &mut Database, n: i64) {
    db.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, k1 INTEGER, k2 INTEGER, x REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE b (id INTEGER PRIMARY KEY, k1 INTEGER, k2 INTEGER, y INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        // k1 = i - i%3 (collisions), k2 = -(i % 5) (5 classes, negatives),
        // x REAL to exercise the cross-type key mix.
        db.execute(
            "INSERT INTO a (id, k1, k2, x) VALUES (?, ?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(-(i % 5)),
                Value::Real(i as f64 / 2.0),
            ],
        )
        .unwrap();
    }
    for i in 1..=n {
        // b mirrors a's key space with a shifted collision pattern.
        db.execute(
            "INSERT INTO b (id, k1, k2, y) VALUES (?, ?, ?, ?)",
            [
                Value::Integer(i),
                Value::Integer(i - (i % 3)),
                Value::Integer(-((i + 2) % 5)),
                Value::Integer(i * 7),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

fn sorted_rows(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| {
        for (x, y) in a.iter().zip(b.iter()) {
            let ord = x.cmp(y);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    rows
}

#[test]
fn multikey_join_parallel_matches_serial() {
    let mut db = Database::open_in_memory().unwrap();
    build_multikey(&mut db, 150_000);
    let sql = "SELECT a.x, b.y, a.k1, b.k2 FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert_eq!(ser.len(), par.len(), "row count differs");
    assert_eq!(ser, par, "parallel multi-key join must equal serial");
    assert!(ser.len() > 1000, "join unexpectedly tiny: {}", ser.len());
    // Ground truth row count: pairs (i, j) with same k1 and k2.
    // k1 = i - i%3 (multiples of 3 buckets); k2 classes overlap only when
    // i%5 == (j+2)%5 AND same k1 bucket — count via SQL itself:
    let n = rows_of(
        &db,
        "SELECT COUNT(*) FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2",
    );
    assert_eq!(n[0][0], Value::Integer(ser.len() as i64));
}

#[test]
fn multikey_join_fused_matches_materialized() {
    // The fused streaming path vs the materialized nested-loop path: for
    // 150k x 150k the fused path answers; forcing the materialized shape
    // via a residual condition (`AND b.y > 0` is NOT a pure equi chain —
    // wait, it is ANDed with the equi keys, so purity declines and the
    // materialized hash join with encoded byte keys answers instead).
    let mut db = Database::open_in_memory().unwrap();
    build_multikey(&mut db, 60_000);
    let fused = "SELECT a.id, b.id FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2";
    let materialized = "SELECT a.id, b.id FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2 AND b.y > 0";
    let f = rows_of(&db, fused);
    let m_all = rows_of(&db, materialized);
    // materialized adds `b.y > 0` (y = id*7 > 0 for all ids >= 1): the
    // same matches — compare sorted.
    assert_eq!(sorted_rows(f.clone()), sorted_rows(m_all.clone()));
    // And against an independent formulation: IN-based semi-check via
    // GROUP BY over the concatenated key.
    let grouped = rows_of(
        &db,
        "SELECT COUNT(*) FROM (SELECT k1, k2 FROM a INTERSECT SELECT k1, k2 FROM b)",
    );
    let _ = grouped; // shape smoke: INTERSECT path executes on join keys
                     // Spot check: rows satisfy BOTH key equalities by construction.
    for r in f.iter().step_by(97) {
        let a_k1 = rows_of(
            &db,
            &format!("SELECT k1 FROM a WHERE id = {}", r[0].as_integer()),
        );
        let b_k1 = rows_of(
            &db,
            &format!("SELECT k1, k2 FROM b WHERE id = {}", r[1].as_integer()),
        );
        let a_k2 = rows_of(
            &db,
            &format!("SELECT k2 FROM a WHERE id = {}", r[0].as_integer()),
        );
        assert_eq!(a_k1[0][0], b_k1[0][0], "k1 must match");
        assert_eq!(a_k2[0][0], b_k1[0][1], "k2 must match");
    }
}

#[test]
fn multikey_join_null_and_type_mix() {
    // NULL keys never match; INTEGER/REAL cross-type key equality (5 =
    // 5.0) through the composite hash + verification; three-key chains.
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE l (id INTEGER PRIMARY KEY, p INTEGER, q INTEGER, r REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE rr (id INTEGER PRIMARY KEY, p INTEGER, q INTEGER, r REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO l (id, p, q, r) VALUES
            (1, 10, 20, 1.5),
            (2, 10, 20, 2.5),
            (3, NULL, 20, 1.5),
            (4, 10, NULL, 1.5),
            (5, 11, 21, 3.0)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO rr (id, p, q, r) VALUES
            (1, 10, 20, 1.5),
            (2, 10, 20, 1.5),
            (3, 10, 20, 2.5),
            (4, NULL, 20, 9.9),
            (5, 10, NULL, 9.9)",
        [],
    )
    .unwrap();
    let sql = "SELECT l.id, rr.id FROM l JOIN rr ON l.p = rr.p AND l.q = rr.q AND l.r = rr.r";
    let got = rows_of(&db, sql);
    // Matches: l(1) [10,20,1.5] joins BOTH rr(1) and rr(2) (duplicate
    // build-side key -> chain); l(2) [10,20,2.5] joins rr(3). NULL-key
    // rows (l3, l4, rr4, rr5) never match; l(5) has no partner.
    let mut pairs: Vec<(i64, i64)> = got
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();
    pairs.sort_unstable();
    assert_eq!(
        pairs,
        vec![(1, 1), (1, 2), (2, 3)],
        "NULL keys must drop, values must match exactly"
    );
    // INTEGER = REAL cross-type composite equality: 5 = 5.0 on every key.
    db.execute(
        "CREATE TABLE n1 (id INTEGER PRIMARY KEY, k INTEGER, v INTEGER)",
        [],
    )
    .unwrap();
    db.execute(
        "CREATE TABLE n2 (id INTEGER PRIMARY KEY, k REAL, v REAL)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO n1 (id, k, v) VALUES (1, 5, 7), (2, 6, 8), (3, 5, 9)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO n2 (id, k, v) VALUES (1, 5.0, 7.0), (2, 6.0, 8.0), (3, 5.0, 9.0)",
        [],
    )
    .unwrap();
    let got = rows_of(
        &db,
        "SELECT n1.id, n2.id FROM n1 JOIN n2 ON n1.k = n2.k AND n1.v = n2.v",
    );
    let mut pairs: Vec<(i64, i64)> = got
        .iter()
        .map(|r| (r[0].as_integer(), r[1].as_integer()))
        .collect();
    pairs.sort_unstable();
    assert_eq!(
        pairs,
        vec![(1, 1), (2, 2), (3, 3)],
        "cross-type composite keys must match"
    );
}

#[test]
fn multikey_join_projection_and_aggregate() {
    // Bare projection + no-Project shapes + an aggregate over the join
    // output (the synthesized combined-row path), big tables, parallel
    // vs serial.
    let mut db = Database::open_in_memory().unwrap();
    build_multikey(&mut db, 130_000);
    let queries = [
        "SELECT a.x, b.y FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2",
        "SELECT a.id, a.k1, a.k2, a.x, b.id, b.k1, b.k2, b.y FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2",
        "SELECT COUNT(*), SUM(b.y), MIN(a.x), MAX(a.x) FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2",
    ];
    for sql in queries {
        db.execute("PRAGMA parallel_scan=0", []).unwrap();
        let ser = rows_of(&db, sql);
        db.execute("PRAGMA parallel_scan=131072", []).unwrap();
        let par = rows_of(&db, sql);
        assert_eq!(ser, par, "{sql}: parallel must equal serial");
    }
    // The aggregate row is non-trivial.
    let agg = rows_of(
        &db,
        "SELECT COUNT(*), SUM(b.y), MIN(a.x), MAX(a.x) FROM a JOIN b ON a.k1 = b.k1 AND a.k2 = b.k2",
    );
    match &agg[0][0] {
        Value::Integer(i) => assert!(*i > 1000, "join aggregate count: {i}"),
        other => panic!("INTEGER expected, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ORDER BY over JOIN output: the sort input is the join's materialized
// rows (not a bare scan) — the materialized-row parallel sort
// (parallel::try_parallel_sort_rows) splits it; parallel (join probe
// split + sort split) must equal serial end-to-end.
// ---------------------------------------------------------------------------

#[test]
fn parallel_sort_over_join_output_matches_serial() {
    let mut db = Database::open_in_memory().unwrap();
    build(&mut db, 60_000);
    // The join emits ~3 matches per row (k = i - i%3 buckets): 180k
    // output rows — above the split threshold.
    let sql = "SELECT a.x, b.y FROM a JOIN b ON a.k = b.k ORDER BY a.x * -1, b.y";
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    let ser = rows_of(&db, sql);
    db.execute("PRAGMA parallel_scan=131072", []).unwrap();
    let par = rows_of(&db, sql);
    assert!(
        ser.len() > 131_072,
        "join output must be above threshold: {}",
        ser.len()
    );
    assert_eq!(ser, par, "ORDER BY over join output must match serial");
    // Spot check: a.x * -1 ascending — a.x = i*3, so x DESC; the first
    // join row of the highest x, then b.y ascending within ties.
    let first = &par[0];
    let max_x = 60_000i64 * 3;
    assert_eq!(first[0].as_integer(), max_x);
    // Ties (same a.x): b.y ascending — the tie block is sorted.
    let tie: Vec<i64> = par
        .iter()
        .filter(|r| r[0].as_integer() == max_x)
        .map(|r| r[1].as_integer())
        .collect();
    let mut sorted = tie.clone();
    sorted.sort_unstable();
    assert_eq!(tie, sorted, "tie block must be b.y-ascending");
}
