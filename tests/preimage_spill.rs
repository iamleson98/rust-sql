//! Savepoint pre-image spill correctness pins (storage::preimage).
//!
//! The undo machinery keeps the most recent pre-images in RAM and drains
//! the rest to an append-only temp file once the hot threshold is passed
//! (RSQL_PREIMAGE_HOT_PAGES, default 8192 pages = 32 MB). These tests
//! force the threshold to 2 so EVERY shape here spills — rollback and
//! ROLLBACK TO must restore byte-exact state through the file path, not
//! just the hot map. The scale discipline (a mass UPDATE touching every
//! leaf of a multi-GB table keeps RSS flat) is the mega marathon's M8
//! budget; these are the correctness pins for the machinery itself.

use rustqlite::Database;

fn open_db() -> (Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("p.db")).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("PRAGMA synchronous = NORMAL", []).unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER)", [])
        .unwrap();
    db.execute("CREATE INDEX ix_t_k ON t (k)", []).unwrap();
    (db, dir)
}

fn populate(db: &mut Database, lo: i64, hi: i64) {
    let mut next = lo;
    while next < hi {
        let n = (hi - next).min(2000);
        let mut sql = String::from("INSERT INTO t (id, k) VALUES ");
        for i in 0..n {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {})", next + i, (next + i) % 97));
        }
        db.execute(&sql, []).unwrap();
        next += n;
    }
}

fn state(db: &Database) -> (i64, i64, i64) {
    let row = db
        .query("SELECT count(*), sum(k), coalesce(max(k), -1) FROM t", [])
        .unwrap();
    let r = &row[0];
    (r[0].as_integer(), r[1].as_integer(), r[2].as_integer())
}

#[test]
fn begin_mass_update_rollback_exact_through_spill() {
    let (mut db, _dir) = open_db();
    populate(&mut db, 1, 30_001);
    let before = state(&db);
    db.execute("BEGIN", []).unwrap();
    db.execute("UPDATE t SET k = k + 1 WHERE id <= 30000", [])
        .unwrap();
    let mid = state(&db);
    assert_eq!(mid.0, before.0);
    assert_ne!(mid.1, before.1, "the update must change the k-sum");
    db.execute("ROLLBACK", []).unwrap();
    let after = state(&db);
    assert_eq!(after, before, "ROLLBACK must restore byte-exact state");
    db.execute("PRAGMA integrity_check", []).unwrap();
}

#[test]
fn rollback_to_savepoint_restores_spilled_images() {
    let (mut db, _dir) = open_db();
    populate(&mut db, 1, 20_001);
    let before = state(&db);
    db.execute("BEGIN", []).unwrap();
    db.execute("SAVEPOINT sp", []).unwrap();
    // Mutate a band, then walk pages with a second mass statement so the
    // level's log crosses many drain generations.
    db.execute("UPDATE t SET k = k + 5 WHERE id <= 10000", [])
        .unwrap();
    db.execute("UPDATE t SET k = k + 7 WHERE id <= 20000", [])
        .unwrap();
    let mid = state(&db);
    assert_ne!(mid.1, before.1);
    db.execute("ROLLBACK TO sp", []).unwrap();
    let back = state(&db);
    assert_eq!(back, before, "ROLLBACK TO must restore the savepoint state");
    db.execute("RELEASE sp", []).unwrap();
    db.execute("COMMIT", []).unwrap();
    let after = state(&db);
    assert_eq!(after, before, "the committed state is the savepoint state");
    db.execute("PRAGMA integrity_check", []).unwrap();
}

#[test]
fn autocommit_mass_update_stays_exact_with_spill() {
    // The mega marathon's M4 shape: ONE autocommit statement mutating a
    // page in every leaf — the engine's implicit transaction captures
    // (and spills) pre-images under it. Commit must land exactly.
    let (mut db, _dir) = open_db();
    populate(&mut db, 1, 25_001);
    let before = state(&db);
    db.execute(
        "UPDATE t SET k = k + 1 WHERE id % 3 = 0 AND id <= 25000",
        [],
    )
    .unwrap();
    let after = state(&db);
    assert_eq!(after.0, before.0);
    // Every third k advanced by exactly 1: sum grows by the count.
    let thirds = (1..=25_000).filter(|i| i % 3 == 0).count() as i64;
    assert_eq!(after.1, before.1 + thirds);
    db.execute("PRAGMA integrity_check", []).unwrap();
}

#[test]
fn nested_savepoints_across_drain_generations() {
    let (mut db, _dir) = open_db();
    populate(&mut db, 1, 12_001);
    let base = state(&db);
    db.execute("BEGIN", []).unwrap();
    db.execute("SAVEPOINT a", []).unwrap();
    db.execute("UPDATE t SET k = k + 1 WHERE id <= 6000", [])
        .unwrap();
    let at_a = state(&db);
    db.execute("SAVEPOINT b", []).unwrap();
    db.execute("UPDATE t SET k = k + 2 WHERE id <= 12000", [])
        .unwrap();
    let at_b = state(&db);
    assert_ne!(at_a, at_b);
    // ROLLBACK TO a restores the state AT SAVEPOINT a's creation
    // (base) — discarding both updates and their spilled pre-images.
    db.execute("ROLLBACK TO a", []).unwrap();
    // First ROLLBACK TO a restores the savepoint-creation state; the
    // second (idempotent re-rollback of the same name) must restore the
    // SAME bytes — the restore loop must not have captured its own
    // pre-restore images into the re-pushed level (the poison bug).
    assert_eq!(state(&db), base, "first ROLLBACK TO restores");
    db.execute("ROLLBACK TO a", []).unwrap();
    assert_eq!(state(&db), base, "idempotent double rollback");
    let _ = at_b;
    db.execute("ROLLBACK", []).unwrap();
    assert_eq!(state(&db), base, "full rollback through every generation");
    db.execute("PRAGMA integrity_check", []).unwrap();
}
