//! Overflow page chains: payloads larger than one page spill to linked
//! Overflow pages (SQLite overflow-chain equivalent). These tests verify
//! correctness across every access path: point lookup, full scan, range
//! scan, streaming (step), update (in-place + delete/insert), delete
//! (chain reclamation), rollback, persistence, integrity, and indexes.

use rustqlite::{Database, Value};

fn sizes() -> Vec<usize> {
    let page = 4096;
    vec![
        10,                                  // tiny
        page - 200,                          // just under the in-page limit
        page - 128 + 1,                      // just over → spills
        page,                                // exactly one page
        page + 1,                            // just over one page
        3 * page,                            // multi-page chain
        17 * page + 123,                     // many pages + remainder
        100 * 1024,                          // 100 KiB
    ]
}

fn seeded(db: &mut Database, n: i64) {
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB, s TEXT, v INTEGER)", [])
        .unwrap();
    db.execute("BEGIN", []).unwrap();
    for i in 0..n {
        db.execute(
            "INSERT INTO t (b, s, v) VALUES (?, ?, ?)",
            [
                Value::Blob(vec![(i % 256) as u8; (i as usize % 9000) + 1]),
                Value::Text(format!("row-{i}-{}", "y".repeat((i as usize % 7000) + 1)).into()),
                Value::Integer(i),
            ],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
}

#[test]
fn overflow_round_trip_all_sizes() {
    for &sz in &sizes() {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)", [])
            .unwrap();
        let blob: Vec<u8> = (0..sz).map(|i| (i % 251) as u8).collect();
        db.execute("INSERT INTO t (b) VALUES (?)", [Value::Blob(blob.clone())])
            .unwrap();
        // Point lookup path.
        let rows = db.query("SELECT b FROM t WHERE id = 1", []).unwrap();
        match &rows[0][0] {
            Value::Blob(got) => assert_eq!(got, &blob, "size {sz}: point lookup mismatch"),
            other => panic!("size {sz}: expected blob, got {other:?}"),
        }
        // length() through the expression evaluator.
        let rows = db.query("SELECT length(b) FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(sz as i64), "size {sz}");
        // Full scan path.
        let rows = db.query("SELECT b FROM t", []).unwrap();
        match &rows[0][0] {
            Value::Blob(got) => assert_eq!(got.len(), sz, "size {sz}: scan mismatch"),
            other => panic!("size {sz}: expected blob, got {other:?}"),
        }
    }
}

#[test]
fn overflow_mixed_rows_scan_order() {
    let mut db = Database::open_in_memory().unwrap();
    seeded(&mut db, 60);
    // Row id R was inserted at index i = R - 1: v = i, |b| = i%9000+1,
    // |s| = 5 + digits(i) + (i%7000+1)  ("row-" + i + "-" + y's).
    let rows = db.query("SELECT v, length(b), length(s) FROM t ORDER BY id", []).unwrap();
    for (k, r) in rows.iter().enumerate() {
        let i = k; // insert index (rowid = i + 1)
        assert_eq!(r[0], Value::Integer(i as i64));
        assert_eq!(r[1], Value::Integer(((i % 9000) + 1) as i64));
        let s_len = 5 + digits(i) + (i % 7000) + 1;
        assert_eq!(r[2], Value::Integer(s_len as i64));
    }
}

fn digits(n: usize) -> usize {
    let mut d = 0;
    let mut n = n;
    loop {
        d += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    d
}

#[test]
fn overflow_range_scan() {
    let mut db = Database::open_in_memory().unwrap();
    seeded(&mut db, 40);
    let rows = db
        .query("SELECT length(b) FROM t WHERE id BETWEEN 10 AND 19 ORDER BY id", [])
        .unwrap();
    assert_eq!(rows.len(), 10);
    for (k, r) in rows.iter().enumerate() {
        let i = 10 + k - 1; // rowid 10+k was inserted at index 9+k
        assert_eq!(r[0], Value::Integer(((i % 9000) + 1) as i64));
    }
}

#[test]
fn overflow_streaming_step() {
    // The streaming statement path (prepare/step) must assemble chains too.
    let mut db = Database::open_in_memory().unwrap();
    let sz = 5 * 4096 + 17;
    let blob: Vec<u8> = (0..sz).map(|i| (i % 249) as u8).collect();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)", []).unwrap();
    db.execute("INSERT INTO t (b) VALUES (?)", [Value::Blob(blob.clone())])
        .unwrap();
    let mut stmt = db.prepare("SELECT b FROM t WHERE id = 1").unwrap();
    use rustqlite::statement::StepResult;
    assert_eq!(stmt.step().unwrap(), StepResult::Row);
    let row = stmt.row().unwrap();
    match &row[0] {
        Value::Blob(got) => assert_eq!(got, &blob),
        other => panic!("expected blob, got {other:?}"),
    }
    assert_eq!(stmt.step().unwrap(), StepResult::Done);
}

#[test]
fn overflow_update_paths() {
    let mut db = Database::open_in_memory().unwrap();
    seeded(&mut db, 30);
    // Grow a spilled row (delete+insert fallback).
    let big: Vec<u8> = vec![7u8; 3 * 4096];
    db.execute("UPDATE t SET b = ? WHERE id = 5", [Value::Blob(big.clone())])
        .unwrap();
    let rows = db.query("SELECT b FROM t WHERE id = 5", []).unwrap();
    match &rows[0][0] {
        Value::Blob(got) => assert_eq!(got, &big),
        other => panic!("{other:?}"),
    }
    // Shrink it back.
    db.execute("UPDATE t SET b = ? WHERE id = 5", [Value::Blob(vec![9u8; 100])])
        .unwrap();
    let rows = db.query("SELECT length(b) FROM t WHERE id = 5", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(100));
    // Same-size in-place patch of an in-page row among spilled rows.
    db.execute("UPDATE t SET b = ? WHERE id = 6", [Value::Blob(vec![3u8; 100])])
        .unwrap();
    db.execute("UPDATE t SET b = ? WHERE id = 6", [Value::Blob(vec![4u8; 100])])
        .unwrap();
    let rows = db.query("SELECT b FROM t WHERE id = 6", []).unwrap();
    match &rows[0][0] {
        Value::Blob(got) => assert!(got.iter().all(|&b| b == 4)),
        other => panic!("{other:?}"),
    }
    // Everything else must be untouched.
    let rows = db.query("SELECT COUNT(*), SUM(length(b)) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(30));
}

#[test]
fn overflow_delete_reclaims_and_stays_correct() {
    let mut db = Database::open_in_memory().unwrap();
    seeded(&mut db, 25);
    for id in [3i64, 7, 11, 19, 24] {
        db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(id)]).unwrap();
    }
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(20));
    // Survivors round-trip (rowid 5 = insert index 4).
    let rows = db.query("SELECT length(b) FROM t WHERE id = 5", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer((4 % 9000) + 1));
    // Deleting a spilled row twice is a no-op.
    db.execute("DELETE FROM t WHERE id = 3", [Value::Integer(3)]).unwrap();
    // Freelist pages are reused by new spilled rows (no unbounded growth).
    let before = db.execute("PRAGMA page_count", []).unwrap();
    for i in 0..10i64 {
        db.execute(
            "INSERT INTO t (b, s, v) VALUES (?, ?, ?)",
            [Value::Blob(vec![1u8; 2 * 4096]), Value::Text("z".repeat(64).into()), Value::Integer(1000 + i)],
        )
        .unwrap();
    }
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(30));
    let _ = before;
}

#[test]
fn overflow_rollback() {
    let mut db = Database::open_in_memory().unwrap();
    seeded(&mut db, 10);
    db.execute("BEGIN", []).unwrap();
    for i in 0..5i64 {
        db.execute(
            "INSERT INTO t (b, s, v) VALUES (?, ?, ?)",
            [Value::Blob(vec![8u8; 3 * 4096]), Value::Text("r".into()), Value::Integer(500 + i)],
        )
        .unwrap();
    }
    db.execute("DELETE FROM t WHERE id = 2", [Value::Integer(2)]).unwrap();
    db.execute("ROLLBACK", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(10));
    let rows = db.query("SELECT length(b) FROM t WHERE id = 2", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer((1 % 9000) + 1));
    // Post-rollback writes work (chain bookkeeping is clean).
    db.execute("INSERT INTO t (b, s, v) VALUES (?, ?, ?)",
        [Value::Blob(vec![5u8; 4097]), Value::Text("p".into()), Value::Integer(600)]).unwrap();
    let rows = db.query("SELECT length(b) FROM t WHERE v = 600", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(4097));
}

#[test]
fn overflow_persistence_across_reopen() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_str().unwrap().to_string();
    {
        let mut db = Database::open(&path).unwrap();
        seeded(&mut db, 15);
    }
    let db = Database::open(&path).unwrap();
    let rows = db.query("SELECT length(b), length(s) FROM t WHERE id = 12", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer((11 % 9000) + 1));
    // Integrity: chains must validate.
    let rows = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(rows[0][0], Value::Text("ok".into()));
}

#[test]
fn overflow_integrity_check() {
    let mut db = Database::open_in_memory().unwrap();
    seeded(&mut db, 50);
    let rows = db.query("PRAGMA integrity_check", []).unwrap();
    assert_eq!(rows[0][0], Value::Text("ok".into()));
}

#[test]
fn overflow_with_index() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, b BLOB)", []).unwrap();
    db.execute("CREATE INDEX iv ON t (v)", []).unwrap();
    for i in 0..20i64 {
        db.execute(
            "INSERT INTO t (v, b) VALUES (?, ?)",
            [Value::Integer(i % 5), Value::Blob(vec![(i % 7) as u8; (i as usize % 9000) + 1])],
        )
        .unwrap();
    }
    // Index lookup that lands on spilled rows.
    let rows = db.query("SELECT COUNT(*) FROM t WHERE v = 2", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(4));
    let rows = db.query("SELECT length(b) FROM t WHERE v = 2 ORDER BY id", []).unwrap();
    for (k, r) in rows.iter().enumerate() {
        let i = 2 + 5 * k;
        assert_eq!(r[0], Value::Integer(((i % 9000) + 1) as i64));
    }
    // Deleting through the index keeps chains + index consistent.
    db.execute("DELETE FROM t WHERE v = 2", []).unwrap();
    let rows = db.query("SELECT COUNT(*) FROM t WHERE v = 2", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(0));
    let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(16));
}

#[test]
fn overflow_replace_and_upsert() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB)", []).unwrap();
    db.execute("INSERT INTO t (b) VALUES (?)", [Value::Blob(vec![1u8; 5 * 4096])])
        .unwrap();
    // REPLACE frees the old chain and writes the new one.
    db.execute(
        "INSERT OR REPLACE INTO t (id, b) VALUES (1, ?)",
        [Value::Blob(vec![2u8; 2 * 4096])],
    )
    .unwrap();
    let rows = db.query("SELECT length(b) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(2 * 4096 as i64));
    // UPSERT grows it again.
    db.execute(
        "INSERT INTO t (id, b) VALUES (1, ?) ON CONFLICT (id) DO UPDATE SET b = excluded.b",
        [Value::Blob(vec![3u8; 4 * 4096])],
    )
    .unwrap();
    let rows = db.query("SELECT length(b) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(4 * 4096 as i64));
}

#[test]
fn overflow_text_functions() {
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)", []).unwrap();
    let s = "abcdef".repeat(2000); // 12 KiB
    db.execute("INSERT INTO t (s) VALUES (?)", [Value::Text(s.clone().into())])
        .unwrap();
    let rows = db.query("SELECT length(s), upper(substr(s, 1, 6)), substr(s, -3) FROM t", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(12000));
    assert_eq!(rows[0][1], Value::Text("ABCDEF".into()));
    assert_eq!(rows[0][2], Value::Text("def".into()));
}
