//! R*Tree tests: rtree / rtree_i32 create/insert/query/drop, dimension
//! constraints, per-dimension min<=max enforcement, i32 range checks,
//! reopen persistence, rollback resync — differential against real
//! SQLite (rusqlite bundled) for query semantics and error messages.

use rustqlite::types::Value;
use rustqlite::Database;

fn lite() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

fn ids(db: &mut Database, sql: &str) -> Vec<i64> {
    db.query(sql, [])
        .unwrap()
        .iter()
        .map(|r| r[0].as_integer())
        .collect()
}

// ---------------------------------------------------------------------------
// Basic surface
// ---------------------------------------------------------------------------

#[test]
fn rtree_create_insert_query() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE VIRTUAL TABLE demo USING rtree(id, x1, x2, y1, y2)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO demo VALUES (1, 1.0, 3.0, 2.0, 4.0);
         INSERT INTO demo VALUES (2, 2.0, 5.0, 3.0, 6.0);
         INSERT INTO demo VALUES (3, 5.0, 7.0, 5.0, 8.0)",
        [],
    )
    .unwrap();
    // Oracle-pinned shapes.
    assert_eq!(
        ids(&mut db, "SELECT id FROM demo WHERE x1 >= 2.0 AND x2 <= 6.0"),
        vec![2]
    );
    assert_eq!(ids(&mut db, "SELECT id FROM demo WHERE id = 2"), vec![2]);
    assert_eq!(
        ids(&mut db, "SELECT id FROM demo WHERE x1 > 1.5"),
        vec![2, 3]
    );
    assert_eq!(ids(&mut db, "SELECT id FROM demo WHERE y2 < 5.5"), vec![1]);
    assert_eq!(ids(&mut db, "SELECT id FROM demo WHERE x1 = 1.0"), vec![1]);
    // Full scan + column reads.
    let rows = db
        .query("SELECT id, x1, x2, y1, y2 FROM demo ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][1].as_real(), 1.0);
    assert_eq!(rows[2][4].as_real(), 8.0);
}

#[test]
fn rtree_differential_random_boxes() {
    let mut ours = Database::open_in_memory().unwrap();
    let oracle = lite();
    ours.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2, y1, y2)", [])
        .unwrap();
    oracle
        .execute_batch("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2, y1, y2);")
        .unwrap();
    // Random boxes with an LCG.
    let mut lcg: u64 = 0xfeed_beef_cafe_f00d;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (lcg >> 33) as f64 / 64.0
    };
    for i in 1..=80i64 {
        let x1 = (next() * 100.0).round() / 10.0;
        let x2 = x1 + (next() * 50.0).round() / 10.0 + 0.5;
        let y1 = (next() * 100.0).round() / 10.0;
        let y2 = y1 + (next() * 50.0).round() / 10.0 + 0.5;
        ours.execute(
            "INSERT INTO t VALUES (?, ?, ?, ?, ?)",
            [
                Value::Integer(i),
                Value::Real(x1),
                Value::Real(x2),
                Value::Real(y1),
                Value::Real(y2),
            ],
        )
        .unwrap();
        oracle
            .execute(
                "INSERT INTO t VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![i, x1, x2, y1, y2],
            )
            .unwrap();
    }
    let queries = [
        "SELECT id FROM t WHERE x1 >= 5.0 AND x2 <= 40.0",
        "SELECT id FROM t WHERE x1 > 3.3",
        "SELECT id FROM t WHERE y1 < 2.0 AND y2 > 4.0",
        "SELECT id FROM t WHERE x2 = 20.5",
        "SELECT id FROM t WHERE id = 42",
        "SELECT id FROM t WHERE y1 >= 1.0 AND y1 <= 9.0 AND x2 < 30.0",
        "SELECT count(*) FROM t",
    ];
    for q in queries {
        // Result order without ORDER BY is tree-traversal defined —
        // compare as sorted sets.
        let mut ours_ids = ids(&mut ours, q);
        ours_ids.sort_unstable();
        let mut stmt = oracle.prepare(q).unwrap();
        let mut oracle_ids: Vec<i64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        oracle_ids.sort_unstable();
        assert_eq!(ours_ids, oracle_ids, "query: {q}");
    }
}

// ---------------------------------------------------------------------------
// Constraints and errors (oracle-pinned messages)
// ---------------------------------------------------------------------------

#[test]
fn rtree_min_max_constraint() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2, y1, y2)", [])
        .unwrap();
    // min > max → the oracle's exact error.
    let err = db
        .execute("INSERT INTO t VALUES (9, 5.0, 1.0, 1.0, 2.0)", [])
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("rtree constraint failed: t.(x1<=x2)"),
        "got: {err}"
    );
    // Per-dimension: y is fine, x violates.
    let err = db
        .execute("INSERT INTO t VALUES (9, 1.0, 2.0, 5.0, 1.0)", [])
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("rtree constraint failed: t.(y1<=y2)"),
        "got: {err}"
    );
}

#[test]
fn rtree_i32_mode() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING rtree_i32(id, x1, x2)", [])
        .unwrap();
    // Reals truncate (oracle: accepted).
    db.execute("INSERT INTO t VALUES (1, 1.5, 2.5)", [])
        .unwrap();
    let rows = db.query("SELECT x1, x2 FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
    assert_eq!(rows[0][1].as_integer(), 2);
    // Out-of-i32-range → constraint error (oracle message).
    let err = db
        .execute("INSERT INTO t VALUES (2, 1, 2147483648)", [])
        .unwrap_err();
    assert!(
        err.to_string().contains("rtree constraint failed"),
        "got: {err}"
    );
}

#[test]
fn rtree_dimension_limits() {
    let mut db = Database::open_in_memory().unwrap();
    // 1..5 dims OK.
    for n in [1, 5] {
        let cols: Vec<String> = (0..n).map(|i| format!("x{i}a, x{i}b")).collect();
        db.execute(
            &format!(
                "CREATE VIRTUAL TABLE d{n} USING rtree(id, {})",
                cols.join(", ")
            ),
            [],
        )
        .unwrap();
    }
    // 6 dims → the oracle's message.
    let cols: Vec<String> = (0..6).map(|i| format!("x{i}a, x{i}b")).collect();
    let err = db
        .execute(
            &format!(
                "CREATE VIRTUAL TABLE d6 USING rtree(id, {})",
                cols.join(", ")
            ),
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("Too many columns for an rtree table"),
        "got: {err}"
    );
    // Odd column count → error.
    let err = db
        .execute("CREATE VIRTUAL TABLE odd USING rtree(id, x1, x2, y1)", [])
        .unwrap_err();
    assert!(err.to_string().contains("rtree"), "got: {err}");
}

#[test]
fn rtree_match_rejected() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2)", [])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 1.0, 2.0)", [])
        .unwrap();
    // The oracle rejects MATCH on rtree tables (an error, not a match).
    let r = db.query("SELECT id FROM t WHERE id MATCH 2", []);
    assert!(r.is_err(), "MATCH must be rejected: {:?}", r);
}

// ---------------------------------------------------------------------------
// DML + persistence
// ---------------------------------------------------------------------------

#[test]
fn rtree_update_delete() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2, y1, y2)", [])
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES (1, 1.0, 3.0, 2.0, 4.0);
         INSERT INTO t VALUES (2, 2.0, 5.0, 3.0, 6.0)",
        [],
    )
    .unwrap();
    // A valid partial UPDATE: only x1 moves (stays <= x2=3).
    db.execute("UPDATE t SET x1 = 2.5 WHERE id = 1", [])
        .unwrap();
    assert_eq!(ids(&mut db, "SELECT id FROM t WHERE x1 >= 2.0"), vec![1, 2]);
    // An UPDATE violating min<=max fails without partial effects.
    let err = db
        .execute("UPDATE t SET x2 = 0.5 WHERE id = 1", [])
        .unwrap_err();
    assert!(err.to_string().contains("rtree constraint failed"));
    assert_eq!(ids(&mut db, "SELECT id FROM t WHERE x1 >= 2.0"), vec![1, 2]);
    db.execute("DELETE FROM t WHERE id = 2", []).unwrap();
    let rows = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0].as_integer(), 1);
}

#[test]
fn rtree_reopen_and_rollback() {
    let dir = std::env::temp_dir().join(format!(
        "rt_reopen_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2, y1, y2)", [])
            .unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 1.0, 3.0, 2.0, 4.0);
             INSERT INTO t VALUES (2, 2.0, 5.0, 3.0, 6.0)",
            [],
        )
        .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE x1 >= 1.0 AND x2 <= 4.0"),
        vec![1],
        "R-tree rebuilt from the shadow on reopen"
    );
    assert_eq!(ids(&mut db, "SELECT id FROM t"), vec![1, 2]);
    // Rollback resync.
    db.execute("BEGIN", []).unwrap();
    db.execute("INSERT INTO t VALUES (3, 10.0, 12.0, 0.0, 1.0)", [])
        .unwrap();
    db.execute("ROLLBACK", []).unwrap();
    assert_eq!(ids(&mut db, "SELECT id FROM t"), vec![1, 2]);
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE x1 > 9.0"),
        Vec::<i64>::new()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rtree_drop_and_module_list() {
    let mut db = Database::open_in_memory().unwrap();
    let rows = db.query("PRAGMA module_list", []).unwrap();
    let names: Vec<String> = rows.iter().map(|r| r[0].as_text()).collect();
    assert!(names.iter().any(|n| n == "rtree"), "{names:?}");
    assert!(names.iter().any(|n| n == "rtree_i32"), "{names:?}");
    db.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2)", [])
        .unwrap();
    db.execute("DROP TABLE t", []).unwrap();
    let rows = db
        .query(
            "SELECT count(*) FROM sqlite_schema WHERE name = 't_content'",
            [],
        )
        .unwrap();
    assert_eq!(rows[0][0].as_integer(), 0);
}

#[test]
fn rtree_scale_tree_growth() {
    // Enough rows to force tree splits (32-entry nodes): correctness is
    // what matters — every id must remain queryable.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE VIRTUAL TABLE t USING rtree(id, x1, x2)", [])
        .unwrap();
    for i in 1..=500i64 {
        let lo = i as f64;
        let hi = lo + 10.0;
        db.execute(
            "INSERT INTO t VALUES (?, ?, ?)",
            [Value::Integer(i), Value::Real(lo), Value::Real(hi)],
        )
        .unwrap();
    }
    assert_eq!(ids(&mut db, "SELECT count(*) FROM t").len(), 1);
    let all = ids(&mut db, "SELECT id FROM t");
    assert_eq!(all.len(), 500);
    // Spot range queries across split boundaries.
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE x1 >= 100.0 AND x2 <= 115.0"
        ),
        vec![100, 101, 102, 103, 104, 105]
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE x1 > 495.0"),
        vec![496, 497, 498, 499, 500]
    );
    // Delete a middle band, re-check.
    db.execute("DELETE FROM t WHERE x1 >= 200.0 AND x1 < 300.0", [])
        .unwrap();
    assert_eq!(
        ids(
            &mut db,
            "SELECT count(*) FROM t WHERE x1 >= 200.0 AND x1 < 300.0"
        )
        .len(),
        1
    );
    let rest = ids(&mut db, "SELECT id FROM t WHERE x1 < 200.0");
    assert_eq!(rest.len(), 199);
}
