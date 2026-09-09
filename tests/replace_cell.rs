//! Correctness of the in-leaf shape-changing UPDATE path
//! (`Btree::replace_table_cell`): payloads that grow/shrink must update
//! the row exactly like the delete+insert path — value fidelity, rowid
//! stability, index consistency, persistence, integrity, and heavy
//! churn (dead-space accumulation up to split compaction).

use rustqlite::{Database, Value};

fn fresh() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    db
}

fn seed(db: &mut Database, n: i64) {
    db.execute("BEGIN", []).unwrap();
    for i in 1..=n {
        db.execute(
            "INSERT INTO t (name, val, score) VALUES (?, ?, ?)",
            [
                Value::Text(format!("user{}", i).into()),
                Value::Integer(i * 2),
                Value::Real(i as f64 * 1.5),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

#[test]
fn replace_grow_shrink_roundtrip() {
    let mut db = fresh();
    seed(&mut db, 500);
    // Grow every payload (int -> X.5 real: +6..7 bytes), then shrink back.
    for i in 1..=500i64 {
        db.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            [Value::Real(i as f64 * 2.5), Value::Integer(i)],
        )
        .unwrap();
    }
    for i in 1..=500i64 {
        let rows = db
            .query(
                "SELECT name, val, score FROM t WHERE id = ?",
                [Value::Integer(i)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Integer(i * 2));
        assert_eq!(rows[0][2], Value::Real(i as f64 * 2.5));
    }
    // Shrink back to the original values.
    for i in 1..=500i64 {
        db.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            [Value::Real(i as f64 * 1.5), Value::Integer(i)],
        )
        .unwrap();
    }
    let rows = db.query("SELECT COUNT(*), SUM(score) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(500));
    let expect: f64 = (1..=500).map(|i| i as f64 * 1.5).sum();
    assert_eq!(rows[0][1], Value::Real(expect));
}

#[test]
fn replace_text_size_flips() {
    let mut db = fresh();
    seed(&mut db, 200);
    // Alternate tiny/huge names to force a wide size distribution.
    for round in 0..4i64 {
        for i in 1..=200i64 {
            let name = if (i + round) % 2 == 0 {
                "x".repeat(3)
            } else {
                format!("long-name-{}-{}", i, "y".repeat((i % 40 + 5) as usize))
            };
            db.execute(
                "UPDATE t SET name = ? WHERE id = ?",
                [Value::Text(name.into()), Value::Integer(i)],
            )
            .unwrap();
        }
    }
    // Every row must be readable and correct.
    let rows = db
        .query("SELECT id, LENGTH(name) FROM t ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 200);
    for (idx, r) in rows.iter().enumerate() {
        let i = idx as i64 + 1;
        assert_eq!(r[0], Value::Integer(i));
        let expect_len = if (i + 3) % 2 == 0 {
            3
        } else {
            format!("long-name-{}-{}", i, "y".repeat((i % 40 + 5) as usize)).len()
        };
        assert_eq!(r[1], Value::Integer(expect_len as i64));
    }
}

#[test]
fn replace_indexed_column_unchanged() {
    // Index maintenance must stay exact when the indexed column is NOT
    // touched (the replace path's eligibility) — and when it IS touched,
    // entries must move.
    let mut db = fresh();
    seed(&mut db, 300);
    for i in 1..=300i64 {
        db.execute(
            "UPDATE t SET score = ? WHERE id = ?",
            [Value::Real(i as f64 * 2.5), Value::Integer(i)],
        )
        .unwrap();
    }
    // idx_val untouched: every lookup by val must find the exact row.
    for i in 1..=300i64 {
        let rows = db
            .query("SELECT id FROM t WHERE val = ?", [Value::Integer(i * 2)])
            .unwrap();
        assert_eq!(rows.len(), 1, "val={} lost from index", i * 2);
        assert_eq!(rows[0][0], Value::Integer(i));
    }
    // Now touch the indexed column too (grows payload via name).
    for i in 1..=300i64 {
        db.execute(
            "UPDATE t SET val = ?, name = ? WHERE id = ?",
            [
                Value::Integer(i * 3),
                Value::Text(format!("name-{}", i * 111).into()),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    for i in 1..=300i64 {
        let rows = db
            .query(
                "SELECT id, name FROM t WHERE val = ?",
                [Value::Integer(i * 3)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "val={} missing after reindex", i * 3);
        assert_eq!(rows[0][1], Value::Text(format!("name-{}", i * 111).into()));
    }
    let rows = db
        .query("SELECT COUNT(*) FROM t WHERE val > 0", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(300));
}

#[test]
fn replace_range_update_size_mix() {
    // Multi-row range UPDATE with mixed growth/shrink (collect path).
    let mut db = fresh();
    seed(&mut db, 2000);
    db.execute("UPDATE t SET score = score * 2.5 WHERE val > 100", [])
        .unwrap();
    let rows = db
        .query("SELECT COUNT(*), SUM(score) FROM t WHERE val > 100", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(1950));
    let expect: f64 = (51..=2000).map(|i| i as f64 * 1.5 * 2.5).sum();
    assert_eq!(rows[0][1], Value::Real(expect));
    // integrity_check must stay clean.
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0], Value::Text("ok".into()));
}

#[test]
fn replace_heavy_churn_persistence() {
    // Path used on disk: file close/reopen after heavy replace churn.
    let path = std::env::temp_dir().join("rq_replace_churn.db");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(path.to_str().unwrap()).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
    seed(&mut db, 1000);
    // Many rounds — dead bytes accumulate until splits compact; the
    // data must survive it all identically.
    for round in 0..6i64 {
        for i in 1..=1000i64 {
            db.execute(
                "UPDATE t SET score = ?, name = ? WHERE id = ?",
                [
                    Value::Real((i + round) as f64 * 2.5),
                    Value::Text(format!("u{}-{}", i, round % 3).into()),
                    Value::Integer(i),
                ],
            )
            .unwrap();
        }
    }
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0], Value::Text("ok".into()));
    let before = db
        .query("SELECT id, name, score FROM t ORDER BY id", [])
        .unwrap();
    drop(db);
    let mut db2 = Database::open(path.to_str().unwrap()).unwrap();
    let after = db2
        .query("SELECT id, name, score FROM t ORDER BY id", [])
        .unwrap();
    assert_eq!(before, after);
    // The reopened tree must still update fine (hints rebuilt).
    db2.execute("UPDATE t SET score = 1.5 WHERE id = 10", [])
        .unwrap();
    let rows = db2.query("SELECT score FROM t WHERE id = 10", []).unwrap();
    assert_eq!(rows[0][0], Value::Real(1.5));
    drop(db2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn replace_overflow_fallback() {
    // Payloads that spill to overflow chains must never take the
    // in-leaf replace — the fallback path owns chain bookkeeping.
    let mut db = fresh();
    db.execute(
        "INSERT INTO t (id, name, val, score) VALUES (1, ?, 10, 1.0)",
        [Value::Text("a".repeat(100).into())],
    )
    .unwrap();
    for round in 0..3 {
        let n = 5000 + round * 1000;
        db.execute(
            "UPDATE t SET name = ? WHERE id = 1",
            [Value::Text("b".repeat(n).into())],
        )
        .unwrap();
        let rows = db
            .query("SELECT LENGTH(name), val FROM t WHERE id = 1", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(n as i64));
        assert_eq!(rows[0][1], Value::Integer(10));
        // Shrink back below page size (exercises both fallbacks).
        db.execute(
            "UPDATE t SET name = ? WHERE id = 1",
            [Value::Text("small".into())],
        )
        .unwrap();
        let rows = db
            .query("SELECT LENGTH(name) FROM t WHERE id = 1", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(5));
    }
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0], Value::Text("ok".into()));
}

#[test]
fn replace_returning_and_triggers() {
    let mut db = fresh();
    seed(&mut db, 50);
    db.execute(
        "CREATE TRIGGER trg AFTER UPDATE ON t BEGIN UPDATE t SET val = val WHERE id = NEW.id; END",
        [],
    )
    .unwrap();
    let rows = db
        .query(
            "UPDATE t SET score = score * 2.5 WHERE id <= 10 RETURNING id, score",
            [],
        )
        .unwrap();
    assert_eq!(rows.len(), 10);
    for (idx, r) in rows.iter().enumerate() {
        let i = idx as i64 + 1;
        assert_eq!(r[0], Value::Integer(i));
        assert_eq!(r[1], Value::Real(i as f64 * 1.5 * 2.5));
    }
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0], Value::Text("ok".into()));
}

#[test]
fn replace_until_split_compaction() {
    // Hammer ONE row with growing payloads until the leaf runs out of
    // free space: the replace must fall back to delete+insert (possibly
    // splitting) and the tree must stay exact.
    let mut db = fresh();
    seed(&mut db, 40);
    for n in 1..=600usize {
        db.execute(
            "UPDATE t SET name = ? WHERE id = 20",
            [Value::Text("z".repeat(n).into())],
        )
        .unwrap();
    }
    let rows = db
        .query("SELECT LENGTH(name) FROM t WHERE id = 20", [])
        .unwrap();
    assert_eq!(rows[0][0], Value::Integer(600));
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(40));
    let ic = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(ic[0][0], Value::Text("ok".into()));
}
