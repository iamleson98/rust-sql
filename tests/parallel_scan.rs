//! Intra-statement parallel aggregation tests (see `executor::parallel`).
//!
//! Contract: for tables above the parallel-scan threshold, the worker-split
//! path must return results IDENTICAL to the serial path (`PRAGMA
//! parallel_scan=0`) — same values, same GROUP BY row order (the range-order
//! merge reproduces the serial first-seen order). Each test builds one big
//! table (above the 131072 default threshold), runs the query both ways,
//! and compares.

use rustqlite::{Database, Value};

const BIG: i64 = 300_000; // comfortably above the 131_072 default threshold

fn big_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, r REAL, cat INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    {
        // Multi-VALUES batches keep the load fast.
        let mut i = 0i64;
        while i < BIG {
            let hi = (i + 1000).min(BIG);
            let mut sql = String::from("INSERT INTO t (id, v, r, cat) VALUES ");
            for j in i..hi {
                if j > i {
                    sql.push(',');
                }
                // v spans negatives; r is exactly representable (halves);
                // 100 buckets, with the LAST 3 rows of every bucket NULL in
                // v (NULL handling in SUM/COUNT(col)/MIN/MAX).
                let v_null = (j % 100) >= 97 && j % 7 == 0;
                let v = if v_null {
                    "NULL".into()
                } else {
                    (j - BIG / 2).to_string()
                };
                let r = (j as f64) * 0.5;
                let cat = j % 100;
                sql.push_str(&format!("({}, {}, {}, {})", j, v, r, cat));
            }
            db.execute(&sql, []).unwrap();
            i = hi;
        }
    }
    db.execute("COMMIT", []).unwrap();
    db
}

fn rows_of(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, []).unwrap()
}

fn both_ways(sql: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let par = big_db();
    let ser = {
        let mut s = big_db();
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    (rows_of(&par, sql), rows_of(&ser, sql))
}

#[test]
fn parallel_aggregate_matches_serial() {
    let (par, ser) = both_ways(
        "SELECT COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v), SUM(r), AVG(r) FROM t",
    );
    assert_eq!(par.len(), 1);
    assert_eq!(par, ser, "parallel aggregate row must equal serial");
    // Spot-check known values (dense rowids, v = id - 150000 with NULLs).
    let sum_v = match &par[0][2] {
        Value::Integer(i) => *i,
        other => panic!("SUM(v) should be INTEGER, got {:?}", other),
    };
    // 300k rows, v = i - 150000, symmetric range -> sum ≈ 0 minus NULL rows.
    // NULL rows: v%100>=97 && v%7==0 (sparse) — just assert it is an int.
    let _ = sum_v;
    let count = match &par[0][0] {
        Value::Integer(i) => *i,
        other => panic!("COUNT(*) should be INTEGER, got {:?}", other),
    };
    assert_eq!(count, BIG);
    let count_v = match &par[0][1] {
        Value::Integer(i) => *i,
        other => panic!("COUNT(v) should be INTEGER, got {:?}", other),
    };
    assert!(count_v < BIG, "COUNT(v) must skip NULLs");
}

#[test]
fn parallel_filtered_aggregate_matches_serial() {
    let (par, ser) = both_ways("SELECT COUNT(*), SUM(v), AVG(v) FROM t WHERE v > 100000");
    assert_eq!(par, ser);
    let n = match &par[0][0] {
        Value::Integer(i) => *i,
        other => panic!("COUNT(*) INTEGER expected, got {:?}", other),
    };
    // v > 100000 means id > 250000 (of 0..300000) minus NULL rows.
    assert!(
        n > 40_000 && n <= 50_000,
        "unexpected filter cardinality: {n}"
    );
}

#[test]
fn parallel_groupby_matches_serial_including_order() {
    let (par, ser) = both_ways(
        "SELECT cat, COUNT(*), COUNT(v), SUM(v), MIN(v), MAX(v), AVG(r) FROM t GROUP BY cat",
    );
    assert_eq!(par.len(), 100, "100 buckets expected");
    assert_eq!(par, ser, "GROUP BY rows (incl. order) must equal serial");
    // The serial first-seen order = rowid order of first occurrence: cat
    // values 0..99 appear first at rowids 0..99.
    for (i, row) in par.iter().enumerate() {
        match &row[0] {
            Value::Integer(c) => assert_eq!(*c, i as i64, "group order = first-seen order"),
            other => panic!("cat INTEGER expected, got {:?}", other),
        }
    }
}

#[test]
fn parallel_groupby_with_having_and_order() {
    // Downstream stages (HAVING / ORDER BY / LIMIT) on the merged result.
    let (par, ser) = both_ways(
        "SELECT cat, SUM(v) FROM t GROUP BY cat HAVING SUM(v) > 0 ORDER BY SUM(v) DESC LIMIT 5",
    );
    assert_eq!(par.len(), 5);
    assert_eq!(par, ser);
}

#[test]
fn parallel_bare_count_star_matches() {
    let (par, ser) = both_ways("SELECT COUNT(*) FROM t");
    assert_eq!(par, ser);
    assert_eq!(par[0][0], Value::Integer(BIG));
}

#[test]
fn parallel_distinct_aggregate_matches() {
    // DISTINCT merge via set-union replay: 100 distinct cats over 300k rows.
    let (par, ser) = both_ways("SELECT COUNT(DISTINCT cat), COUNT(DISTINCT v) FROM t");
    assert_eq!(par, ser);
    match &par[0][0] {
        Value::Integer(i) => assert_eq!(*i, 100),
        other => panic!("COUNT(DISTINCT cat) INTEGER expected, got {:?}", other),
    }
}

#[test]
fn parallel_declines_inside_transaction() {
    // A query inside an open write transaction MUST take the serial path
    // (workers are foreign to the committed-view TLS) — and still be
    // correct read-your-own-writes.
    let mut db = big_db();
    db.execute("BEGIN", []).unwrap();
    db.execute(
        "INSERT INTO t (id, v, r, cat) VALUES (9999999, 1, 1.0, 1)",
        [],
    )
    .unwrap();
    let rows = db.query("SELECT COUNT(*), SUM(v) FROM t", []).unwrap();
    let count = match &rows[0][0] {
        Value::Integer(i) => *i,
        other => panic!("INTEGER expected, got {:?}", other),
    };
    assert_eq!(count, BIG + 1, "read-your-own-writes inside the txn");
    db.execute("ROLLBACK", []).unwrap();
}

#[test]
fn parallel_scan_pragma_roundtrip() {
    fn read(db: &Database) -> i64 {
        let rows = db.query("PRAGMA parallel_scan", []).unwrap();
        match &rows[0][0] {
            Value::Integer(i) => *i,
            other => panic!("INTEGER expected, got {:?}", other),
        }
    }
    let mut db = Database::open_in_memory().unwrap();
    assert_eq!(read(&db), 131_072, "default threshold");
    db.execute("PRAGMA parallel_scan=0", []).unwrap();
    assert_eq!(read(&db), 0, "disabled");
    db.execute("PRAGMA parallel_scan=ON", []).unwrap();
    assert_eq!(read(&db), 131_072, "ON restores the default");
    db.execute("PRAGMA parallel_scan=5000", []).unwrap();
    assert_eq!(read(&db), 5000, "custom threshold");
    db.execute("PRAGMA parallel_scan=OFF", []).unwrap();
    assert_eq!(read(&db), 0, "OFF");
}

#[test]
fn parallel_small_table_below_threshold() {
    // Below the threshold: results correct (serial path taken silently).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE s (v INTEGER)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..1000i64 {
        db.execute("INSERT INTO s (v) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let rows = db.query("SELECT COUNT(*), SUM(v) FROM s", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(1000));
    assert_eq!(rows[0][1], Value::Integer(499_500));
}

#[test]
fn parallel_with_rowid_gaps() {
    // Deleted rows leave rowid gaps — workers stay balanced on live bytes
    // and the results must still match serial exactly.
    let build = || {
        let mut db = big_db();
        db.execute("DELETE FROM t WHERE id % 3 = 0", []).unwrap();
        db
    };
    let par = build();
    let mut ser = build();
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();
    let a = rows_of(&par, "SELECT COUNT(*), SUM(v), MIN(v), MAX(v) FROM t");
    let b = rows_of(&ser, "SELECT COUNT(*), SUM(v), MIN(v), MAX(v) FROM t");
    assert_eq!(a, b, "gappy rowid space must merge correctly");
    match &a[0][0] {
        Value::Integer(i) => assert_eq!(*i, 200_000, "expected 2/3 of rows"),
        other => panic!("INTEGER expected, got {:?}", other),
    }
}

#[test]
fn parallel_groupby_multi_key() {
    // Two bare-column keys: the selective branch handles multi-key intern.
    let (par, ser) = both_ways("SELECT cat, v > 0, COUNT(*) FROM t GROUP BY cat, v > 0");
    assert_eq!(par, ser);
    assert!(par.len() >= 190, "bool subkeys split most buckets");
}

#[test]
fn parallel_matches_sqlite_on_big_aggregate() {
    // Cross-check the big-table aggregate against real SQLite (rusqlite,
    // bundled) — same data, same query, same answer.
    let (par, _) =
        both_ways("SELECT COUNT(*), SUM(v), MIN(v), MAX(v) FROM t WHERE v > -1000 AND v < 1000");
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, r REAL, cat INTEGER);")
        .unwrap();
    let mut stmt = conn
        .prepare("INSERT INTO t (id, v, r, cat) VALUES (?, ?, ?, ?)")
        .unwrap();
    for j in 0..BIG {
        let v_null = (j % 100) >= 97 && j % 7 == 0;
        let v: Option<i64> = if v_null { None } else { Some(j - BIG / 2) };
        stmt.execute(rusqlite::params![j, v, (j as f64) * 0.5, j % 100])
            .unwrap();
    }
    drop(stmt);
    let (n, sum, min, max): (i64, Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(*), SUM(v), MIN(v), MAX(v) FROM t WHERE v > -1000 AND v < 1000",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(par[0][0], Value::Integer(n));
    assert_eq!(par[0][1], sum.map(Value::Integer).unwrap_or(Value::Null));
    assert_eq!(par[0][2], min.map(Value::Integer).unwrap_or(Value::Null));
    assert_eq!(par[0][3], max.map(Value::Integer).unwrap_or(Value::Null));
}

// ============================================================================
// Parallel top-N (ORDER BY ... LIMIT) — the workers feed per-range
// keep-heaps; the merge must be BIT-IDENTICAL to the serial streaming
// top-N under the strict total order (key, then rowid).
// ============================================================================

#[test]
fn parallel_topn_asc_desc_match_serial() {
    // r is unique (halves of distinct rowids) — no tie latitude at all.
    for sql in [
        "SELECT * FROM t ORDER BY r DESC LIMIT 25",
        "SELECT * FROM t ORDER BY r ASC LIMIT 25",
    ] {
        let (par, ser) = both_ways(sql);
        assert_eq!(par.len(), 25);
        assert_eq!(par, ser, "parallel top-N must equal serial exactly");
    }
    // Spot-check the DESC extremes: the 25 largest r = 0.5 * (299999-i).
    let (par, _) = both_ways("SELECT id, r FROM t ORDER BY r DESC LIMIT 25");
    for (i, row) in par.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(BIG - 1 - i as i64));
    }
}

#[test]
fn parallel_topn_projection_matches_serial() {
    // Projection is a strict subset; the key column (r) is NOT projected.
    let (par, ser) = both_ways("SELECT id, v FROM t ORDER BY r DESC LIMIT 15");
    assert_eq!(par.len(), 15);
    assert_eq!(par, ser);
    assert_eq!(par[0].len(), 2, "projected width");
    assert_eq!(par[0][0], Value::Integer(BIG - 1));
}

#[test]
fn parallel_topn_offset_pagination_matches() {
    // Page 2 of an ASC ordering — the window is [offset, offset+count).
    let (par, ser) = both_ways("SELECT id, r FROM t ORDER BY r LIMIT 10 OFFSET 100");
    assert_eq!(par.len(), 10);
    assert_eq!(par, ser);
    assert_eq!(par[0][0], Value::Integer(100), "first row of page 2");
    assert_eq!(par[9][0], Value::Integer(109), "last row of page 2");
}

#[test]
fn parallel_topn_ties_rowid_order_matches_serial() {
    // cat has 3000 rows per value — massive ties. The strict total order
    // breaks ties by rowid ASC on BOTH directions (SQLite's stable
    // sorter semantics for scan-sourced rows); parallel must reproduce
    // the serial survivor set and order bit-for-bit.
    for sql in [
        "SELECT id, cat FROM t ORDER BY cat LIMIT 25",
        "SELECT id, cat FROM t ORDER BY cat DESC LIMIT 25",
    ] {
        let (par, ser) = both_ways(sql);
        assert_eq!(par, ser, "tied keys must tie-break by rowid identically");
        assert_eq!(par.len(), 25);
    }
    // ASC page 1: cat=0 rows, rowids 0,100,200,... (cat = id % 100).
    let (par, _) = both_ways("SELECT id, cat FROM t ORDER BY cat LIMIT 25");
    for (i, row) in par.iter().enumerate() {
        assert_eq!(row[1], Value::Integer(0));
        assert_eq!(row[0], Value::Integer(i as i64 * 100));
    }
}

#[test]
fn parallel_topn_null_keys_match_serial() {
    // v contains NULLs (NULLs sort first ASC, last DESC) — the NULL
    // ordering must survive the split+merge exactly.
    for sql in [
        "SELECT id, v FROM t ORDER BY v ASC LIMIT 12",
        "SELECT id, v FROM t ORDER BY v DESC LIMIT 12",
    ] {
        let (par, ser) = both_ways(sql);
        assert_eq!(par, ser);
        assert_eq!(par.len(), 12);
    }
}

#[test]
fn parallel_topn_rowid_alias_key() {
    // ORDER BY the rowid-alias column: resolve_topn_key_col maps it to
    // the alias column; DESC on the key of the table itself.
    let (par, ser) = both_ways("SELECT id, v FROM t ORDER BY id DESC LIMIT 10");
    assert_eq!(par, ser);
    assert_eq!(par[0][0], Value::Integer(BIG - 1));
}

#[test]
fn parallel_topn_matches_sqlite_unique_keys() {
    // Cross-check against real SQLite on UNIQUE keys (r halves; and the
    // top v values, which are distinct). On unique keys even the row
    // order is contract, not implementation detail.
    let (par_r, _) = both_ways("SELECT id, v, r, cat FROM t ORDER BY r DESC LIMIT 20");
    let (par_v, _) = both_ways("SELECT id, v FROM t ORDER BY v DESC LIMIT 20");
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, r REAL, cat INTEGER);")
        .unwrap();
    let mut stmt = conn
        .prepare("INSERT INTO t (id, v, r, cat) VALUES (?, ?, ?, ?)")
        .unwrap();
    for j in 0..BIG {
        let v_null = (j % 100) >= 97 && j % 7 == 0;
        let v: Option<i64> = if v_null { None } else { Some(j - BIG / 2) };
        stmt.execute(rusqlite::params![j, v, (j as f64) * 0.5, j % 100])
            .unwrap();
    }
    drop(stmt);
    let sql_r: Vec<(i64, Option<i64>, f64, i64)> = conn
        .prepare("SELECT id, v, r, cat FROM t ORDER BY r DESC LIMIT 20")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(par_r.len(), sql_r.len());
    for (a, b) in par_r.iter().zip(sql_r.iter()) {
        assert_eq!(a[0], Value::Integer(b.0));
        assert_eq!(a[1], b.1.map(Value::Integer).unwrap_or(Value::Null));
        assert_eq!(a[2], Value::Real(b.2));
        assert_eq!(a[3], Value::Integer(b.3));
    }
    let sql_v: Vec<(i64, Option<i64>)> = conn
        .prepare("SELECT id, v FROM t ORDER BY v DESC LIMIT 20")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(par_v.len(), sql_v.len());
    for (a, b) in par_v.iter().zip(sql_v.iter()) {
        assert_eq!(a[0], Value::Integer(b.0));
        assert_eq!(a[1], b.1.map(Value::Integer).unwrap_or(Value::Null));
    }
}

#[test]
fn parallel_topn_declines_inside_transaction() {
    // Read-your-own-writes: inside an open write txn the workers decline
    // (serial path) — a freshly inserted extreme row must win the DESC
    // top-1 immediately.
    let mut db = big_db();
    db.execute("BEGIN", []).unwrap();
    db.execute(
        "INSERT INTO t (id, v, r, cat) VALUES (9999999, 999999, 5e10, 7)",
        [],
    )
    .unwrap();
    let rows = db
        .query("SELECT id, v FROM t ORDER BY v DESC LIMIT 3", [])
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Integer(9_999_999), "the new max wins");
    assert_eq!(rows[0][1], Value::Integer(999_999));
    db.execute("ROLLBACK", []).unwrap();
}

#[test]
fn parallel_topn_declines_below_threshold() {
    // A small table: the fusion still answers via the serial streaming
    // path — correctness independent of the split.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..500i64 {
        db.execute("INSERT INTO s (v) VALUES (?)", [Value::Integer(i * 7 % 97)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let rows = db
        .query("SELECT id, v FROM s ORDER BY v DESC, id ASC LIMIT 5", [])
        .unwrap();
    // Multi-term ORDER BY keeps the general sort path — still correct.
    assert_eq!(rows.len(), 5);
    let first_v = match rows[0][1] {
        Value::Integer(i) => i,
        ref other => panic!("INTEGER expected, got {:?}", other),
    };
    assert_eq!(first_v, 96, "96 = 7*k mod 97 max, first at id 14");
}

// ========================================================================
// GROUP BY streaming driver + fused Project-over-Aggregate (2026-09-07
// memory pass): the statement path finalizes groups in serving batches
// from the owned grouper (GroupByDriver), and the executor's
// Project-over-Aggregate fusion emits the final projected rows directly.
// Both must be result-identical to the materialized path — same rows,
// same first-seen order, same column names.
// ========================================================================

/// Sentinel for "insert NULL here" in the test tables below.
const SENTINEL_NULL: &str = "NULL_SENTINEL";

#[test]
fn groupby_driver_matches_materialized_path() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, v INTEGER)",
        [],
    )
    .unwrap();
    for (cat, v) in [
        ("a", 1),
        ("b", 5),
        ("a", 2),
        (SENTINEL_NULL, 9),
        ("b", 3),
        ("c", 7),
        (SENTINEL_NULL, 1),
        ("a", 4),
    ] {
        let cat_val = if cat == SENTINEL_NULL {
            Value::Null
        } else {
            Value::Text(cat.into())
        };
        db.execute(
            "INSERT INTO t (cat, v) VALUES (?, ?)",
            [cat_val, Value::Integer(v)],
        )
        .unwrap();
    }
    let sql = "SELECT cat, COUNT(*), SUM(v), MIN(v), MAX(v) FROM t GROUP BY cat";
    // Materialized path (db.query — executor fusion).
    let (cols_q, rows_q) = db.query_with_columns(sql, []).unwrap();
    // Streaming driver path (prepare/step — GroupByDriver).
    let mut stmt = db.prepare(sql).unwrap();
    let mut rows_s: Vec<rustqlite::Row> = Vec::new();
    while stmt.step().unwrap() == rustqlite::StepResult::Row {
        if let Some(r) = stmt.row() {
            rows_s.push(r.clone());
        }
    }
    assert_eq!(rows_q, rows_s, "driver rows must equal materialized rows");
    let cols_s: Vec<String> = (0..stmt.column_count())
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();
    assert_eq!(cols_q, cols_s, "driver column names must match");

    // First-seen order: a, b, NULL, c.
    assert_eq!(rows_q.len(), 4);
    assert_eq!(rows_q[0][0], Value::Text("a".into()));
    assert_eq!(rows_q[1][0], Value::Text("b".into()));
    assert_eq!(rows_q[2][0], Value::Null);
    assert_eq!(rows_q[3][0], Value::Text("c".into()));
    // a: count 3, sum 7, min 1, max 4.
    assert_eq!(rows_q[0][1], Value::Integer(3));
    assert_eq!(rows_q[0][2], Value::Integer(7));
    assert_eq!(rows_q[0][3], Value::Integer(1));
    assert_eq!(rows_q[0][4], Value::Integer(4));
}

#[test]
fn groupby_driver_arithmetic_key_and_aliases() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=200i64 {
        db.execute("INSERT INTO t (v) VALUES (?)", [Value::Integer(i * 3)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    // Arithmetic key (compiled GROUP BY path) + alias + COUNT only — the
    // S03 shape, served through the streaming driver.
    let sql = "SELECT v / 30 AS bucket, COUNT(*) AS n FROM t GROUP BY v / 30";
    let (cols_q, rows_q) = db.query_with_columns(sql, []).unwrap();
    let mut stmt = db.prepare(sql).unwrap();
    let mut rows_s = Vec::new();
    while stmt.step().unwrap() == rustqlite::StepResult::Row {
        if let Some(r) = stmt.row() {
            rows_s.push(r.clone());
        }
    }
    assert_eq!(rows_q, rows_s);
    let cols_s: Vec<String> = (0..stmt.column_count())
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();
    assert_eq!(cols_q, cols_s);
    assert_eq!(cols_q, vec!["bucket".to_string(), "n".to_string()]);
    // v=3..600 (i*3, i=1..200) → floor(v/30) buckets 0..20: buckets 0..19
    // hold 10 rows each except bucket 0 (v=3..27 → 9 rows), bucket 20
    // holds only v=600.
    assert_eq!(rows_q.len(), 21);
    let last = rows_q.last().unwrap();
    assert_eq!(last[0], Value::Integer(20));
    assert_eq!(last[1], Value::Integer(1));
    let first = rows_q.first().unwrap();
    assert_eq!(first[0], Value::Integer(0));
    assert_eq!(first[1], Value::Integer(9));
}

#[test]
fn groupby_driver_multi_key_and_concat() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, s TEXT)",
        [],
    )
    .unwrap();
    for (a, b, s) in [
        ("x", 1, "p"),
        ("y", 2, "q"),
        ("x", 1, "r"),
        ("x", 2, "s"),
        ("y", 2, "t"),
        ("x", 1, "u"),
    ] {
        db.execute(
            "INSERT INTO t (a, b, s) VALUES (?, ?, ?)",
            [
                Value::Text(a.into()),
                Value::Integer(b),
                Value::Text(s.into()),
            ],
        )
        .unwrap();
    }
    // Multi-key + GROUP_CONCAT + DISTINCT: exercises the multi-key intern
    // path, the cold AggState half, and the DISTINCT replay — through BOTH
    // serving paths.
    let sql = "SELECT a, b, GROUP_CONCAT(s), COUNT(DISTINCT b) FROM t GROUP BY a, b";
    let (_, rows_q) = db.query_with_columns(sql, []).unwrap();
    let mut stmt = db.prepare(sql).unwrap();
    let mut rows_s = Vec::new();
    while stmt.step().unwrap() == rustqlite::StepResult::Row {
        if let Some(r) = stmt.row() {
            rows_s.push(r.clone());
        }
    }
    assert_eq!(rows_q, rows_s);
    // (x,1): p,r,u; (y,2): q,t; (x,2): s — first-seen order.
    assert_eq!(rows_q.len(), 3);
    assert_eq!(rows_q[0][2], Value::Text("p,r,u".into()));
    assert_eq!(rows_q[1][2], Value::Text("q,t".into()));
    assert_eq!(rows_q[2][2], Value::Text("s".into()));
    assert_eq!(rows_q[0][3], Value::Integer(1));
}

#[test]
fn groupby_fused_projection_survives_wrappers() {
    // LIMIT / ORDER BY over the fused GROUP BY shape: the fusion is inside
    // the executor, wrappers (Sort, Limit) must observe identical rows.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1000i64 {
        db.execute("INSERT INTO t (g) VALUES (?)", [Value::Integer(i % 37)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    for sql in [
        "SELECT g, COUNT(*) FROM t GROUP BY g ORDER BY g DESC LIMIT 5",
        "SELECT g, COUNT(*) c FROM t GROUP BY g HAVING COUNT(*) > 27 ORDER BY c DESC, g",
        "SELECT COUNT(*), g FROM t GROUP BY g", // aggregate first in projection
    ] {
        let (cols_q, rows_q) = db.query_with_columns(sql, []).unwrap();
        let mut stmt = db.prepare(sql).unwrap();
        let mut rows_s = Vec::new();
        while stmt.step().unwrap() == rustqlite::StepResult::Row {
            if let Some(r) = stmt.row() {
                rows_s.push(r.clone());
            }
        }
        assert_eq!(rows_q, rows_s, "mismatch for {sql}");
        let cols_s: Vec<String> = (0..stmt.column_count())
            .map(|i| stmt.column_name(i).unwrap().to_string())
            .collect();
        assert_eq!(cols_q, cols_s, "columns mismatch for {sql}");
    }
}

// ============================================================================
// Parallel unbounded ORDER BY — chunk sort + k-way range-ordered merge
// ============================================================================

#[test]
fn parallel_order_by_matches_serial() {
    // Unbounded, multi-term, mixed ASC/DESC, ties in v (v = id % 1000
    // repeats), REAL and INTEGER key columns.
    let (par, ser) = both_ways("SELECT id, v, r, cat FROM t ORDER BY v DESC, r ASC, id");
    assert_eq!(par.len(), BIG as usize);
    assert_eq!(
        par, ser,
        "parallel ORDER BY must equal the serial stable sort"
    );
}

#[test]
fn parallel_order_by_desc_single_key_with_ties() {
    // Heavy ties: ORDER BY cat (100 buckets over 300k rows) — the rowid
    // tiebreak must reproduce scan order inside every tie class.
    let (par, ser) = both_ways("SELECT id, cat FROM t ORDER BY cat DESC");
    assert_eq!(par.len(), BIG as usize);
    assert_eq!(par, ser);

    // Spot-check the tie discipline: within one cat class, rowids ascend.
    let first_cat = match &par[0][1] {
        Value::Integer(c) => *c,
        other => panic!("cat must be INTEGER, got {other:?}"),
    };
    let mut prev_rowid = i64::MIN;
    for row in par.iter().take_while(|r| r[1] == Value::Integer(first_cat)) {
        let id = match &row[0] {
            Value::Integer(i) => *i,
            other => panic!("id must be INTEGER, got {other:?}"),
        };
        assert!(id >= prev_rowid, "ties must keep rowid (scan) order");
        prev_rowid = id;
    }
}

#[test]
fn parallel_order_by_real_key_matches_serial() {
    let (par, ser) = both_ways("SELECT id, r FROM t ORDER BY r, id DESC");
    assert_eq!(par.len(), BIG as usize);
    assert_eq!(par, ser);
}

#[test]
fn parallel_order_by_ordinal_and_alias() {
    // Ordinal keys + an aliased scan (prefix-qualified column refs).
    let (par, ser) = both_ways("SELECT id, v FROM t AS x ORDER BY 2 DESC, 1");
    assert_eq!(par, ser);
    let (par, ser) = both_ways("SELECT x.id, x.v FROM t AS x ORDER BY x.v, x.id DESC");
    assert_eq!(par, ser);
}

#[test]
fn parallel_order_by_with_projection_above() {
    // Project(Sort(Scan)) shape: the projection consumes the sorted scan.
    let (par, ser) = both_ways("SELECT v, r FROM t ORDER BY v DESC, r, id");
    assert_eq!(par.len(), BIG as usize);
    assert_eq!(par, ser);
}

#[test]
fn parallel_order_by_hidden_rowid_table() {
    // No INTEGER PRIMARY KEY: the scan appends the hidden rowid slot;
    // ORDER BY rowid exercises the trailing-slot key.
    fn build() -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE h (a INTEGER, b TEXT)", [])
            .unwrap();
        db.execute("BEGIN", []).unwrap();
        let mut i = 0i64;
        while i < BIG {
            let hi = (i + 1000).min(BIG);
            let mut sql = String::from("INSERT INTO h (a, b) VALUES ");
            for j in i..hi {
                if j > i {
                    sql.push(',');
                }
                // a ties heavily (1000 distinct), b ties within a.
                sql.push_str(&format!("({}, 'k{}')", j % 1000, j % 10));
            }
            db.execute(&sql, []).unwrap();
            i = hi;
        }
        db.execute("COMMIT", []).unwrap();
        db
    }
    let par = build();
    let mut ser = build();
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();

    let q1 = "SELECT a, b FROM h ORDER BY a, b";
    assert_eq!(
        rows_of(&par, q1),
        rows_of(&ser, q1),
        "hidden-rowid: ORDER BY a, b"
    );

    let q2 = "SELECT a, b FROM h ORDER BY rowid DESC";
    assert_eq!(
        rows_of(&par, q2),
        rows_of(&ser, q2),
        "hidden-rowid: ORDER BY rowid DESC"
    );

    let q3 = "SELECT a, b, rowid FROM h ORDER BY a DESC, rowid";
    assert_eq!(
        rows_of(&par, q3),
        rows_of(&ser, q3),
        "hidden-rowid: projected rowid"
    );
}

// ============================================================================
// Fused aggregate filters with bound parameters
// ============================================================================

fn both_ways_bound(sql: &str, params: [Value; 1]) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let par = big_db();
    let mut ser = big_db();
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();
    let a = par.query(sql, params.clone()).unwrap();
    let b = ser.query(sql, params).unwrap();
    (a, b)
}

#[test]
fn parallel_fused_aggregate_param_filter_matches_serial() {
    // `WHERE v > ?`: the fused filter materializes the parameter ONCE
    // (positional `?`), and the worker-split walk must return exactly
    // the serial values.
    let (a, b) = both_ways_bound(
        "SELECT COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM t WHERE v > ?",
        [Value::Integer(100_000)],
    );
    assert_eq!(a, b, "param-filtered fused aggregate: parallel == serial");
    let n = match &a[0][0] {
        Value::Integer(n) => *n,
        other => panic!("COUNT(*) INTEGER expected, got {other:?}"),
    };
    assert!(n > 40_000 && n <= 50_000, "unexpected cardinality {n}");

    // Same bound value, literal spelling — both paths must agree.
    let (lit, _) = both_ways("SELECT COUNT(*), SUM(v), AVG(v) FROM t WHERE v > 100000");
    assert_eq!(a[0][0], lit[0][0]);
    assert_eq!(a[0][1], lit[0][1]);
    assert_eq!(a[0][2], lit[0][2]);
}

#[test]
fn fused_aggregate_param_filter_swapped_and_eq() {
    // Swapped operand order (`? < v`) and an equality param filter.
    let (a, b) = both_ways_bound(
        "SELECT COUNT(*), AVG(v) FROM t WHERE 100000 < v",
        [Value::Integer(0)], // unused — shapes use literals here
    );
    assert_eq!(a, b);

    let (a, b) = both_ways_bound(
        "SELECT COUNT(*), SUM(r), AVG(r) FROM t WHERE cat = ?",
        [Value::Integer(42)],
    );
    assert_eq!(a, b, "param equality filter");
    let n = match &a[0][0] {
        Value::Integer(n) => *n,
        other => panic!("COUNT(*) INTEGER expected, got {other:?}"),
    };
    assert_eq!(n, 3000, "cat = 42 over 300k rows in 100 buckets");
}

#[test]
fn fused_aggregate_named_param_filter() {
    // Named parameter materialization (`:lim`) through the prepared-
    // statement bind path: the fused filter resolves the named value
    // once, and parallel == serial.
    use rustqlite::{StepResult, Value as V};

    fn run(db: &mut Database, bind: fn(&mut rustqlite::Statement, V)) -> Vec<Vec<Value>> {
        let mut stmt = db
            .prepare("SELECT COUNT(*), SUM(v) FROM t WHERE v > :lim")
            .unwrap();
        bind(&mut stmt, V::Integer(50_000));
        let mut rows = Vec::new();
        while stmt.step().unwrap() == StepResult::Row {
            rows.push(stmt_row(&stmt));
        }
        rows
    }
    fn stmt_row(stmt: &rustqlite::Statement) -> Vec<Value> {
        (0..stmt.column_count())
            .map(|i| stmt.column_value(i).cloned().unwrap_or(Value::Null))
            .collect()
    }

    let mut par = big_db();
    let mut ser = big_db();
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();
    let a = run(&mut par, |s, v| {
        s.bind_named(":lim", v).unwrap();
    });
    let b = run(&mut ser, |s, v| {
        s.bind_named(":lim", v).unwrap();
    });
    assert_eq!(a, b, "named-param fused filter: parallel == serial");
    // v > 50000 over 300k rows = 100000 rows minus the sparse NULL-v
    // rows the generator plants ((j % 100) >= 97 && j % 7 == 0).
    let n = match &a[0][0] {
        Value::Integer(n) => *n,
        other => panic!("COUNT(*) INTEGER expected, got {other:?}"),
    };
    assert!((99_000..=100_000).contains(&n), "unexpected count {n}");
    let _ = V::Null;
}

#[test]
fn fused_aggregate_null_param_filter_declines_cleanly() {
    // A NULL (or text) param filter must decline to the generic path and
    // still produce the correct (empty) result, serial and parallel.
    let par = big_db();
    let mut ser = big_db();
    ser.execute("PRAGMA parallel_scan=0", []).unwrap();
    let a = par
        .query("SELECT COUNT(*), SUM(v) FROM t WHERE v > ?", [Value::Null])
        .unwrap();
    let b = ser
        .query("SELECT COUNT(*), SUM(v) FROM t WHERE v > ?", [Value::Null])
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(
        a[0][0],
        Value::Integer(0),
        "NULL comparison filters nothing"
    );
}

#[test]
fn parallel_fused_aggregate_expression_arg_declines() {
    // `total(v * 1.0)` is not a bare-column arg: the fused machine must
    // decline — the parallel plan once accepted it as a COUNT(*)-style
    // placeholder (slot None = NumVal::I(1)) and summed 1-per-row,
    // 300000 instead of ~0. Surfaced by the numeric-parity suite
    // against real SQLite; the serial fused path always declined it.
    let (par, ser) = both_ways("SELECT total(v * 1.0), sum(v + 0) FROM t");
    assert_eq!(
        par, ser,
        "expression-arg aggregate must fall back identically"
    );
    let tot = match &par[0][0] {
        Value::Real(f) => *f,
        Value::Integer(i) => *i as f64,
        other => panic!("total must be numeric, got {other:?}"),
    };
    assert!(
        (tot - par[0][1].as_real()).abs() < 1e-6,
        "total(v*1.0) must equal sum(v+0) numerically: {tot} vs {}",
        par[0][1].as_real()
    );
    assert!(
        tot.abs() > 1000.0,
        "must not be a 1-per-row placeholder: {tot}"
    );
}

// ---------------------------------------------------------------------------
// Parallel sort with COLLATE terms: `ORDER BY name COLLATE NOCASE, ...`
// splits across workers exactly like the bare-column sort, with the
// collation NAME riding the key tuple (the comparator resolves it per
// comparison — the same lookup + fallback the serial path uses, so the
// orders cannot diverge).
// ---------------------------------------------------------------------------

fn text_db() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER)",
        [],
    )
    .unwrap();
    db.execute("BEGIN", []).unwrap();
    {
        let mut i = 0i64;
        while i < BIG {
            let hi = (i + 1000).min(BIG);
            let mut sql = String::from("INSERT INTO t (id, name, v) VALUES ");
            for j in i..hi {
                if j > i {
                    sql.push(',');
                }
                // Mixed case so NOCASE actually reorders vs BINARY;
                // shared prefixes force multi-term tie-breaks.
                let name = match j % 5 {
                    0 => format!("'user-{:05}'", j % 1000),
                    1 => format!("'USER-{:05}'", j % 1000),
                    2 => format!("'User-{:05}'", j % 1000),
                    3 => format!("'uSeR-{:05}'", j % 1000),
                    _ => format!("'UsEr-{:05}'", j % 1000),
                };
                sql.push_str(&format!("({}, {}, {})", j, name, -j));
            }
            db.execute(&sql, []).unwrap();
            i = hi;
        }
    }
    db.execute("COMMIT", []).unwrap();
    db
}

#[test]
fn parallel_sort_collate_matches_serial() {
    let queries = [
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE",
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE DESC",
        "SELECT id, name, v FROM t ORDER BY name COLLATE NOCASE, v DESC",
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE DESC, id",
        "SELECT id, name FROM t ORDER BY name COLLATE RTRIM, id",
    ];
    for sql in queries {
        let par = text_db();
        let ser = {
            let mut s = text_db();
            s.execute("PRAGMA parallel_scan=0", []).unwrap();
            s
        };
        let a = rows_of(&par, sql);
        let b = rows_of(&ser, sql);
        assert_eq!(a.len(), BIG as usize, "{sql}: row count");
        assert_eq!(a, b, "{sql}: parallel COLLATE sort must equal serial");
        // The first block under NOCASE must group the mixed-case
        // spellings of the same name together (a BINARY order would
        // not) — spot-check the ordering actually used the collation.
        let sql_head = format!("{sql} LIMIT 400");
        let head = rows_of(&par, &sql_head);
        let mut sorted_groups = 0;
        let mut prev: Option<String> = None;
        for r in &head {
            let key = match &r[1] {
                Value::Text(t) => t.as_str().to_lowercase(),
                other => panic!("{sql}: TEXT expected, got {other:?}"),
            };
            if prev.as_deref() != Some(key.as_str()) {
                sorted_groups += 1;
            }
            prev = Some(key);
        }
        // 400 rows over ~200 distinct no-case keys (j % 1000 with 5-case
        // groups): NOCASE collapses each key's 5 spellings adjacently.
        assert!(
            sorted_groups < 400,
            "{sql}: NOCASE must collapse mixed-case spellings (got {sorted_groups} groups in 400 rows)"
        );
    }
}

#[test]
fn parallel_sort_unknown_collation_falls_back_like_serial() {
    // A COLLATE name that is not registered: BOTH paths fall back to
    // the connection comparator (the serial comparator's
    // unwrap_or_else and the parallel comparator's None arm) — the
    // results must still match each other.
    let sql = "SELECT id, name FROM t ORDER BY name COLLATE NOSUCHCOLL, id";
    let par = text_db();
    let ser = {
        let mut s = text_db();
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let a = rows_of(&par, sql);
    let b = rows_of(&ser, sql);
    assert_eq!(a, b, "unknown collation fallback must match serial");
}

// ---------------------------------------------------------------------------
// Collated top-N fusion: `ORDER BY <col> [COLLATE x] [DESC] LIMIT k` — the
// fusion gate unwraps the Collate wrapper (explicit AND declared), the
// resolved collation threads through the keep-heap discipline, serial
// streaming and worker split alike.
// ---------------------------------------------------------------------------

#[test]
fn parallel_topn_collate_battery_matches_serial() {
    let queries = [
        // explicit COLLATE, ASC
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 300",
        // explicit COLLATE, DESC
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE DESC LIMIT 300",
        // OFFSET window (total 1000 — still fused)
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 250 OFFSET 750",
        // RTRIM collation
        "SELECT id, name FROM t ORDER BY name COLLATE RTRIM LIMIT 300",
        // key outside the projection (fused Project shape)
        "SELECT id, v FROM t ORDER BY name COLLATE NOCASE LIMIT 300",
        // multi-column projection
        "SELECT id, name, v FROM t ORDER BY name COLLATE NOCASE LIMIT 300",
        // tiny keep
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 3",
        // unregistered collation: both paths fall back to the connection
        // comparator (None arm == serial comparator's unwrap_or_else)
        "SELECT id, name FROM t ORDER BY name COLLATE NOSUCHCOLL LIMIT 300",
    ];
    for sql in queries {
        let par = text_db();
        let ser = {
            let mut s = text_db();
            s.execute("PRAGMA parallel_scan=0", []).unwrap();
            s
        };
        let a = rows_of(&par, sql);
        let b = rows_of(&ser, sql);
        assert_eq!(a, b, "{sql}: parallel collated top-N must equal serial");
        // NOCASE monotonicity spot check: successive keys never decrease
        // (never increase for DESC) under the collation's order. Only for
        // projections that carry the key column (the key-outside-
        // projection shape has no key to observe — its equivalence to
        // serial is the assertion above).
        let proj = sql.split(" FROM ").next().unwrap_or("");
        let key_in_output = proj.contains("name");
        if key_in_output {
            let desc = sql.contains("DESC");
            let mut prev: Option<String> = None;
            for r in &a {
                let key = match &r[1] {
                    Value::Text(t) => {
                        if sql.contains("NOSUCHCOLL") {
                            t.as_str().to_string()
                        } else {
                            t.as_str().to_lowercase()
                        }
                    }
                    other => panic!("{sql}: TEXT key expected, got {other:?}"),
                };
                if let Some(p) = &prev {
                    let ok = if desc { key <= *p } else { key >= *p };
                    assert!(
                        ok,
                        "{sql}: ordering not monotonic at key {key:?} after {p:?}"
                    );
                }
                prev = Some(key);
            }
        }
    }
}

#[test]
fn parallel_topn_collate_actually_collates() {
    let db = text_db();
    // First/last key identity under NOCASE: the minimum no-case key is
    // 'user-00000' (any spelling), the maximum is 'user-00999'.
    let first = rows_of(
        &db,
        "SELECT name FROM t ORDER BY name COLLATE NOCASE LIMIT 1",
    );
    assert_eq!(first.len(), 1);
    match &first[0][0] {
        Value::Text(t) => assert_eq!(
            t.as_str().to_ascii_lowercase(),
            "user-00000",
            "NOCASE minimum key expected"
        ),
        other => panic!("TEXT expected, got {other:?}"),
    }
    let last = rows_of(
        &db,
        "SELECT name FROM t ORDER BY name COLLATE NOCASE DESC LIMIT 1",
    );
    match &last[0][0] {
        Value::Text(t) => assert_eq!(
            t.as_str().to_ascii_lowercase(),
            "user-00999",
            "NOCASE maximum key expected"
        ),
        other => panic!("TEXT expected, got {other:?}"),
    }
    // Tie block: the minimum no-case key has 300 rows (rowids
    // 0, 1000, ..., 299000). Ties break by rowid ASC on BOTH ASC and
    // DESC terms — pin both.
    let rows = rows_of(
        &db,
        "SELECT id FROM t ORDER BY name COLLATE NOCASE LIMIT 300",
    );
    let expected: Vec<Vec<Value>> = (0..300u32)
        .map(|k| vec![Value::Integer(k as i64 * 1000)])
        .collect();
    assert_eq!(
        rows, expected,
        "300-row NOCASE tie block must keep rowid ASC order (ASC term)"
    );
    let rows_desc = rows_of(
        &db,
        "SELECT id FROM t ORDER BY name COLLATE NOCASE DESC LIMIT 300",
    );
    let expected_desc: Vec<Vec<Value>> = (0..300u32)
        .map(|k| vec![Value::Integer(999 + k as i64 * 1000)])
        .collect();
    assert_eq!(
        rows_desc, expected_desc,
        "300-row NOCASE tie block must keep rowid ASC order (DESC term)"
    );
}

#[test]
fn parallel_topn_collate_fused_matches_materializing() {
    // The fused streaming path caps at keep <= 4096; a bigger bound runs
    // the materializing exec_topn with the same collated comparator. The
    // fused prefix must equal the materializing prefix bit-for-bit.
    let db = text_db();
    let fused = rows_of(
        &db,
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 250",
    );
    let materializing = rows_of(
        &db,
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 5000",
    );
    assert_eq!(fused.len(), 250);
    assert_eq!(materializing.len(), 5000);
    assert_eq!(
        fused,
        materializing[..250].to_vec(),
        "fused top-N prefix must equal the materializing path"
    );
    // OFFSET windows: same comparison through a windowed slice.
    let fused_off = rows_of(
        &db,
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 250 OFFSET 1000",
    );
    let materializing_off = rows_of(
        &db,
        "SELECT id, name FROM t ORDER BY name COLLATE NOCASE LIMIT 5000 OFFSET 1000",
    );
    assert_eq!(
        fused_off,
        materializing_off[..250].to_vec(),
        "fused OFFSET window must equal the materializing window"
    );
}

#[test]
fn parallel_topn_declared_collation_fuses() {
    // A DECLARED column collation arrives at the fusion as the same
    // Collate wrapper (resolve_order_by_terms attaches it), so bare
    // `ORDER BY k LIMIT n` must fuse AND collate — parallel == serial,
    // NULLs first (the value order routes non-TEXT pairs around the
    // collation), then NOCASE key order.
    let build = || {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT COLLATE NOCASE, v INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        {
            let mut i = 0i64;
            while i < 150_000 {
                let hi = (i + 1000).min(150_000);
                let mut sql = String::from("INSERT INTO t (id, k, v) VALUES ");
                for j in i..hi {
                    if j > i {
                        sql.push(',');
                    }
                    // 5-case spellings over 30k distinct keys -> 5 rows
                    // per no-case key; a NULL cluster at 40000..41000.
                    let k = if (40_000..41_000).contains(&j) {
                        "NULL".to_string()
                    } else {
                        match j % 5 {
                            0 => format!("'key-{:05}'", j % 30_000),
                            1 => format!("'KEY-{:05}'", j % 30_000),
                            2 => format!("'Key-{:05}'", j % 30_000),
                            3 => format!("'kEy-{:05}'", j % 30_000),
                            _ => format!("'KeY-{:05}'", j % 30_000),
                        }
                    };
                    sql.push_str(&format!("({}, {}, {})", j, k, -j));
                }
                db.execute(&sql, []).unwrap();
                i = hi;
            }
        }
        db.execute("COMMIT", []).unwrap();
        db
    };
    let par = build();
    let ser = {
        let mut s = build();
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let sql = "SELECT id, k FROM t ORDER BY k LIMIT 500";
    let a = rows_of(&par, sql);
    let b = rows_of(&ser, sql);
    assert_eq!(a, b, "declared-collation top-N must match serial");
    // NULL cluster first: the 1000 NULL keys precede every TEXT key.
    assert!(
        a.iter().all(|r| matches!(&r[1], Value::Null)),
        "the first 500 rows must be the NULL-key cluster (NULL sorts first)"
    );
    // After the NULLs, NOCASE order: probe past the cluster.
    let after = rows_of(&par, "SELECT id, k FROM t ORDER BY k LIMIT 5 OFFSET 1000");
    for r in &after {
        match &r[1] {
            Value::Text(t) => assert_eq!(
                t.as_str().to_ascii_lowercase(),
                "key-00000",
                "first non-NULL no-case key expected"
            ),
            other => panic!("TEXT expected after the NULL cluster, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// No-GROUP-BY DISTINCT aggregates: per-range AggState sets + range-ordered
// set-union merge (parallel::try_parallel_distinct_aggregate). The battery
// covers mixed distinct/non-distinct lists, every allowed function, and
// compiled-filter shapes; TEXT distinct and NULL-degenerate tables pin the
// semantics against the serial paths.
// ---------------------------------------------------------------------------

#[test]
fn parallel_distinct_aggregate_battery_matches_serial() {
    // One big_db pair for the whole battery (building 300k rows per query
    // would dominate the runtime).
    let par = big_db();
    let ser = {
        let mut s = big_db();
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let queries = [
        "SELECT COUNT(DISTINCT cat), COUNT(DISTINCT v) FROM t",
        "SELECT SUM(DISTINCT v) FROM t",
        "SELECT COUNT(DISTINCT cat), SUM(DISTINCT v), MIN(DISTINCT v), MAX(DISTINCT v) FROM t",
        "SELECT AVG(DISTINCT v) FROM t",
        "SELECT TOTAL(DISTINCT v) FROM t",
        // mixed: DISTINCT and non-DISTINCT in one list
        "SELECT COUNT(DISTINCT cat), SUM(v), COUNT(*) FROM t",
        // compiled filters
        "SELECT COUNT(DISTINCT cat) FROM t WHERE v > 100000",
        "SELECT SUM(DISTINCT v), COUNT(DISTINCT cat) FROM t WHERE cat < 50",
        "SELECT MIN(DISTINCT v), MAX(DISTINCT v) FROM t WHERE cat >= 90",
    ];
    for sql in queries {
        let a = rows_of(&par, sql);
        let b = rows_of(&ser, sql);
        assert_eq!(a.len(), 1, "{sql}: one aggregate row");
        assert_eq!(a, b, "{sql}: parallel DISTINCT must equal serial");
    }
    // Ground truths (independent of the engine's DISTINCT machinery):
    // cat = j % 100 over dense rowids -> exactly 100 distinct values.
    match &rows_of(&par, "SELECT COUNT(DISTINCT cat) FROM t")[0][0] {
        Value::Integer(i) => assert_eq!(*i, 100, "COUNT(DISTINCT cat) ground truth"),
        other => panic!("INTEGER expected, got {other:?}"),
    }
    // COUNT(DISTINCT v): every row's v = id - 150000 is unique except the
    // NULL rows (v IS NULL); distinct-count == non-NULL row count.
    let row = &rows_of(&par, "SELECT COUNT(DISTINCT v), COUNT(v) FROM t")[0];
    let distinct_v = match &row[0] {
        Value::Integer(i) => *i,
        other => panic!("INTEGER expected, got {other:?}"),
    };
    let count_v = match &row[1] {
        Value::Integer(i) => *i,
        other => panic!("INTEGER expected, got {other:?}"),
    };
    assert_eq!(distinct_v, count_v, "DISTINCT v count == non-NULL v count");
    // MIN/MAX(DISTINCT v) == MIN/MAX(v) (distinct never changes extremes).
    let row = &rows_of(
        &par,
        "SELECT MIN(DISTINCT v), MAX(DISTINCT v), MIN(v), MAX(v) FROM t",
    )[0];
    assert_eq!(row[0], row[2], "MIN(DISTINCT v) == MIN(v)");
    assert_eq!(row[1], row[3], "MAX(DISTINCT v) == MAX(v)");
    // SUM(DISTINCT v) == SUM over the deduplicated subquery (a completely
    // different execution path: materialize + dedup + aggregate).
    let a = rows_of(&par, "SELECT SUM(DISTINCT v) FROM t");
    let b = rows_of(&par, "SELECT SUM(v) FROM (SELECT DISTINCT v FROM t)");
    assert_eq!(a, b, "SUM(DISTINCT v) == subquery-dedup SUM(v)");
}

#[test]
fn parallel_distinct_text_matches_serial() {
    // text_db: name = user-{j%1000} with the spelling chosen by j%5 —
    // 1000 % 5 == 0, so each key arrives with exactly ONE spelling and
    // the BINARY distinct count is exactly the key count.
    let par = text_db();
    let ser = {
        let mut s = text_db();
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let sql = "SELECT COUNT(DISTINCT name) FROM t";
    let a = rows_of(&par, sql);
    let b = rows_of(&ser, sql);
    assert_eq!(a, b, "TEXT COUNT(DISTINCT) must match serial");
    match &a[0][0] {
        Value::Integer(i) => assert_eq!(
            *i, 1000,
            "distinct BINARY names (one spelling per key by construction)"
        ),
        other => panic!("INTEGER expected, got {other:?}"),
    }
    // MIN/MAX(DISTINCT text) are the BINARY string extremes (a 'USER-'
    // spelling precedes every 'user-' one byte-wise).
    let sql2 = "SELECT MIN(DISTINCT name), MAX(DISTINCT name) FROM t";
    let a2 = rows_of(&par, sql2);
    let b2 = rows_of(&ser, sql2);
    assert_eq!(a2, b2, "TEXT MIN/MAX(DISTINCT) must match serial");
    // A dedicated table where every key DOES carry all 5 spellings
    // (spelling by row-block, keys by rowid mod): 2000 keys x 5
    // spellings = 10000 distinct BINARY strings.
    let build = || {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, v INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        {
            let mut i = 0i64;
            while i < 200_000 {
                let hi = (i + 1000).min(200_000);
                let mut sql = String::from("INSERT INTO t (id, name, v) VALUES ");
                for j in i..hi {
                    if j > i {
                        sql.push(',');
                    }
                    let name = match (j / 2000) % 5 {
                        0 => format!("'user-{:05}'", j % 2000),
                        1 => format!("'USER-{:05}'", j % 2000),
                        2 => format!("'User-{:05}'", j % 2000),
                        3 => format!("'uSeR-{:05}'", j % 2000),
                        _ => format!("'UsEr-{:05}'", j % 2000),
                    };
                    sql.push_str(&format!("({}, {}, {})", j, name, -j));
                }
                db.execute(&sql, []).unwrap();
                i = hi;
            }
        }
        db.execute("COMMIT", []).unwrap();
        db
    };
    let par5 = build();
    let ser5 = {
        let mut s = build();
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let sql3 = "SELECT COUNT(DISTINCT name) FROM t";
    let a3 = rows_of(&par5, sql3);
    let b3 = rows_of(&ser5, sql3);
    assert_eq!(a3, b3, "5-spelling TEXT COUNT(DISTINCT) must match serial");
    match &a3[0][0] {
        Value::Integer(i) => assert_eq!(*i, 10_000, "2000 keys x 5 spellings"),
        other => panic!("INTEGER expected, got {other:?}"),
    }
}

#[test]
fn parallel_distinct_null_and_sparse_semantics() {
    // Degenerate distinct sets over big tables (above threshold):
    // all-NULL column and one-non-NULL column.
    let build = |non_null: Option<i64>| {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, cat INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        {
            let mut i = 0i64;
            while i < 200_000 {
                let hi = (i + 1000).min(200_000);
                let mut sql = String::from("INSERT INTO t (id, v, cat) VALUES ");
                for j in i..hi {
                    if j > i {
                        sql.push(',');
                    }
                    let v = if Some(j) == non_null {
                        "42".to_string()
                    } else {
                        "NULL".to_string()
                    };
                    sql.push_str(&format!("({}, {}, {})", j, v, j % 10));
                }
                db.execute(&sql, []).unwrap();
                i = hi;
            }
        }
        db.execute("COMMIT", []).unwrap();
        db
    };
    // One non-NULL value: every DISTINCT aggregate sees exactly {42}.
    let sql = "SELECT COUNT(DISTINCT v), SUM(DISTINCT v), MIN(DISTINCT v), MAX(DISTINCT v), AVG(DISTINCT v) FROM t";
    let par = build(Some(150_000));
    let ser = {
        let mut s = build(Some(150_000));
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let a = rows_of(&par, sql);
    let b = rows_of(&ser, sql);
    assert_eq!(a, b, "sparse DISTINCT set must match serial");
    assert_eq!(
        a[0],
        vec![
            Value::Integer(1),
            Value::Integer(42),
            Value::Integer(42),
            Value::Integer(42),
            Value::Real(42.0)
        ],
        "one-value DISTINCT ground truth"
    );
    // All-NULL column: the distinct set is empty — COUNT 0, SUM/MIN/MAX
    // NULL, AVG NULL (SQLite's empty-aggregate semantics).
    let par = build(None);
    let ser = {
        let mut s = build(None);
        s.execute("PRAGMA parallel_scan=0", []).unwrap();
        s
    };
    let a = rows_of(&par, sql);
    let b = rows_of(&ser, sql);
    assert_eq!(a, b, "all-NULL DISTINCT must match serial");
    assert_eq!(
        a[0],
        vec![
            Value::Integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ],
        "empty DISTINCT set ground truth"
    );
    // A filter that passes NO rows: same empty-set semantics through the
    // compiled-filter arm.
    let sql_filtered =
        "SELECT COUNT(DISTINCT v), SUM(DISTINCT v), MIN(DISTINCT v) FROM t WHERE v > 1000000000";
    let a = rows_of(&par, sql_filtered);
    let b = rows_of(&ser, sql_filtered);
    assert_eq!(a, b, "filtered-empty DISTINCT must match serial");
    assert_eq!(
        a[0],
        vec![Value::Integer(0), Value::Null, Value::Null],
        "no-row DISTINCT ground truth"
    );
}
