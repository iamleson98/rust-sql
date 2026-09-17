//! On-disk format upgrade: `RSQLDB03`/`RSQLDB04` files (legacy index
//! order-key encoding) must migrate to the current prefix-free `RSQLDB05`
//! encoding at open — a full logical rebuild that never reads a legacy
//! index tree (raw table scans + DDL replay; indexes rebuild from rows).
//!
//! Fixtures were produced by `examples/make_v4_fixture.rs` built from the
//! PRE-RSQLDB05 tree: every index entry in them carries the OLD key
//! encoding, including the prefix-collision classes the new format fixes
//! (empty-string vs NUL-leading text, small-int vs big-int keys, empty
//! blob vs NUL blob) — so the post-upgrade assertions double as
//! regression pins for the encoding itself.

use rustqlite::{Database, Value};
use std::io::Read;
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copy a fixture (plus any `-wal` sidecar) into a temp dir so the
/// migration's in-place rewrite never touches the committed fixture.
fn stage(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let dst = dir.path().join(name);
    std::fs::copy(fixture(name), &dst).unwrap();
    let wal = fixture(&format!("{name}-wal"));
    if wal.exists() {
        std::fs::copy(&wal, dir.path().join(format!("{name}-wal"))).unwrap();
    }
    dst
}

fn magic(path: &std::path::Path) -> String {
    let mut f = std::fs::File::open(path).unwrap();
    let mut buf = [0u8; 8];
    f.read_exact(&mut buf).unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

fn qi(db: &Database, sql: &str) -> i64 {
    let rows = db.query(sql, ()).unwrap();
    match rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(i)) => *i,
        ref other => panic!("expected single integer for {sql:?}, got {other:?}"),
    }
}

fn q_count(db: &Database, sql: &str) -> i64 {
    qi(db, sql)
}

/// The 2^53 boundary of the OLD 9-byte small-int key encoding.
const TWO53: i64 = 9_007_199_254_740_992;

/// The full data battery: every collision-sensitive shape the fixture
/// encodes, asserted against the UPGRADED database (and reused for the
/// V3-patched and WAL variants).
fn assert_fixture_data(db: &mut Database) {
    // Row counts and rowid preservation (sparse explicit rowids).
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text"), 20);
    assert_eq!(q_count(db, "SELECT max(rowid) FROM t_text"), 1001);
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE rowid = 1000"),
        1
    );
    assert_eq!(
        q_count(db, "SELECT id FROM t_text WHERE rowid = 1000"),
        1000
    );

    // TEXT index keys: the empty string matches exactly its own row.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text WHERE v = ''"), 1);
    assert_eq!(q_count(db, "SELECT id FROM t_text WHERE v = ''"), 1);
    // Shared prefixes don't over-match.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text WHERE v = 'ab'"), 0);
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE v = 'abc'"),
        1
    );
    assert_eq!(q_count(db, "SELECT id FROM t_text WHERE v = 'abc'"), 3);
    // 'abc' vs 'abcd': shorter key is NOT a prefix-match of the longer.
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE v LIKE 'abc'"),
        1
    );
    // Unicode text keys survive the rebuild.
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE v = 'héllo'"),
        1
    );

    // NUL-bearing text (bound-parameter rows): distinct from ''.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text WHERE id = 90"), 1);
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text WHERE id = 91"), 1);

    // INTEGER index keys across the old 2^53 encoding boundary: an
    // equality probe for 2^53 must not prefix-match 2^53 + k.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text WHERE n = 100"), 1);
    for (probe, want) in [
        (TWO53 - 1, 1),
        (TWO53, 1),
        (TWO53 + 1, 1),
        (TWO53 + 2, 1),
        (-TWO53, 1),
        (-TWO53 + 1, 1),
        (i64::MAX, 1),
        (i64::MIN, 1),
        (TWO53 + 3, 0),
        (-TWO53 - 1, 0),
    ] {
        let got = q_count(
            db,
            &format!("SELECT count(*) FROM t_text WHERE n = {probe}"),
        );
        assert_eq!(got, want, "n = {probe}");
    }
    assert_eq!(q_count(db, "SELECT id FROM t_text WHERE n = 100"), 1);

    // BLOB index keys: x'' matches only the empty-blob rows.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_text WHERE b = x''"), 3);
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE b = x'00'"),
        1
    );
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE b = x'00AA'"),
        1
    );
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE b = x'00' || x''"),
        1
    );
    // NULL blobs are exempt from the index (count via table): id 6 plus
    // every row inserted without a `b` value (ids 20-27, 1000).
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_text WHERE b IS NULL"),
        10
    );

    // Overflow payload row (20 KB blob spans overflow pages).
    assert_eq!(
        q_count(
            db,
            "SELECT count(*) FROM t_text WHERE id = 1001 AND length(b) = 20000"
        ),
        1
    );

    // WITHOUT ROWID table: logical PK intact, pad index intact.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_wr"), 4);
    let pad: String = match &db
        .query("SELECT pad FROM t_wr WHERE k1 = '' AND k2 = 5", ())
        .unwrap()[0][0]
    {
        Value::Text(t) => t.to_string(),
        other => panic!("pad: {other:?}"),
    };
    assert_eq!(pad, "p4");
    assert_eq!(q_count(db, "SELECT count(*) FROM t_wr WHERE pad = 'p2'"), 1);

    // UNIQUE text index: the empty-string row is there exactly once, and
    // uniqueness still fires post-upgrade.
    assert_eq!(q_count(db, "SELECT count(*) FROM t_uniq WHERE v = ''"), 1);
    assert_eq!(q_count(db, "SELECT count(*) FROM t_uniq"), 3);
    assert!(db
        .execute("INSERT INTO t_uniq (v) VALUES ('')", ())
        .is_err());

    // Expression index + partial index: entries rebuilt from the rows.
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_expr WHERE a + b = 23"),
        1
    );
    assert_eq!(q_count(db, "SELECT count(*) FROM t_expr WHERE a = 5"), 1);
    assert_eq!(q_count(db, "SELECT count(*) FROM t_expr"), 4);

    // Generated columns (virtual + stored) recompute on the target.
    assert_eq!(
        q_count(
            db,
            "SELECT count(*) FROM t_gen WHERE base = -3 AND twice = -6 AND quad = -12"
        ),
        1
    );
    assert_eq!(q_count(db, "SELECT count(*) FROM t_gen"), 4);

    // View survived the replay.
    assert_eq!(q_count(db, "SELECT count(*) FROM v_notes"), 3);

    // AUTOINCREMENT deleted-tail: the high-water (50) is ABOVE the max
    // live rowid (9) — the next auto rowid must be 51, not 50.
    assert_eq!(
        q_count(db, "SELECT seq FROM sqlite_sequence WHERE name = 't_alias'"),
        50
    );
    db.execute("INSERT INTO t_alias (note) VALUES ('post')", ())
        .unwrap();
    assert_eq!(qi(db, "SELECT max(id) FROM t_alias"), 51);
    // The trigger fired for the post-upgrade insert too.
    assert_eq!(
        q_count(db, "SELECT count(*) FROM t_alias WHERE note = 'seen'"),
        3
    );

    // user_version survived.
    assert_eq!(q_count(db, "PRAGMA user_version"), 4242);
}

#[test]
fn v4_fixture_upgrades_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let path = stage(&dir, "v4_fixture.db");
    assert_eq!(magic(&path), "RSQLDB04");

    // The open itself performs the migration.
    let mut db = Database::open(&path).unwrap();
    assert_eq!(magic(&path), "RSQLDB05", "magic rewritten in place");
    assert_fixture_data(&mut db);
    drop(db);

    // Re-open: no second migration, data identical.
    let db2 = Database::open(&path).unwrap();
    assert_eq!(magic(&path), "RSQLDB05");
    assert_eq!(q_count(&db2, "SELECT count(*) FROM t_text"), 20);
    assert_eq!(q_count(&db2, "SELECT count(*) FROM t_text WHERE v = ''"), 1);
    // No upgrade temp file left behind.
    assert!(!dir.path().join("v4_fixture.db.rsql05.tmp").exists());
}

#[test]
fn v4_fixture_with_wal_sidecar_folds_committed_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = stage(&dir, "v4_fixture_wal.db");
    assert_eq!(magic(&path), "RSQLDB04");
    assert!(dir.path().join("v4_fixture_wal.db-wal").exists());

    let db = Database::open(&path).unwrap();
    assert_eq!(magic(&path), "RSQLDB05");
    // The committed WAL rows survive the rebuild...
    assert_eq!(q_count(&db, "SELECT count(*) FROM t_text"), 2);
    let v: String = match &db.query("SELECT v FROM t_text WHERE id = 2", ()).unwrap()[0][0] {
        Value::Text(t) => t.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(v, "wal-row-2");
    // ...and the stale sidecar (legacy-format frames) is retired.
    assert!(!dir.path().join("v4_fixture_wal.db-wal").exists());
    drop(db);

    // Reopen: still consistent.
    let db2 = Database::open(&path).unwrap();
    assert_eq!(q_count(&db2, "SELECT count(*) FROM t_text"), 2);
}

#[test]
fn v3_magic_is_accepted_and_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let path = stage(&dir, "v4_fixture.db");
    // Simulate the V3 magic (its index keys are the SAME legacy family;
    // the logical-rebuild path never touches the freelist).
    {
        use std::io::{Seek, Write};
        let mut f = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        f.write_all(b"RSQLDB03").unwrap();
    }
    assert_eq!(magic(&path), "RSQLDB03");
    let db = Database::open(&path).unwrap();
    assert_eq!(magic(&path), "RSQLDB05");
    assert_eq!(q_count(&db, "SELECT count(*) FROM t_text"), 20);
    assert_eq!(q_count(&db, "SELECT count(*) FROM t_text WHERE v = ''"), 1);
}

#[test]
fn pre_v3_magic_is_rejected_with_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = stage(&dir, "v4_fixture.db");
    {
        use std::io::{Seek, Write};
        let mut f = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        f.write_all(b"RSQLDB02").unwrap();
    }
    let err = Database::open(&path)
        .err()
        .expect("RSQLDB02 must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("unsupported database format version"),
        "actionable message, got: {msg}"
    );
}

#[test]
fn fresh_files_write_the_current_magic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (a)", ()).unwrap();
        db.execute("INSERT INTO t VALUES (1)", ()).unwrap();
    }
    assert_eq!(magic(&path), "RSQLDB05");
    let db = Database::open(&path).unwrap();
    assert_eq!(q_count(&db, "SELECT count(*) FROM t"), 1);
}
