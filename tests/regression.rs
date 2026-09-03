//! Regression testing — modeled on §5 of https://www.sqlite.org/testing.html
//!
//! "Whenever a bug is reported against SQLite, that bug is not considered
//! fixed until new test cases that would exhibit the bug have been added
//! to either the TCL or TH3 test suites. Over the years, this has resulted
//! in thousands and thousands of new tests. These regression tests ensure
//! that bugs that have been fixed in the past are not reintroduced into
//! future versions."
//!
//! Every bug ever fixed in this repository gets a permanent, named test
//! here (the api.rs unit tests hold the storage-layer ones; this file
//! holds end-to-end ones). Each test documents the defect it guards
//! against, the commit that fixed it, and would fail if the fix regressed.
//!
//! Run with: cargo test --test regression

use rustqlite::{Database, Value};

// ===========================================================================
// B+tree splits moving table/index roots (commits 40bb296, 7e56b7f):
// the catalog's Arc<Table> lags the split; root overrides and schema-row
// persistence must keep every entry reachable.
// ===========================================================================

#[test]
fn regression_index_roots_survive_splits_and_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("reg-roots.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();

    // Enough rows to split both the table B+tree AND the index B+tree
    // several times (root page moves with each level-0 split).
    for i in 1..=5_000i64 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i * 13)])
            .unwrap();
    }
    // Every indexed lookup must find its row (stale roots made entries
    // past the first split silently unreachable).
    for i in 1..=5_000i64 {
        let rows = db
            .query("SELECT id FROM t WHERE val = ?", [Value::Integer(i * 13)])
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "val={} not found via index (stale root?)",
            i * 13
        );
    }
    // Close and reopen: roots must have been persisted to the schema rows.
    drop(db);
    let mut db = Database::open(&path).unwrap();
    for i in 1..=5_000i64 {
        let rows = db
            .query("SELECT id FROM t WHERE val = ?", [Value::Integer(i * 13)])
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "val={} missing after reopen (root not persisted?)",
            i * 13
        );
    }
    // And the index still updates after reopen.
    db.execute(
        "INSERT INTO t (val) VALUES (?)",
        [Value::Integer(5_001 * 13)],
    )
    .unwrap();
    let rows = db
        .query(
            "SELECT id FROM t WHERE val = ?",
            [Value::Integer(5_001 * 13)],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// ===========================================================================
// CREATE INDEX backfill (commit 7e56b7f): an index created AFTER data was
// inserted must contain all pre-existing rows.
// ===========================================================================

#[test]
fn regression_create_index_backfills_existing_rows() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", [])
        .unwrap();
    for i in 1..=2_000i64 {
        db.execute(
            "INSERT INTO t (name) VALUES (?)",
            [Value::Text(format!("name{:04}", i).into())],
        )
        .unwrap();
    }
    // Index created after the fact: every row must be findable through it.
    db.execute("CREATE INDEX idx_name ON t(name)", []).unwrap();
    for i in 1..=2_000i64 {
        let rows = db
            .query(
                "SELECT id FROM t WHERE name = ?",
                [Value::Text(format!("name{:04}", i).into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "backfilled index lost name{:04}", i);
    }
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE name IS NOT NULL", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(2_000))
    );
}

// ===========================================================================
// Quick-split byte accounting (commit ad1af5c): the fast split path
// undercounted the right page when the median landed on the insert
// position, corrupting the page. Insert patterns that hit that exact
// boundary must round-trip cleanly.
// ===========================================================================

#[test]
fn regression_quick_split_boundary_exact_bytes() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE q (id INTEGER PRIMARY KEY, pad TEXT)", [])
        .unwrap();
    // Variable-width rows: pads of every length 0..64 drive the cell-size
    // distribution through the quick-split median boundary repeatedly.
    for round in 0..8 {
        for len in 0..64usize {
            let id = round * 1000 + len as i64 + 1;
            db.execute(
                "INSERT INTO q (id, pad) VALUES (?, ?)",
                [Value::Integer(id), Value::Text("p".repeat(len).into())],
            )
            .unwrap();
        }
    }
    let rows = db.query("SELECT COUNT(*) FROM q", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(512))
    );
    // Every row must round-trip with its exact payload (page corruption
    // manifests as wrong payloads or structure errors).
    for round in 0..8 {
        for len in 0..64usize {
            let id = round * 1000 + len as i64 + 1;
            let rows = db
                .query(
                    "SELECT pad, length(pad) FROM q WHERE id = ?",
                    [Value::Integer(id)],
                )
                .unwrap();
            assert_eq!(rows.len(), 1, "row id={} lost after split", id);
            assert_eq!(
                rows[0][1],
                Value::Integer(len as i64),
                "row id={} pad corrupted",
                id
            );
            if len > 0 {
                assert_eq!(rows[0][0], Value::Text("p".repeat(len).into()));
            }
        }
    }
}

// ===========================================================================
// max-rowid invalidation (commit c951111): after DELETEing the max row,
// new autoincrement rowids must not resurrect or collide.
// ===========================================================================

#[test]
fn regression_max_rowid_invalidation_after_delete() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE m (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 1..=100i64 {
        db.execute(
            "INSERT INTO m (v) VALUES (?)",
            [Value::Text(format!("v{}", i).into())],
        )
        .unwrap();
    }
    // Delete the top 10 rows.
    db.execute("DELETE FROM m WHERE id > 90", []).unwrap();
    // New inserts get fresh rowids (max+1 semantics: 91.., never a reused
    // id that still exists, never a collision).
    for i in 0..20i64 {
        db.execute(
            "INSERT INTO m (v) VALUES (?)",
            [Value::Text(format!("w{}", i).into())],
        )
        .unwrap();
    }
    let rows = db.query("SELECT COUNT(*) FROM m", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(110))
    );
    // No duplicate rowids.
    let rows = db.query("SELECT COUNT(DISTINCT id) FROM m", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(110))
    );
    // Lookups by every id must return exactly one row.
    for id in 1..=110i64 {
        let rows = db
            .query("SELECT v FROM m WHERE id = ?", [Value::Integer(id)])
            .unwrap();
        assert_eq!(rows.len(), 1, "id={} returned {} rows", id, rows.len());
    }
}

// ===========================================================================
// Projection permutation codec (commit 83db9ba): selecting columns in an
// order different from storage order via the rowid-range fast path used to
// scramble values between columns.
// ===========================================================================

#[test]
fn regression_projection_permutation_via_rowid_range() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, a TEXT, b INTEGER, c REAL)",
        [],
    )
    .unwrap();
    for i in 1..=500i64 {
        db.execute(
            "INSERT INTO p (a, b, c) VALUES (?, ?, ?)",
            [
                Value::Text(format!("a{}", i).into()),
                Value::Integer(i * 3),
                Value::Real(i as f64 + 0.5),
            ],
        )
        .unwrap();
    }
    // All 6 permutations of (a, b, c) via the range path must agree.
    for perm in [
        "a, b, c", "a, c, b", "b, a, c", "b, c, a", "c, a, b", "c, b, a",
    ] {
        let rows = db
            .query(
                &format!("SELECT {} FROM p WHERE id BETWEEN 100 AND 200", perm),
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 101, "perm {} wrong row count", perm);
        for (k, row) in rows.iter().enumerate() {
            let id = 100 + k as i64;
            let va = Value::Text(format!("a{}", id).into());
            let vb = Value::Integer(id * 3);
            let vc = Value::Real(id as f64 + 0.5);
            let expected: Vec<Value> = perm
                .split(", ")
                .map(|c| match c {
                    "a" => va.clone(),
                    "b" => vb.clone(),
                    _ => vc.clone(),
                })
                .collect();
            assert_eq!(row, &expected, "perm {} row {} scrambled", perm, id);
        }
    }
}

// ===========================================================================
// UPDATE/DELETE WHERE routing (commit 11f259d): predicates on UPDATE and
// DELETE must go through the same scan machinery as SELECT — including
// non-sargable predicates that force full scans with residual filters.
// ===========================================================================

#[test]
fn regression_update_delete_where_routing() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, v INTEGER, s TEXT)",
        [],
    )
    .unwrap();
    for i in 1..=300i64 {
        db.execute(
            "INSERT INTO u (v, s) VALUES (?, ?)",
            [Value::Integer(i), Value::Text(format!("s{}", i % 7).into())],
        )
        .unwrap();
    }
    // Non-sargable UPDATE predicate (expression on the column).
    db.execute("UPDATE u SET v = -v WHERE v % 3 = 0", [])
        .unwrap();
    let rows = db.query("SELECT COUNT(*) FROM u WHERE v < 0", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(100))
    );

    // UPDATE with a join-shaped predicate.
    db.execute(
        "UPDATE u SET s = 'gone' WHERE v IN (SELECT v FROM u WHERE v = -6)",
        [],
    )
    .unwrap();
    let rows = db
        .query("SELECT COUNT(*) FROM u WHERE s = 'gone'", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(1))
    );

    // Non-sargable DELETE predicate.
    db.execute("DELETE FROM u WHERE v % 5 = 0 AND v < 0", [])
        .unwrap();
    let rows = db.query("SELECT COUNT(*) FROM u", []).unwrap();
    // 300 - 100 updated (v in {-3,-6,...,-300}) + 20 of those deleted (v%5==0)
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(280))
    );

    // IS NULL routing on UPDATE.
    db.execute("UPDATE u SET s = NULL WHERE id < 10", [])
        .unwrap();
    let rows = db
        .query("SELECT COUNT(*) FROM u WHERE s IS NULL", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(9))
    );
}

// ===========================================================================
// NULL semantics batch (commit 3a8065a — "3 more NULL bugs" surfaced by the
// differential suite): NULL in expressions, aggregates, DISTINCT, ORDER BY.
// ===========================================================================

#[test]
fn regression_null_semantics_corners() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE n (a INTEGER, b INTEGER)", [])
        .unwrap();
    db.execute(
        "INSERT INTO n VALUES (1, 1), (NULL, 2), (3, NULL), (NULL, NULL), (2, 2)",
        [],
    )
    .unwrap();

    // NULL comparisons are never TRUE.
    let rows = db
        .query("SELECT COUNT(*) FROM n WHERE a = NULL", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(0))
    );
    let rows = db
        .query("SELECT COUNT(*) FROM n WHERE a <> NULL", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(0))
    );

    // COUNT(column) skips NULLs; COUNT(*) does not. The rows are
    // (1,1), (NULL,2), (3,NULL), (NULL,NULL), (2,2): non-NULL a = {1,3,2}
    // = 3, non-NULL b = {1,2,2} = 3 (verified against real SQLite).
    let rows = db
        .query("SELECT COUNT(*), COUNT(a), COUNT(b) FROM n", [])
        .unwrap();
    assert_eq!(
        rows[0],
        vec![Value::Integer(5), Value::Integer(3), Value::Integer(3)]
    );

    // SUM/AVG/MIN/MAX over all-NULL → NULL.
    db.execute("CREATE TABLE e (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO e VALUES (NULL), (NULL)", [])
        .unwrap();
    let rows = db
        .query("SELECT SUM(x), AVG(x), MIN(x), MAX(x), COUNT(x) FROM e", [])
        .unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Integer(0)
        ]
    );

    // DISTINCT treats NULLs as equal (one NULL row): a values are
    // 1, NULL, 3, NULL, 2 -> distinct {NULL, 1, 2, 3} = 4 rows.
    let rows = db.query("SELECT DISTINCT a FROM n ORDER BY a", []).unwrap();
    assert_eq!(rows.len(), 4);

    // GROUP BY groups NULLs together.
    let rows = db
        .query("SELECT a, COUNT(*) FROM n GROUP BY a ORDER BY a", [])
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0], Value::Null);
    assert_eq!(rows[0][1], Value::Integer(2));
}

// ===========================================================================
// Today's warning-hunt finds (commit "eliminate all cargo check warnings"):
// RenameColumn computed the live override-aware root but then shadowed it
// with the stale catalog root — after a mid-session B+tree split the
// renamed table's schema row could point at a dead root page.
// ===========================================================================

#[test]
fn regression_rename_column_after_root_split_uses_live_root() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("rename-root.db");
    let mut db = Database::open(&path).unwrap();
    db.execute(
        "CREATE TABLE rc (id INTEGER PRIMARY KEY, old_name TEXT, v INTEGER)",
        [],
    )
    .unwrap();
    // Insert enough rows to split the B+tree (the root page moves).
    for i in 1..=3_000i64 {
        db.execute(
            "INSERT INTO rc (old_name, v) VALUES (?, ?)",
            [Value::Text(format!("n{}", i).into()), Value::Integer(i)],
        )
        .unwrap();
    }
    // Rename the column — the schema row must record the LIVE root.
    db.execute("ALTER TABLE rc RENAME COLUMN old_name TO new_name", [])
        .unwrap();
    // Write more (forces another split past the rename)...
    for i in 3_001..=4_000i64 {
        db.execute(
            "INSERT INTO rc (new_name, v) VALUES (?, ?)",
            [Value::Text(format!("n{}", i).into()), Value::Integer(i)],
        )
        .unwrap();
    }
    db.flush().unwrap();
    // ...reopen (roots come from the persisted schema rows now)...
    drop(db);
    let db = Database::open(&path).unwrap();
    // ...and every row must still be reachable under the new column name.
    for i in 1..=4_000i64 {
        let rows = db
            .query(
                "SELECT v FROM rc WHERE new_name = ?",
                [Value::Text(format!("n{}", i).into())],
            )
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "row {} unreachable after rename+split+reopen (stale root in schema row?)",
            i
        );
    }
    let rows = db.query("SELECT COUNT(*) FROM rc", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(4_000))
    );
}

// ===========================================================================
// Statement-cache correctness: a cached plan must not leak state between
// executions (params, aggregates, or row sets bleeding across runs).
// ===========================================================================

#[test]
fn regression_statement_cache_does_not_leak_state() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, g TEXT, v INTEGER)",
        [],
    )
    .unwrap();
    for i in 1..=100i64 {
        db.execute(
            "INSERT INTO c (g, v) VALUES (?, ?)",
            [
                Value::Text(if i % 2 == 0 {
                    "even".into()
                } else {
                    "odd".into()
                }),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    // The SAME statement text executed with DIFFERENT parameters must give
    // independent results — cached-plans with bound-in param state was a
    // classic leak.
    for run in 0..5 {
        let rows = db
            .query(
                "SELECT COUNT(*), SUM(v) FROM c WHERE g = ? AND v > ?",
                [
                    Value::Text(if run % 2 == 0 {
                        "even".into()
                    } else {
                        "odd".into()
                    }),
                    Value::Integer(20 + run * 10),
                ],
            )
            .unwrap();
        let g = if run % 2 == 0 { "even" } else { "odd" };
        let threshold = 20 + run * 10;
        let expected_count = (1..=100)
            .filter(|i| (if run % 2 == 0 { i % 2 == 0 } else { i % 2 == 1 }) && *i > threshold)
            .count();
        let expected_sum: i64 = (1..=100)
            .filter(|i| (if run % 2 == 0 { i % 2 == 0 } else { i % 2 == 1 }) && *i > threshold)
            .sum();
        assert_eq!(
            rows[0],
            vec![
                Value::Integer(expected_count as i64),
                Value::Integer(expected_sum)
            ],
            "run {} (g={}, v>{}): cached-statement leak suspected",
            run,
            g,
            threshold
        );
    }
}

// ===========================================================================
// WAL round-trip: data committed pre-checkpoint must be recovered when the
// connection is dropped without an explicit checkpoint (commit 7f403c1).
// ===========================================================================

#[test]
fn regression_wal_recovery_without_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("wal-reg.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("CREATE TABLE w (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        for i in 1..=1_000i64 {
            db.execute(
                "INSERT INTO w (v) VALUES (?)",
                [Value::Text(format!("v{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
        // NO explicit checkpoint: drop the connection with frames in the WAL.
    }
    // Reopen: un-checkpointed committed frames must be recovered.
    let mut db = Database::open(&path).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM w", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(1_000)),
        "WAL frames lost on reopen (recovery did not run?)"
    );
    // And the recovered table accepts further writes.
    db.execute("INSERT INTO w (v) VALUES ('post-recovery')", [])
        .unwrap();
    let rows = db.query("SELECT COUNT(*) FROM w", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(1_001))
    );
}

// ===========================================================================
// Savepoint rollback (page-level undo): nested savepoints with interleaved
// DDL and bulk DML must restore byte-exact state.
// ===========================================================================

#[test]
fn regression_nested_savepoint_bulk_rollback() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY, v TEXT)", [])
        .unwrap();
    for i in 1..=500i64 {
        db.execute(
            "INSERT INTO s (v) VALUES (?)",
            [Value::Text(format!("orig{}", i).into())],
        )
        .unwrap();
    }
    db.execute("SAVEPOINT outer", []).unwrap();
    db.execute("DELETE FROM s WHERE id > 250", []).unwrap();
    db.execute("UPDATE s SET v = 'changed' WHERE id <= 250", [])
        .unwrap();
    db.execute("SAVEPOINT inner", []).unwrap();
    db.execute("CREATE TABLE extra (x INTEGER)", []).unwrap();
    db.execute("INSERT INTO extra VALUES (1), (2), (3)", [])
        .unwrap();
    // Rollback to inner: `extra` disappears, the outer changes stay.
    db.execute("ROLLBACK TO inner", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM s", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(250))
    );
    let rows = db
        .query("SELECT COUNT(*) FROM s WHERE v = 'changed'", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(250))
    );
    let err = db.query("SELECT COUNT(*) FROM extra", []);
    assert!(
        err.is_err(),
        "table created inside a rolled-back savepoint still exists"
    );
    // Rollback to outer: everything is back to the original 500 rows.
    db.execute("ROLLBACK TO outer", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM s", []).unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(500))
    );
    let rows = db
        .query("SELECT COUNT(*) FROM s WHERE v LIKE 'orig%'", [])
        .unwrap();
    assert_eq!(
        rows.first().and_then(|r| r.first()),
        Some(&Value::Integer(500))
    );
}

// ===========================================================================
// Aggregate rewrite keyed only by function name (sqlx-driver sprint,
// 2026-09-02): `SELECT SUM(qty), SUM(price) ...` rewrote BOTH calls to
// __agg_0, so every aggregate after the first silently reported the
// FIRST aggregate's value (any path — GROUP BY, no-GROUP BY, streaming).
// The rewrite must match on the argument expression as well.
// ===========================================================================

#[test]
fn regression_multiple_aggregates_different_args() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE li (oid INT, qty INT, price REAL)", [])
        .unwrap();
    db.execute(
        "INSERT INTO li (oid, qty, price) VALUES (1, 3, 10), (2, 2, 6)",
        [],
    )
    .unwrap();

    // No GROUP BY: two SUMs over different columns.
    let rows = db.query("SELECT SUM(qty), SUM(price) FROM li", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(5));
    assert_eq!(rows[0][1], Value::Real(16.0));

    // Same function, expression vs column.
    let rows = db
        .query("SELECT SUM(qty), SUM(qty * price) FROM li", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(5));
    assert_eq!(rows[0][1], Value::Real(42.0));

    // GROUP BY: per-group values must differ per aggregate.
    let rows = db
        .query(
            "SELECT oid, SUM(qty), SUM(price) FROM li GROUP BY oid ORDER BY oid",
            [],
        )
        .unwrap();
    assert_eq!(
        rows[0],
        vec![Value::Integer(1), Value::Integer(3), Value::Real(10.0)]
    );
    assert_eq!(
        rows[1],
        vec![Value::Integer(2), Value::Integer(2), Value::Real(6.0)]
    );

    // Swapped order (the loose matcher favored aggregates[0] regardless).
    let rows = db
        .query(
            "SELECT oid, SUM(price), SUM(qty) FROM li GROUP BY oid ORDER BY oid",
            [],
        )
        .unwrap();
    assert_eq!(
        rows[0],
        vec![Value::Integer(1), Value::Real(10.0), Value::Integer(3)]
    );
    assert_eq!(
        rows[1],
        vec![Value::Integer(2), Value::Real(6.0), Value::Integer(2)]
    );

    // Aggregates in ORDER BY / expressions.
    let rows = db
        .query("SELECT SUM(qty) * 2, SUM(price) + 1 FROM li", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[0][1], Value::Real(17.0));

    // AVG/ MIN/ MAX with different args.
    let rows = db
        .query("SELECT MIN(qty), MAX(price), AVG(price) FROM li", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(2));
    assert_eq!(rows[0][1], Value::Real(10.0));
    assert!((rows[0][2].as_real() - 8.0).abs() < 1e-9);

    // COUNT(*) alongside SUM stays independent.
    let rows = db
        .query("SELECT COUNT(*), SUM(qty), COUNT(qty) FROM li", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(2));
    assert_eq!(rows[0][1], Value::Integer(5));
    assert_eq!(rows[0][2], Value::Integer(2));

    // HAVING referencing a second aggregate with different args.
    let rows = db
        .query(
            "SELECT oid FROM li GROUP BY oid HAVING SUM(qty) > 2 AND SUM(price) > 8",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Integer(1));
}

// ===========================================================================
// Statement-path DML merge-back dropped root overrides (sqlx-driver
// sprint, 2026-09-02): a prepared-statement INSERT that triggered a B+tree
// split recorded the new root only in the reader context; the merge-back
// wrote back max-rowids but NOT root/index overrides, so the catalog kept
// pointing at the pre-split root. Every insert/read after the first split
// went through the stale root — 5000 inserts silently retained ~391 rows
// (one leaf page), in BOTH transaction and autocommit modes.
// ===========================================================================

#[test]
fn regression_statement_dml_survives_btree_splits() {
    use rustqlite::{Statement, StepResult};

    for tx in [false, true] {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b REAL, c TEXT)",
            [],
        )
        .unwrap();
        // An index makes index-root splits happen too.
        db.execute("CREATE INDEX idx_c ON t (c)", []).unwrap();

        if tx {
            db.execute("BEGIN", []).unwrap();
        }
        // Per-row prepare/step (exactly what the sqlx driver does).
        for i in 0..5000i64 {
            let mut stmt: Statement<'_> = db
                .prepare("INSERT INTO t (a, b, c) VALUES (?, ?, ?)")
                .unwrap();
            stmt.bind_all(&[
                Value::Integer(i),
                Value::Real(i as f64),
                Value::Text(format!("name-{i:05}").into()),
            ])
            .unwrap();
            while stmt.step().unwrap() == StepResult::Row {}
            stmt.finalize().unwrap();
        }
        if tx {
            db.execute("COMMIT", []).unwrap();
        }

        let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(
            rows[0][0],
            Value::Integer(5000),
            "row count after bulk insert (tx={tx})"
        );

        // Every row must be reachable by rowid (stale roots made rows past
        // the first split unreachable).
        for i in [1i64, 100, 391, 392, 1000, 2500, 4999, 5000] {
            let rows = db
                .query("SELECT a FROM t WHERE id = ?", [Value::Integer(i)])
                .unwrap();
            assert_eq!(rows.len(), 1, "row {i} reachable (tx={tx})");
            assert_eq!(rows[0][0], Value::Integer(i - 1));
        }

        // ...and through the index.
        let rows = db
            .query(
                "SELECT id FROM t WHERE c = ?",
                [Value::Text("name-04999".into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "index lookup for the last row (tx={tx})");
        assert_eq!(rows[0][0], Value::Integer(5000));

        // One prepared statement re-bound per row (the reset path).
        let mut db2 = Database::open_in_memory().unwrap();
        db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT)", [])
            .unwrap();
        {
            let mut stmt = db2.prepare("INSERT INTO t (a) VALUES (?)").unwrap();
            for i in 0..3000i64 {
                stmt.bind_all(&[Value::Integer(i)]).unwrap();
                while stmt.step().unwrap() == StepResult::Row {}
                stmt.reset();
            }
            stmt.finalize().unwrap();
        }
        let rows = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(3000), "reset path (tx={tx})");
    }
}

// ===========================================================================
// ROLLBACK after COMMITTED splits (2026-09-03): a plain ROLLBACK restored
// the pager by DROPPING the page cache + truncating to the BEGIN snapshot,
// then reset the root bookkeeping to the catalog's CREATE-time roots. Two
// defects: (a) committed data lived only in dirty cache pages (in-memory
// lazy write-back), so the cache drop destroyed rows that the BEGIN-time
// flush had not written; (b) even with the file intact, the root maps
// pointed at the CREATE-time root page, which after earlier committed
// splits was an ordinary leaf — reads saw only that leaf's rows. Fixed by
// journaling the transaction with an implicit `__begin__` savepoint
// (non-destructive undo) and restoring the BEGIN-time maps snapshot.
// ===========================================================================

#[test]
fn regression_rollback_after_committed_splits_keeps_all_rows() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    // First transaction: enough rows to split the root multiple times.
    db.execute("BEGIN", []).unwrap();
    for i in 1..=10_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let before = db
        .query("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM t", [])
        .unwrap();
    let row = before.first().cloned().unwrap();
    assert_eq!(row[0], Value::Integer(10_000));
    assert_eq!(row[1], Value::Integer(50_005_000));

    // Second transaction: mutate, then roll back.
    db.execute("BEGIN", []).unwrap();
    for i in 10_001..=20_000i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("DELETE FROM t WHERE id > 5000", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();

    let after = db
        .query("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM t", [])
        .unwrap();
    let row = after.first().cloned().unwrap();
    assert_eq!(row[0], Value::Integer(10_000), "row count lost by ROLLBACK");
    assert_eq!(
        row[1],
        Value::Integer(50_005_000),
        "SUM corrupted by ROLLBACK"
    );
    assert_eq!(row[2], Value::Integer(1));
    assert_eq!(row[3], Value::Integer(10_000));

    // Rolled-back rows must be invisible; committed rows must be findable.
    assert_eq!(
        db.query("SELECT val FROM t WHERE id = 20000", [])
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        db.query("SELECT val FROM t WHERE id = 10000", [])
            .unwrap()
            .len(),
        1
    );

    // The tree is still fully writable afterwards.
    db.execute("BEGIN", []).unwrap();
    for i in 20_001..=20_005i64 {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("name{i}").into()),
                Value::Integer(i),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(10_005));
}

#[test]
fn regression_rollback_tiny_txn_after_splits() {
    // Even a ONE-row transaction rolled back after committed splits used to
    // destroy the table (the maps reset, not the pager, was the trigger).
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 1..=1_000i64 {
        db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
            .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    for _ in 0..3 {
        db.execute("BEGIN", []).unwrap();
        db.execute("INSERT INTO t (val) VALUES (999999)", [])
            .unwrap();
        db.execute("ROLLBACK", []).unwrap();
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(n[0][0], Value::Integer(1_000));
    }
    // Update-in-place + rollback with indexes (undo must restore pages
    // shared by table and index trees).
    db.execute("CREATE INDEX ival ON t(val)", []).unwrap();
    db.execute("BEGIN", []).unwrap();
    db.execute("UPDATE t SET val = val + 1000000", []).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    let agg = db.query("SELECT COUNT(*), SUM(val) FROM t", []).unwrap();
    assert_eq!(agg[0][0], Value::Integer(1_000));
    assert_eq!(agg[0][1], Value::Integer(500_500));
    // Index lookups still resolve the original values.
    let hits = db
        .query("SELECT COUNT(*) FROM t WHERE val = 500", [])
        .unwrap();
    assert_eq!(hits[0][0], Value::Integer(1));
}
