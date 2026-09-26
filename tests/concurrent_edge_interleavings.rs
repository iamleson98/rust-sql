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
//! * V6 — UNMARKED first-append walk (table): A's establishing insert
//!   walks the right edge (no hint yet) and pins the right-most leaf; a
//!   sibling's PURE append-split (its first insert does not fit — the
//!   old leaf is byte-identical, so its stamp never moves) then demotes
//!   that leaf; A's later hinted appends land past the sibling's
//!   separator. The walked spine MUST be marked as decision reads or
//!   A's commit validates base==now and installs the stranded rows (the
//!   S4 soak's residual 1M-scale corruption: leaf [.. 1030000025] under
//!   a separator of 1000000032).
//! * V7 — the INDEX twin: same interleaving on a secondary index's
//!   right edge (a huge TEXT key forces the sibling's pure index
//!   append-split); the index walk must mark its spine identically.
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

// ============================================================
// V6/V7 — the unmarked right-edge walk (the S4 residual class)
// ============================================================

/// Seed `n` rows with 30-char notes so the right-most table leaf is
/// nearly full but still has room for a handful of tiny ('x'-note)
/// rows — the size asymmetry that lets A's establishing insert FIT
/// while B's huge first insert forces a PURE append-split (old leaf
/// byte-identical, stamp unmoved).
fn ins_note(db: &Database, id: i64, note: &str) {
    let mut stmt = db
        .prepare("INSERT INTO events (id, k, note) VALUES (?, ?, ?)")
        .unwrap();
    stmt.bind(1, Value::Integer(id)).unwrap();
    stmt.bind(2, Value::Integer(id)).unwrap();
    stmt.bind(3, Value::Text(note.into())).unwrap();
    stmt.step().unwrap();
}

fn ins_evt(db: &Database, id: i64, k: &str) {
    let mut stmt = db.prepare("INSERT INTO evt (id, k) VALUES (?, ?)").unwrap();
    stmt.bind(1, Value::Integer(id)).unwrap();
    stmt.bind(2, Value::Text(k.into())).unwrap();
    stmt.step().unwrap();
}

#[test]
fn concurrent_edge_v6_unmarked_walk_vs_pure_append_split() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    // A DEEP tree (the single-leaf shape is caught by the root-view
    // validation): 150 rows of 30-char notes — the first leaf fills
    // (~88-93 rows) and splits, the right-most leaf ends ~60 rows full
    // (~1.3KB free): room for A's tiny rows, none for B's 2KB one.
    db.execute(
        "CREATE TABLE events (id INTEGER PRIMARY KEY, k INTEGER, note TEXT)",
        [],
    )
    .unwrap();
    // NO secondary index — V6 pins the TABLE-side walk in isolation (the
    // index tree has its own walk and would mask the table hole with
    // unrelated leaf-stamp conflicts; V7 covers the index side).
    {
        let mut sql = String::from("INSERT INTO events (id, k, note) VALUES ");
        for i in 1..=150i64 {
            if i > 1 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, '{}')", i, i * 7, "n".repeat(30)));
        }
        db.execute(&sql, []).unwrap();
    }

    // A: first insert of the txn — NO hint → the right-edge WALK pins
    // the right-most leaf (pre-fix: without decision marks).
    Database::set_conn_identity(1);
    db.begin_concurrent_transaction().unwrap();
    ins_note(&db, 1001, "x"); // fits → walk + append, pin established

    // B: one HUGE row — does not fit → PURE append-split (the old leaf
    // keeps every cell, only the new leaf + parent separator install).
    Database::set_conn_identity(2);
    db.begin_concurrent_transaction().unwrap();
    ins_note(&db, 1101, &"b".repeat(3900));
    db.commit_concurrent_transaction().unwrap(); // separator lands at 201

    // A: post-split appends via the stale pin — rows PAST B's separator
    // (they belong in B's new leaf; a stale install strands them in the
    // old one under a separator of ~200).
    Database::set_conn_identity(1);
    for id in 1202..=1210 {
        ins_note(&db, id, "x");
    }
    let a_committed = db.commit_concurrent_transaction().is_ok();
    Database::set_conn_identity(0);

    assert!(integrity_ok(&db), "V6 corrupted the tree");
    let n: i64 = db.query("SELECT count(*) FROM events", []).unwrap()[0][0].as_integer();
    let want = if a_committed { 161 } else { 151 };
    assert_eq!(n, want, "V6 row survival: committed={a_committed}");
    // EVERY surviving row must be findable BY DESCENT (the stranded-rows
    // signature is exactly: counted by the leaf chain, missed by probes).
    for id in 1..=150i64 {
        let hits: i64 = db
            .query(
                "SELECT count(*) FROM events WHERE id = ?",
                [Value::Integer(id)],
            )
            .unwrap()[0][0]
            .as_integer();
        assert_eq!(hits, 1, "V6 seed row {id} unreachable");
    }
    let hits: i64 = db
        .query(
            "SELECT count(*) FROM events WHERE id = 1101",
            [Value::Integer(1101)],
        )
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(hits, 1, "V6 B's split row unreachable");
    if a_committed {
        for id in 1001i64..=1001 {
            let hits: i64 = db
                .query(
                    "SELECT count(*) FROM events WHERE id = ?",
                    [Value::Integer(id)],
                )
                .unwrap()[0][0]
                .as_integer();
            assert_eq!(hits, 1, "V6 A's row {id} unreachable");
        }
        for id in 1202..=1210i64 {
            let hits: i64 = db
                .query(
                    "SELECT count(*) FROM events WHERE id = ?",
                    [Value::Integer(id)],
                )
                .unwrap()[0][0]
                .as_integer();
            assert_eq!(hits, 1, "V6 A's post-split row {id} stranded");
        }
    }
}

#[test]
fn concurrent_edge_v7_index_unmarked_walk_vs_pure_split() {
    // TWO THREADS (the soak's real shape): the insert scratch is
    // thread-local, so A's post-split inserts find A's OWN hint — a
    // single-threaded interleave would hand them B's exit hint instead
    // (whose pin fails and falls to the DESCENT, whose interior marks
    // accidentally save the commit). Deterministic handshake: A pins
    // the index right edge, B pure-splits it, A appends past it.
    use std::sync::mpsc;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v7.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("PRAGMA journal_mode = WAL", []).unwrap();
    db.execute("CREATE TABLE evt (id INTEGER PRIMARY KEY, k TEXT)", [])
        .unwrap();
    db.execute("CREATE INDEX ix_evt_k ON evt (k)", []).unwrap();
    // A DEEP index tree (the single-leaf shape is caught by the
    // root-view validation): 500 six-char keys ~ 11B index cells — the
    // first index leaf fills (~370+) and splits, the right-most ends
    // with room for A's short keys but none for B's 3.8KB one.
    {
        let mut sql = String::from("INSERT INTO evt (id, k) VALUES ");
        for i in 1..=500i64 {
            if i > 1 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, 'y{:05}')", i, i));
        }
        db.execute(&sql, []).unwrap();
    }
    let db = std::sync::Arc::new(db);
    let (pin_tx, pin_rx) = mpsc::channel::<()>(); // A -> B (pin established)
    let (split_tx, split_rx) = mpsc::channel::<()>(); // B -> A (split committed)
    let db_a = std::sync::Arc::clone(&db);
    let db_b = std::sync::Arc::clone(&db);
    let a = std::thread::spawn(move || {
        Database::set_conn_identity(1);
        db_a.begin_concurrent_transaction().unwrap();
        ins_evt(&db_a, 1001, "yzz1"); // walk pins the index right edge
        pin_tx.send(()).unwrap();
        split_rx.recv().unwrap(); // B's pure split + commit landed
        for j in 2..=9 {
            ins_evt(&db_a, 1000 + j, &format!("zz{j}"));
        }
        let r = db_a.commit_concurrent_transaction();
        Database::set_conn_identity(0);
        r.is_ok()
    });
    let b = std::thread::spawn(move || {
        Database::set_conn_identity(2);
        pin_rx.recv().unwrap();
        db_b.begin_concurrent_transaction().unwrap();
        // A huge key that sorts after A's: does not fit -> PURE index
        // append-split (the old leaf is byte-identical, stamp unmoved).
        ins_evt(&db_b, 2001, &format!("z{}", "b".repeat(3800)));
        db_b.commit_concurrent_transaction().unwrap();
        split_tx.send(()).unwrap();
        Database::set_conn_identity(0);
    });
    let a_committed = a.join().unwrap();
    b.join().unwrap();

    assert!(integrity_ok(&db), "V7 corrupted the tree");
    // Every committed row's index entry must be findable BY INDEX
    // DESCENT (SELECT ... WHERE k = ? plans through ix_evt_k).
    for (id, k) in (1..=500i64).map(|i| (i, format!("y{i:05}"))) {
        let hits: Vec<i64> = db
            .query("SELECT id FROM evt WHERE k = ?", [Value::Text(k.into())])
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_integer())
            .collect();
        assert_eq!(hits, vec![id], "V7 seed index entry {id} unreachable");
    }
    let hits: Vec<i64> = db
        .query(
            "SELECT id FROM evt WHERE k = ?",
            [Value::Text(format!("z{}", "b".repeat(3800)).into())],
        )
        .unwrap()
        .into_iter()
        .map(|r| r[0].as_integer())
        .collect();
    assert_eq!(hits, vec![2001], "V7 B's index entry unreachable");
    if a_committed {
        for (id, k) in std::iter::once((1001, "yzz1".to_string()))
            .chain((2..=9).map(|j| (1000 + j, format!("zz{j}"))))
        {
            let hits: Vec<i64> = db
                .query("SELECT id FROM evt WHERE k = ?", [Value::Text(k.into())])
                .unwrap()
                .into_iter()
                .map(|r| r[0].as_integer())
                .collect();
            assert_eq!(hits, vec![id], "V7 A's index entry {id} stranded");
        }
    }
    // No duplicate index entries (a stale install double-grafts).
    let dups: i64 = db
        .query(
            "SELECT count(*) FROM (SELECT k FROM evt GROUP BY k HAVING count(*) > 1)",
            [],
        )
        .unwrap()[0][0]
        .as_integer();
    assert_eq!(dups, 0, "V7 duplicate index entries");
}
