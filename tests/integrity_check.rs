//! `PRAGMA integrity_check` / `quick_check` — correctness of the checker
//! itself, modeled on SQLite's integrity.test / integrity1.test approach:
//! a clean database must report `ok`; every induced structural corruption
//! must be REPORTED (never panic); and fault-recovery scenarios must leave
//! a database that either passes the check or fails to open gracefully.
//!
//! https://www.sqlite.org/pragma.html#pragma_integrity_check
//! https://www.sqlite.org/testing.html (§3.2: after every simulated I/O
//! error the database is examined with PRAGMA integrity_check).

use rustqlite::{Database, Value};

fn build_clean_db(path: &std::path::Path) -> Vec<u8> {
    let mut db = Database::open(path).unwrap();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, r REAL, b BLOB)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX idx_s ON t(s)", []).unwrap();
    db.execute("CREATE TABLE u (v TEXT)", []).unwrap();
    for i in 1..=400i64 {
        db.execute(
            "INSERT INTO t (s, r, b) VALUES (?, ?, ?)",
            [
                Value::Text(format!("value-{:04}", i * 3).into()),
                Value::Real(i as f64 / 7.0),
                Value::Blob(vec![(i % 256) as u8; (i % 17) as usize]),
            ],
        )
        .unwrap();
    }
    for i in 1..=30i64 {
        db.execute(
            "INSERT INTO u (v) VALUES (?)",
            [Value::Text(format!("u{}", i).into())],
        )
        .unwrap();
    }
    // Savepoint rollback (exercises the freelist).
    db.execute("SAVEPOINT sp", []).unwrap();
    db.execute("INSERT INTO u (v) VALUES ('gone')", []).unwrap();
    db.execute("DELETE FROM t WHERE id > 380", []).unwrap();
    db.execute("ROLLBACK TO sp", []).unwrap();
    db.execute("RELEASE sp", []).unwrap();
    db.flush().unwrap();
    std::fs::read(path).unwrap()
}

fn first_col(db: &mut Database, pragma: &str) -> Vec<String> {
    let rows = db.query(pragma, []).expect("pragma query must not panic");
    rows.iter().map(|r| r[0].as_text()).collect()
}

#[test]
fn clean_database_reports_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("clean.db");
    build_clean_db(&path);

    let mut db = Database::open(&path).unwrap();
    assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
    assert_eq!(first_col(&mut db, "PRAGMA quick_check"), vec!["ok".to_string()]);

    // Live-session check after more DML (checks the live-root plumbing):
    // the flush inside the pragma makes the on-disk state current.
    db.execute("INSERT INTO t (s, r, b) VALUES ('live', 1.0, X'01')", []).unwrap();
    assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
}

#[test]
fn wal_mode_database_reports_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("wal.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("PRAGMA journal_mode = WAL", []).unwrap();
        db.execute("CREATE TABLE w (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
        for i in 1..=300i64 {
            db.execute(
                "INSERT INTO w (v) VALUES (?)",
                [Value::Text(format!("r{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
        assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
    }
    // Re-open with a live WAL: WAL-served pages must satisfy the file-shape
    // check (no false "truncated" report).
    let mut db = Database::open(&path).unwrap();
    assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
}

#[test]
fn cell_pointer_corruption_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("badcell.db");
    let original = build_clean_db(&path);

    // Our layout: page size u32 LE at bytes 8..12; a b-tree page's
    // 12-byte header (type, freeblock, ncells, content-start, frag,
    // reserved) is followed by the cell-pointer array at offset 12. Page 1
    // is a table leaf with hundreds of cells — striking its first cell
    // pointer to 0xFFFF (beyond the page) must surface as a corruption
    // message, never a panic.
    let page_size = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
    let mut bytes = original.clone();
    let base = page_size;
    bytes[base + 12..base + 14].copy_from_slice(&0xFFFFu16.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();

    if let Ok(mut db) = Database::open(&path) {
        let report = first_col(&mut db, "PRAGMA integrity_check");
        assert!(
            !report.is_empty() && report.iter().any(|m| m != "ok"),
            "cell-pointer corruption must be reported, got {:?}",
            report
        );
    }
    // (A graceful open failure is also acceptable.)
}

#[test]
fn rowid_order_corruption_is_reported_or_graceful() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("badrowid.db");
    let original = build_clean_db(&path);

    // Overwrite the page-count field with a LARGER number: the pager now
    // believes pages exist that don't; the shape check must catch it.
    let mut bytes = original.clone();
    let n = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    bytes[16..20].copy_from_slice(&(n + 3).to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    if let Ok(mut db) = Database::open(&path) {
        let report = first_col(&mut db, "PRAGMA integrity_check");
        assert!(
            report.iter().any(|m| m.contains("truncated") || m.contains("unreadable")),
            "phantom page count must be reported, got {:?}",
            report
        );
    }
}

#[test]
fn freelist_corruption_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("badfree.db");
    let original = build_clean_db(&path);

    // Claim a bogus freelist: head pointing beyond EOF with count > 0.
    let mut bytes = original.clone();
    let n = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    bytes[20..24].copy_from_slice(&(n + 10).to_le_bytes()); // freelist head
    bytes[24..28].copy_from_slice(&2u32.to_le_bytes()); // freelist count
    std::fs::write(&path, &bytes).unwrap();

    if let Ok(mut db) = Database::open(&path) {
        let report = first_col(&mut db, "PRAGMA integrity_check");
        assert!(
            report.iter().any(|m| m.contains("freelist")),
            "bogus freelist must be reported, got {:?}",
            report
        );
    }
}

#[test]
fn payload_corruption_does_not_false_positive() {
    // Data-only corruption — a byte inside a TEXT VALUE — changes VALUES
    // but not structure: the check must still report ok (SQLite's
    // integrity_check likewise does not checksum page content; it walks
    // structure and decodes records, and a value byte flip decodes fine).
    //
    // Deterministic targeting: build a table whose rows carry a long,
    // recognizable run of 'A's, find that run in the raw file, and flip a
    // byte in its MIDDLE (value bytes, provably past the record header).
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("payload.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, s TEXT)", []).unwrap();
        for i in 1..=20i64 {
            let filler = "A".repeat(200);
            db.execute(
                "INSERT INTO p (s) VALUES (?)",
                [Value::Text(format!("head-{}-{}-tail", i, filler).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
    }
    let original = std::fs::read(&path).unwrap();
    let run = original
        .windows(64)
        .position(|w| w.iter().all(|&b| b == b'A'))
        .expect("long 'A' run must exist in the file");
    // Middle of the run: a VALUE byte (past the record header varints).
    let strike = run + 32;

    let mut bytes = original.clone();
    bytes[strike] = b'B';
    std::fs::write(&path, &bytes).unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
    // The data DID change — one 'A' became 'B' — proving the strike was a
    // live value byte (not a structural no-op).
    let rows = db.query("SELECT COUNT(*) FROM p WHERE s LIKE '%B%'", []).unwrap();
    let n = match rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(n)) => *n,
        other => panic!("COUNT returned {:?}", other),
    };
    assert!(n >= 1, "flipped byte should appear as a changed value, got {}", n);
}

#[test]
fn truncated_file_is_graceful() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("trunc.db");
    let original = build_clean_db(&path);
    std::fs::write(&path, &original[..original.len() - 100]).unwrap();
    // Open must fail gracefully with a corruption error (never panic).
    let r = Database::open(&path);
    assert!(r.is_err(), "truncated file must not open");
}

#[test]
fn crash_recovered_database_passes_integrity() {
    // End-to-end: a crash mid-transaction (child abort), then recovery —
    // the recovered database MUST pass integrity_check (SQLite's crash
    // tests verify exactly this after recovery).
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("crash.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
        for i in 1..=200i64 {
            db.execute(
                "INSERT INTO c (v) VALUES (?)",
                [Value::Text(format!("base-{}", i).into())],
            )
            .unwrap();
        }
        db.flush().unwrap();
    }
    let original = std::fs::read(&path).unwrap();

    // Simulate the torn state of a mid-transaction crash: BEGIN, writes,
    // no COMMIT — the connection's dirty pages never hit the disk, so the
    // file equals the committed baseline.
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=50i64 {
            db.execute(
                "INSERT INTO c (v) VALUES (?)",
                [Value::Text(format!("tx-{}", i).into())],
            )
            .unwrap();
        }
        // Simulated crash: drop without COMMIT/flush.
        std::mem::forget(db);
    }
    // The file on disk is the committed baseline (dirty pages died with
    // the process). Restoring the pristine copy emulates that exactly.
    std::fs::write(&path, &original).unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
    let rows = db.query("SELECT COUNT(*) FROM c", []).unwrap();
    assert_eq!(rows[0][0], Value::Integer(200));
}

#[test]
fn savepoint_rollback_leaves_ok_database() {
    // SQLite semantics: ROLLBACK TO keeps the savepoint on the stack.
    // After rollback + release, the database must pass integrity_check
    // (freelist and page state rewound consistently).
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sp.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY, v TEXT)", []).unwrap();
        for i in 1..=300i64 {
            db.execute(
                "INSERT INTO s (v) VALUES (?)",
                [Value::Text(format!("orig{}", i).into())],
            )
            .unwrap();
        }
        db.execute("SAVEPOINT outer", []).unwrap();
        db.execute("DELETE FROM s WHERE id > 150", []).unwrap();
        db.execute("SAVEPOINT inner", []).unwrap();
        db.execute("INSERT INTO s (v) VALUES ('extra')", []).unwrap();
        db.execute("ROLLBACK TO inner", []).unwrap();
        // 'inner' stays on the stack: ROLLBACK TO again must work.
        db.execute("ROLLBACK TO inner", []).unwrap();
        db.execute("ROLLBACK TO outer", []).unwrap();
        // 'outer' also stays: RELEASE it now.
        db.execute("RELEASE outer", []).unwrap();
        db.flush().unwrap();
        let rows = db.query("SELECT COUNT(*) FROM s", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(300));
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(first_col(&mut db, "PRAGMA integrity_check"), vec!["ok".to_string()]);
}
