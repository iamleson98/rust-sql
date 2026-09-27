//! The 3+-way split: a page packed to near-avail with sizeable cells can
//! receive a mid-key insert where NO contiguous 2-partition fits — every
//! split point overloads one half past the page budget. Observed shape:
//! a leaf holding two ~1.9 KB blob rows takes a ~2.5 KB INSERT OR REPLACE
//! between them (cells [1717, 2493, 1941] bytes against a 4084-byte
//! budget; both 2-way points put ≥ 4210 on one side). The old code
//! surfaced this as `corruption: leaf page N cannot split: no feasible
//! byte-aware point`; the split now distributes the cells across 3+
//! pages (run 1 keeps the old page identity, every boundary reports a
//! separator to the parent exactly like an ordinary split).
//!
//! These tests pin: the direct corner (deterministic shape), the original
//! fuzz workload (12 rounds × 25 mixed ops, the exact LCG the probe
//! used), the index-tree variant (giant keys packing an interior), the
//! right-edge variant (the mid-key insert landing on the tree's right
//! edge), and the SQLite-format container (the splice paths consume the
//! same engine b-tree code).

use rustqlite::{Database, Value};

fn as_int(v: &Value) -> i64 {
    match v {
        Value::Integer(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn integrity_ok(db: &mut Database) {
    let rows = db.query("PRAGMA integrity_check", []).unwrap();
    assert!(
        rows.iter()
            .any(|r| r.len() == 1 && format!("{:?}", r[0]).contains("ok")),
        "integrity_check not ok: {rows:?}"
    );
}

/// Deterministic corner: two ~2000-byte rows pack a 4 KiB leaf, then a
/// ~2400-byte row lands BETWEEN them — both 2-way split points overload
/// one half. Pre-fix this was the exact `cannot split` corruption.
#[test]
fn multiway_split_direct_corner() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("native.db")).unwrap();
    db.execute(
        "CREATE TABLE m(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, blob BLOB)",
        [],
    )
    .unwrap();
    // 2000-byte payloads: two of them = ~4030 cell bytes on the leaf
    // (budget 4084) — packed.
    let blob = |n: usize| Value::Blob(vec![b'x'; n]);
    db.execute(
        "INSERT INTO m(id, a, b, blob) VALUES (1, 'aaa', 10, ?)",
        [blob(2000)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO m(id, a, b, blob) VALUES (3, 'ccc', 30, ?)",
        [blob(1990)],
    )
    .unwrap();
    // Mid-key row, bigger than the leftover gap: rowid 2 between 1 and 3.
    db.execute(
        "INSERT INTO m(id, a, b, blob) VALUES (2, 'bbb', 20, ?)",
        [blob(2400)],
    )
    .unwrap();
    // All three rows visible, payloads exact.
    for (id, len) in [(1i64, 2000usize), (2, 2400), (3, 1990)] {
        let rows = db
            .query(
                "SELECT length(blob) FROM m WHERE id = ?",
                [Value::Integer(id)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "row {id} missing after the 3-way split");
        assert_eq!(rows[0][0], Value::Integer(len as i64));
    }
    let count = db.query("SELECT count(*) FROM m", []).unwrap();
    assert_eq!(count[0][0], Value::Integer(3));
    integrity_ok(&mut db);
    // Reopen: the on-disk shape must be durable.
    drop(db);
    let mut db = Database::open(dir.path().join("native.db")).unwrap();
    let count = db.query("SELECT count(*) FROM m", []).unwrap();
    assert_eq!(count[0][0], Value::Integer(3));
    integrity_ok(&mut db);
}

/// The full reproducer workload (examples/probe_native_fuzz.rs): 12
/// rounds × 25 mixed ops (INSERT OR REPLACE with 0–2499-byte blobs,
/// DELETE, WITHOUT ROWID upserts, range UPDATEs) against a 300-id keyspace
/// on the native file container — failed at round 8 pre-fix.
#[test]
fn multiway_split_replace_fuzz_native() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.db");
    let mut db = Database::open(&path).unwrap();
    db.execute(
        "CREATE TABLE m(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, blob BLOB)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ixm ON m(b)", []).unwrap();
    db.execute(
        "CREATE TABLE w(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
        [],
    )
    .unwrap();
    let mut lcg = 0x1234_5678u64;
    let mut rnd = || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (lcg >> 33) as i64
    };
    for round in 0..12 {
        for _ in 0..25 {
            let op = rnd().rem_euclid(4);
            let id = 1 + rnd().rem_euclid(300);
            match op {
                0 => {
                    let blob_len = (rnd().rem_euclid(2500)) as usize;
                    let blob: Vec<u8> = (0..blob_len).map(|i| (i % 251) as u8).collect();
                    db.execute(
                        "INSERT OR REPLACE INTO m(id, a, b, blob) VALUES (?, ?, ?, ?)",
                        [
                            Value::Integer(id),
                            Value::Text(format!("s{id}-{round}").into()),
                            Value::Integer(rnd()),
                            Value::Blob(blob),
                        ],
                    )
                    .unwrap();
                }
                1 => {
                    db.execute("DELETE FROM m WHERE id = ?", [Value::Integer(id)])
                        .unwrap();
                }
                2 => {
                    db.execute(
                        "INSERT OR REPLACE INTO w(k, v) VALUES (?, ?)",
                        [Value::Text(format!("k{id}").into()), Value::Integer(round)],
                    )
                    .unwrap();
                }
                _ => {
                    db.execute("UPDATE m SET b = b + 1 WHERE id < ?", [Value::Integer(id)])
                        .unwrap();
                }
            }
        }
    }
    integrity_ok(&mut db);
    // Row payloads round-trip: every surviving row's blob length matches
    // its checksum prefix.
    let rows = db.query("SELECT id, length(blob) FROM m", []).unwrap();
    for r in &rows {
        let id = as_int(&r[0]);
        let len = as_int(&r[1]) as usize;
        let one = db
            .query(
                "SELECT substr(blob, 1, 8) FROM m WHERE id = ?",
                [Value::Integer(id)],
            )
            .unwrap();
        let expect: Vec<u8> = (0..len.min(8)).map(|i| (i % 251) as u8).collect();
        match &one[0][0] {
            Value::Blob(b) if b.len() == len.min(8) && b[..] == expect[..] => {}
            v => panic!("row {id} payload corrupted: {v:?}"),
        }
    }
    // Durability: reopen and re-verify.
    drop(db);
    let mut db = Database::open(&path).unwrap();
    integrity_ok(&mut db);
    let n = db.query("SELECT count(*) FROM m", []).unwrap();
    assert!(as_int(&n[0][0]) > 0);
}

/// Index-tree variant: ~2 KB indexed values pack an INDEX leaf the same
/// way blob rows pack a table leaf — two entries fill the page, and a
/// mid-key third entry bigger than the leftover gap makes every 2-way
/// point overload one half. The multi-way split must keep every
/// (key, rowid) pair findable.
#[test]
fn multiway_split_index_giant_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("native.db")).unwrap();
    db.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT)", [])
        .unwrap();
    db.execute("CREATE UNIQUE INDEX ik ON t(k)", []).unwrap();
    // Plant the corner deterministically: 'a...' + 'c...' pack one leaf
    // (~4030 cell bytes, budget 4084), then 'b...' (bigger than the gap)
    // lands between them.
    let key = |prefix: &str, pad: usize| format!("{prefix}{}", "y".repeat(pad));
    db.execute(
        "INSERT INTO t(id, k) VALUES (1, ?)",
        [Value::Text(key("a", 2100).into())],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t(id, k) VALUES (3, ?)",
        [Value::Text(key("c", 1984).into())],
    )
    .unwrap();
    db.execute(
        "INSERT INTO t(id, k) VALUES (2, ?)",
        [Value::Text(key("b", 2400).into())],
    )
    .unwrap();
    // Grow the tree several levels with alternating packing.
    db.execute("BEGIN", []).unwrap();
    for i in 4..=200i64 {
        let (prefix, pad) = if i % 2 == 0 {
            (format!("k{:04}-", i), 1900)
        } else {
            (format!("k{:04}-", i), 40)
        };
        db.execute(
            "INSERT INTO t(id, k) VALUES (?, ?)",
            [Value::Integer(i), Value::Text(key(&prefix, pad).into())],
        )
        .unwrap();
    }
    db.execute("COMMIT", []).unwrap();
    integrity_ok(&mut db);
    // The three corner keys first.
    for (id, prefix, pad) in [(1i64, "a", 2100usize), (2, "b", 2400), (3, "c", 1984)] {
        let rows = db
            .query(
                "SELECT id FROM t WHERE k = ?",
                [Value::Text(key(prefix, pad).into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "corner key {prefix} lost after the split");
        assert_eq!(rows[0][0], Value::Integer(id));
    }
    // Then every grown key findable through the index.
    for i in 4..=200i64 {
        let (prefix, pad) = if i % 2 == 0 {
            (format!("k{:04}-", i), 1900)
        } else {
            (format!("k{:04}-", i), 40)
        };
        let rows = db
            .query(
                "SELECT id FROM t WHERE k = ?",
                [Value::Text(key(&prefix, pad).into())],
            )
            .unwrap();
        assert_eq!(rows.len(), 1, "key {i} lost after index splits");
        assert_eq!(rows[0][0], Value::Integer(i));
    }
    // Full scans agree with the count.
    let n = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(200));
    integrity_ok(&mut db);
    drop(db);
    let mut db = Database::open(dir.path().join("native.db")).unwrap();
    integrity_ok(&mut db);
    let n = db.query("SELECT count(*) FROM t", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(200));
}

/// Right-edge variant: the packing page is the tree's right-most leaf, so
/// the parent splice takes the right-most branch (append cell + move the
/// right-most slot), and — when the leaf is also the ROOT — the root
/// split installs multiple boundaries at once.
#[test]
fn multiway_split_root_and_right_edge() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("native.db")).unwrap();
    db.execute("CREATE TABLE m(id INTEGER PRIMARY KEY, blob BLOB)", [])
        .unwrap();
    let blob = |n: usize| Value::Blob(vec![b'z'; n]);
    // Three ~2 KB rows in DESCENDING id order: each insert lands at the
    // tree's right... no — descending ids land at the LEFT edge; use
    // ascending ids but replace the middle one with a giant payload so
    // the split fires while the tree is still shallow (root leaf).
    db.execute("INSERT INTO m(id, blob) VALUES (1, ?)", [blob(1900)])
        .unwrap();
    db.execute("INSERT INTO m(id, blob) VALUES (3, ?)", [blob(1900)])
        .unwrap();
    // This one splits the ROOT leaf 3-way (page budget 4084, cells
    // ~1930 + ~2430 + ~1930).
    db.execute("INSERT INTO m(id, blob) VALUES (2, ?)", [blob(2400)])
        .unwrap();
    let n = db.query("SELECT count(*) FROM m", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(3));
    integrity_ok(&mut db);
    // Keep growing past the root split: the interior now has multiple
    // boundaries; further splits exercise the parent-splice multi path.
    for i in 4..=60i64 {
        let pad = if i % 3 == 0 { 2400 } else { 1700 };
        db.execute(
            "INSERT INTO m(id, blob) VALUES (?, ?)",
            [Value::Integer(i), blob(pad)],
        )
        .unwrap();
    }
    let n = db.query("SELECT count(*) FROM m", []).unwrap();
    assert_eq!(n[0][0], Value::Integer(60));
    integrity_ok(&mut db);
    // ORDER BY exercises every leaf chain + separator routing.
    let rows = db.query("SELECT id FROM m ORDER BY id", []).unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| as_int(&r[0])).collect();
    assert_eq!(ids, (1..=60).collect::<Vec<_>>());
    drop(db);
    let mut db = Database::open(dir.path().join("native.db")).unwrap();
    integrity_ok(&mut db);
    let rows = db.query("SELECT id FROM m ORDER BY id DESC", []).unwrap();
    assert_eq!(rows.len(), 60);
    assert_eq!(rows[0][0], Value::Integer(60));
}

/// The SQLite-format container runs the same engine b-tree code through
/// its splice paths — the blob-replace workload must survive there too,
/// and the result must verify in REAL SQLite.
#[test]
fn multiway_split_sqlite_format_container() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("share.db");
    let mut db = Database::open_sqlite_format(&path).unwrap();
    db.execute(
        "CREATE TABLE m(id INTEGER PRIMARY KEY, a TEXT, b INTEGER, blob BLOB)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ixm ON m(b)", []).unwrap();
    // Deterministic corner first: pack a leaf with two ~2 KB rows, then
    // replace-insert a bigger one between them (3-way split through the
    // splice path).
    let blob = |n: usize| Value::Blob(vec![b'w'; n]);
    db.execute(
        "INSERT INTO m(id, a, b, blob) VALUES (1, 'a', 1, ?)",
        [blob(2000)],
    )
    .unwrap();
    db.execute(
        "INSERT INTO m(id, a, b, blob) VALUES (3, 'c', 3, ?)",
        [blob(1990)],
    )
    .unwrap();
    db.execute(
        "INSERT OR REPLACE INTO m(id, a, b, blob) VALUES (2, 'b', 2, ?)",
        [blob(2400)],
    )
    .unwrap();
    integrity_ok(&mut db);
    let mut lcg = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (lcg >> 33) as i64
    };
    for _ in 0..260 {
        let id = 1 + rnd().rem_euclid(120);
        let blob_len = (rnd().rem_euclid(2600)) as usize;
        let blob: Vec<u8> = (0..blob_len).map(|i| (i % 249) as u8).collect();
        db.execute(
            "INSERT OR REPLACE INTO m(id, a, b, blob) VALUES (?, ?, ?, ?)",
            [
                Value::Integer(id),
                Value::Text(format!("s{id}").into()),
                Value::Integer(rnd()),
                Value::Blob(blob),
            ],
        )
        .unwrap();
    }
    integrity_ok(&mut db);
    let n = db.query("SELECT count(*) FROM m", []).unwrap()[0][0].clone();
    drop(db);
    // Real SQLite must open the file and agree (interop both directions).
    let sqlite_bin =
        std::env::var("RUSTQLITE_SQLITE3_BIN").unwrap_or_else(|_| "sqlite3".to_string());
    if std::process::Command::new(&sqlite_bin)
        .arg(&path)
        .arg("SELECT count(*) FROM m;")
        .output()
        .is_ok()
    {
        let out = std::process::Command::new(&sqlite_bin)
            .arg(&path)
            .arg("SELECT count(*) FROM m; PRAGMA integrity_check;")
            .output()
            .expect("sqlite3 run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first = stdout.lines().next().unwrap_or("").trim();
        assert_eq!(
            first.parse::<i64>().ok(),
            Some(as_int(&n)),
            "SQLite count mismatch: {stdout}"
        );
        assert!(
            stdout.lines().skip(1).any(|l| l.trim() == "ok"),
            "SQLite integrity_check failed: {stdout}"
        );
    }
}
