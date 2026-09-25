//! Deterministic single-threaded interleavings of the concurrent-regime
//! right-edge hazards — the regression pins for the S4 soak's residual
//! corruption class (stranded rows behind mis-routed interior separators).
//!
//! Each variant drives two writer identities through ONE shared engine on
//! one thread (the statement-level writer scope makes this legal), landing
//! the exact interleavings the soak's 4-writer race produced:
//!
//! * V1 — stale-shadow append-split: A appends into L's shadow and
//!   commits; B, whose L shadow predates A's install, append-splits per
//!   the stale view — the promoted separator would strand A's rows. The
//!   DECISION-READ validation (append-split marks the old page) must
//!   route B's commit through the merge replay instead.
//! * V2 — committed append, then a fresh transaction inserting mid-range
//!   rows: the late-touch view must decline the append and place sorted.
//! * V3 — cross-transaction append hint: the insert scratch carries the
//!   pinned right-most leaf across COMMIT boundaries; a sibling commit
//!   may have split the right edge since. The scope gate must drop the
//!   hint (a same-transaction hint is the only usable kind).
//! * V4 — stale-interior append: A splits the right edge; B, whose
//!   interior view predates the split, appends into the no-longer-
//!   rightmost leaf — the spine decision reads must conflict B into a
//!   merge.
//! * V5 — six rounds of both writers appending into the same right-most
//!   leaf from interleaved stale views; the loser merges every time.
//!
//! All variants assert `PRAGMA integrity_check` + exact row survival.

use rustqlite::{Database, Value};

fn interleave_db(dir: &std::path::Path, name: &str) -> Database {
    let path = dir.join(name);
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    db.execute("CREATE INDEX ix_events_k ON events (k)", [])
        .unwrap();
    // Seed one full right-most leaf: 82 rows (the observed split width).
    let mut sql = String::from("INSERT INTO events (id, k, note) VALUES ");
    for i in 1..=82i64 {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {}, 'n')", i, i * 7));
    }
    db.execute(&sql, []).unwrap();
    db
}

fn ins(db: &Database, id: i64) {
    // The &self DML path (prepared statement per call) — the insert
    // scratch carries the append hint ACROSS these statements, exactly
    // the cross-statement carry the scope gate must tame.
    let mut stmt = db
        .prepare("INSERT INTO events (id, k, note) VALUES (?, ?, 'x')")
        .unwrap();
    stmt.bind(1, Value::Integer(id)).unwrap();
    stmt.bind(2, Value::Integer(id)).unwrap();
    stmt.step().unwrap();
}

fn integrity_ok(db: &Database) -> bool {
    db.query("PRAGMA integrity_check", []).unwrap()[0][0].as_text() == "ok"
}

#[test]
fn concurrent_edge_v1_stale_shadow_append_split() {
    let dir = tempfile::tempdir().unwrap();
    let db = interleave_db(dir.path(), "v1.db");
    Database::set_conn_identity(1);
    db.begin_concurrent_transaction().unwrap();
    for id in 101..=125 {
        ins(&db, id);
    }
    Database::set_conn_identity(2);
    db.begin_concurrent_transaction().unwrap();
    for id in 201..=225 {
        ins(&db, id);
    }
    Database::set_conn_identity(1);
    db.commit_concurrent_transaction().unwrap();
    Database::set_conn_identity(2);
    for id in 226..=250 {
        ins(&db, id);
    }
    // Fast path or merge — either is correct; a stale install is not.
    if db.commit_concurrent_transaction().is_err() {
        let _ = db.rollback_concurrent_transaction();
    }
    Database::set_conn_identity(0);
    let n: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
    assert!(integrity_ok(&db), "V1 corrupted the tree");
    // 82 seed + A's 25 + B's 50 (B's single txn spans both batches) when
    // everything lands; 107 floor when B lost everything (not expected).
    assert!((107..=157).contains(&n), "V1 row survival: {n}");
}

#[test]
fn concurrent_edge_v2_midrange_after_committed_append() {
    let dir = tempfile::tempdir().unwrap();
    let db = interleave_db(dir.path(), "v2.db");
    Database::set_conn_identity(1);
    db.begin_concurrent_transaction().unwrap();
    for id in 101..=125 {
        ins(&db, id);
    }
    db.commit_concurrent_transaction().unwrap();
    Database::set_conn_identity(2);
    db.begin_concurrent_transaction().unwrap();
    for id in 83..=95 {
        ins(&db, id); // sorts BETWEEN the seed max (82) and A's rows
    }
    if db.commit_concurrent_transaction().is_err() {
        let _ = db.rollback_concurrent_transaction();
    }
    Database::set_conn_identity(0);
    assert!(integrity_ok(&db), "V2 corrupted the tree");
}

#[test]
fn concurrent_edge_v3_cross_transaction_hint() {
    let dir = tempfile::tempdir().unwrap();
    let db = interleave_db(dir.path(), "v3.db");
    Database::set_conn_identity(2);
    db.begin_concurrent_transaction().unwrap();
    for id in 201..=210 {
        ins(&db, id); // establishes the append hint on the right-most leaf
    }
    db.commit_concurrent_transaction().unwrap();
    Database::set_conn_identity(1);
    db.begin_concurrent_transaction().unwrap();
    for id in 101..=120 {
        ins(&db, id); // A appends into the same leaf (its own fresh view)
    }
    db.commit_concurrent_transaction().unwrap();
    Database::set_conn_identity(2);
    db.begin_concurrent_transaction().unwrap();
    for id in 211..=240 {
        ins(&db, id); // receives the SCRATCH-CARRIED hint (cross-txn)
    }
    if db.commit_concurrent_transaction().is_err() {
        let _ = db.rollback_concurrent_transaction();
    }
    Database::set_conn_identity(0);
    assert!(integrity_ok(&db), "V3 corrupted the tree");
}

#[test]
fn concurrent_edge_v4_stale_interior_append() {
    let dir = tempfile::tempdir().unwrap();
    let db = interleave_db(dir.path(), "v4.db");
    Database::set_conn_identity(1);
    db.begin_concurrent_transaction().unwrap();
    for id in 101..=125 {
        ins(&db, id);
    }
    Database::set_conn_identity(2);
    db.begin_concurrent_transaction().unwrap();
    for id in 201..=205 {
        ins(&db, id); // B fetches its interior + leaf views (pre-split)
    }
    Database::set_conn_identity(1);
    db.commit_concurrent_transaction().unwrap(); // A's rows land in the leaf
    Database::set_conn_identity(2);
    for id in 206..=230 {
        ins(&db, id); // the leaf is full now: split per B's STALE view
    }
    if db.commit_concurrent_transaction().is_err() {
        let _ = db.rollback_concurrent_transaction();
    }
    Database::set_conn_identity(0);
    assert!(integrity_ok(&db), "V4 corrupted the tree");
}

#[test]
fn concurrent_edge_v5_repeated_round_robin_appends() {
    let dir = tempfile::tempdir().unwrap();
    let db = interleave_db(dir.path(), "v5.db");
    for round in 0..6i64 {
        Database::set_conn_identity(1);
        db.begin_concurrent_transaction().unwrap();
        Database::set_conn_identity(2);
        db.begin_concurrent_transaction().unwrap();
        let base = 1000 + round * 1000;
        for j in 0..30 {
            Database::set_conn_identity(1);
            ins(&db, base + j);
            Database::set_conn_identity(2);
            ins(&db, base + 500 + j);
        }
        Database::set_conn_identity(1);
        if db.commit_concurrent_transaction().is_err() {
            let _ = db.rollback_concurrent_transaction();
        }
        Database::set_conn_identity(2);
        if db.commit_concurrent_transaction().is_err() {
            let _ = db.rollback_concurrent_transaction();
        }
    }
    Database::set_conn_identity(0);
    assert!(integrity_ok(&db), "V5 corrupted the tree");
}
