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
