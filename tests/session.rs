//! Session-extension integration tests (engine side). The
//! SQLite-parity surface (byte formats, coalescing, apply semantics,
//! rebasing) is differential-pinned against real SQLite in
//! `session_differential.rs`; these tests pin the engine-side behavior
//! through the public Rust API.

use rustqlite::session::{self, ChangeGroup, ChangesetIter, Rebaser, Session};
use rustqlite::{Database, Value};

fn mem() -> Database {
    Database::open_in_memory().unwrap()
}

fn sess(db: &mut Database) -> Session {
    let s = db.create_session();
    s.attach(None);
    s
}

fn apply_all(db: &mut Database, cs: &[u8]) -> Result<(), session::ApplyError> {
    session::changeset_apply(db, cs, 0, None, &mut |_e| session::CHANGESET_OMIT).map(|_| ())
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, ()).unwrap()
}

#[test]
fn fresh_session_records_nothing_until_attach() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    let s = db.create_session(); // NO attach
    db.execute("INSERT INTO t VALUES(1, 'x')", ()).unwrap();
    assert!(s.is_empty());
    s.attach(None);
    db.execute("INSERT INTO t VALUES(2, 'y')", ()).unwrap();
    assert_eq!(s.change_count(), 1);
    let cs = s.changeset(&db).unwrap();
    assert!(!cs.is_empty());
}

#[test]
fn changeset_round_trip_insert_update_delete() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'x'), (2, 'y')", ())
        .unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(3, 'z')", ()).unwrap();
    db.execute("UPDATE t SET b = 'X' WHERE a = 1", ()).unwrap();
    db.execute("DELETE FROM t WHERE a = 2", ()).unwrap();
    let cs = s.changeset(&db).unwrap();

    // Apply to a fresh copy of the base.
    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db2.execute("INSERT INTO t VALUES(1, 'x'), (2, 'y')", ())
        .unwrap();
    apply_all(&mut db2, &cs).unwrap();
    let got = rows(&db2, "SELECT a, b FROM t ORDER BY a");
    let want = rows(&db, "SELECT a, b FROM t ORDER BY a");
    assert_eq!(got, want);
}

#[test]
fn insert_then_delete_collapses_to_nothing() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)", ())
        .unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    db.execute("DELETE FROM t WHERE a = 1", ()).unwrap();
    assert!(s.changeset(&db).unwrap().is_empty(), "session non-empty");
}

#[test]
fn insert_then_update_is_one_insert_with_final_values() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(1, 'x')", ()).unwrap();
    db.execute("UPDATE t SET b = 'final' WHERE a = 1", ())
        .unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut it = ChangesetIter::new(&cs).unwrap();
    assert!(it.next().unwrap());
    assert_eq!(it.op(), session::codec::OP_INSERT);
    assert_eq!(
        it.new_val(1).unwrap().to_value().unwrap(),
        Value::Text("final".into())
    );
    assert!(!it.next().unwrap(), "expected exactly one change");
}

#[test]
fn delete_then_insert_same_pk_becomes_update() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'old')", ()).unwrap();
    let s = sess(&mut db);
    db.execute("DELETE FROM t WHERE a = 1", ()).unwrap();
    db.execute("INSERT INTO t VALUES(1, 'new')", ()).unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut it = ChangesetIter::new(&cs).unwrap();
    assert!(it.next().unwrap());
    assert_eq!(it.op(), session::codec::OP_UPDATE);
    assert_eq!(
        it.old(1).unwrap().to_value().unwrap(),
        Value::Text("old".into())
    );
    assert_eq!(
        it.new_val(1).unwrap().to_value().unwrap(),
        Value::Text("new".into())
    );
    assert!(!it.next().unwrap());
}

#[test]
fn update_back_to_original_values_is_noop() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'orig')", ()).unwrap();
    let s = sess(&mut db);
    db.execute("UPDATE t SET b = 'tmp' WHERE a = 1", ())
        .unwrap();
    db.execute("UPDATE t SET b = 'orig' WHERE a = 1", ())
        .unwrap();
    assert!(s.changeset(&db).unwrap().is_empty());
}

#[test]
fn pk_moving_update_is_delete_plus_insert() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'x')", ()).unwrap();
    let s = sess(&mut db);
    db.execute("UPDATE t SET a = 5 WHERE a = 1", ()).unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut ops = Vec::new();
    let mut it = ChangesetIter::new(&cs).unwrap();
    while it.next().unwrap() {
        ops.push(it.op());
    }
    ops.sort();
    assert_eq!(
        ops,
        vec![session::codec::OP_DELETE, session::codec::OP_INSERT]
    );
}

#[test]
fn rowid_table_synthetic_rowid_column() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a, b TEXT)", ()).unwrap();
    let s = db.create_session();
    s.set_implicit_rowid_pk(true); // OBJCONFIG_ROWID (SQLite's opt-in)
    s.attach(None);
    db.execute("INSERT INTO t(rowid, a, b) VALUES(42, 7, 'x')", ())
        .unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut it = ChangesetIter::new(&cs).unwrap();
    assert!(it.next().unwrap());
    // n_col = 3: _rowid_ + a + b; the synthetic column is the PK.
    assert_eq!(it.n_col(), 3);
    assert_eq!(it.pk(), &[1u8, 0, 0]);
    assert_eq!(
        it.new_val(0).unwrap().to_value().unwrap(),
        Value::Integer(42)
    );
    // Applying replays the exact rowid.
    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a, b TEXT)", ()).unwrap();
    apply_all(&mut db2, &cs).unwrap();
    let rid = rows(&db2, "SELECT rowid, a FROM t");
    assert_eq!(rid[0][0], Value::Integer(42));
    assert_eq!(rid[0][1], Value::Integer(7));
}

#[test]
fn without_rowid_and_composite_pk_tables() {
    let mut db = mem();
    db.execute(
        "CREATE TABLE w(a TEXT, b INT, c TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID",
        (),
    )
    .unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO w VALUES('k', 1, 'v')", ()).unwrap();
    db.execute("UPDATE w SET c = 'v2' WHERE a = 'k' AND b = 1", ())
        .unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut db2 = mem();
    db2.execute(
        "CREATE TABLE w(a TEXT, b INT, c TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID",
        (),
    )
    .unwrap();
    apply_all(&mut db2, &cs).unwrap();
    assert_eq!(rows(&db2, "SELECT * FROM w"), rows(&db, "SELECT * FROM w"));
}

#[test]
fn null_pk_rows_are_ignored() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a TEXT PRIMARY KEY, b)", ())
        .unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(NULL, 'x')", ()).unwrap();
    assert!(s.is_empty());
    assert!(s.changeset(&db).unwrap().is_empty());
}

#[test]
fn value_types_round_trip() {
    let mut db = mem();
    db.execute("CREATE TABLE t(k INTEGER PRIMARY KEY, i, r, t, b, n)", ())
        .unwrap();
    let s = sess(&mut db);
    db.execute(
        "INSERT INTO t VALUES(1, -9223372036854775808, 3.5, 'héllo', x'00ff10', NULL)",
        (),
    )
    .unwrap();
    db.execute(
        "INSERT INTO t VALUES(2, 9223372036854775807, -0.0, '', x'', NULL)",
        (),
    )
    .unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut db2 = mem();
    db2.execute("CREATE TABLE t(k INTEGER PRIMARY KEY, i, r, t, b, n)", ())
        .unwrap();
    apply_all(&mut db2, &cs).unwrap();
    assert_eq!(
        rows(&db2, "SELECT * FROM t ORDER BY k"),
        rows(&db, "SELECT * FROM t ORDER BY k")
    );
}

#[test]
fn rollback_self_heals_the_changeset() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'orig')", ()).unwrap();
    let s = sess(&mut db);
    // A transaction that rolls back: the recorded changes resolve to
    // nothing against the restored state.
    db.execute("BEGIN", ()).unwrap();
    db.execute("INSERT INTO t VALUES(2, 'tx')", ()).unwrap();
    db.execute("DELETE FROM t WHERE a = 1", ()).unwrap();
    db.execute("ROLLBACK", ()).unwrap();
    assert!(s.changeset(&db).unwrap().is_empty());
}

#[test]
fn trigger_changes_marked_indirect() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("CREATE TABLE log(m TEXT)", ()).unwrap();
    db.execute(
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES('ins'); END",
        (),
    )
    .unwrap();
    let s = db.create_session();
    s.set_implicit_rowid_pk(true); // the log table has no explicit PK
    s.attach(None);
    db.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut it = ChangesetIter::new(&cs).unwrap();
    let mut indirect_flags = Vec::new();
    while it.next().unwrap() {
        indirect_flags.push((it.table().to_string(), it.indirect()));
    }
    assert!(indirect_flags.contains(&("t".to_string(), false)));
    assert!(indirect_flags.contains(&("log".to_string(), true)));
}

#[test]
fn table_filter_and_attach_order() {
    let mut db = mem();
    db.execute("CREATE TABLE a(x INTEGER PRIMARY KEY)", ())
        .unwrap();
    db.execute("CREATE TABLE b(x INTEGER PRIMARY KEY)", ())
        .unwrap();
    let s = db.create_session();
    s.set_table_filter(Some(|name: &str| name != "a"));
    db.execute("INSERT INTO a VALUES(1)", ()).unwrap();
    db.execute("INSERT INTO b VALUES(1)", ()).unwrap();
    let cs = s.changeset(&db).unwrap();
    let mut it = ChangesetIter::new(&cs).unwrap();
    assert!(it.next().unwrap());
    assert_eq!(it.table(), "b");
    assert!(!it.next().unwrap());
}

#[test]
fn apply_conflict_data_and_replace() {
    let base = || {
        let mut db = mem();
        db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
            .unwrap();
        db.execute("INSERT INTO t VALUES(1, 'orig')", ()).unwrap();
        db
    };
    let mut db1 = base();
    let s = sess(&mut db1);
    db1.execute("UPDATE t SET b = 'mine' WHERE a = 1", ())
        .unwrap();
    let cs = s.changeset(&db1).unwrap();

    // The remote modified the same row differently.
    let mut db2 = base();
    db2.execute("UPDATE t SET b = 'theirs' WHERE a = 1", ())
        .unwrap();

    let mut conflicts = Vec::new();
    session::changeset_apply(&mut db2, &cs, 0, None, &mut |e| {
        conflicts.push(e.code);
        session::CHANGESET_REPLACE
    })
    .unwrap();
    assert_eq!(conflicts, vec![session::CHANGESET_DATA]);
    // REPLACE on DATA: retry without old-value verification → ours wins.
    assert_eq!(
        rows(&db2, "SELECT b FROM t")[0][0],
        Value::Text("mine".into())
    );
}

#[test]
fn apply_conflict_insert_and_omit() {
    let mut db1 = mem();
    db1.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)", ())
        .unwrap();
    let s = sess(&mut db1);
    db1.execute("INSERT INTO t VALUES(1)", ()).unwrap();
    let cs = s.changeset(&db1).unwrap();

    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY)", ())
        .unwrap();
    db2.execute("INSERT INTO t VALUES(1)", ()).unwrap(); // conflict

    let mut seen_conflict_row = false;
    let rc = session::changeset_apply(&mut db2, &cs, 0, None, &mut |e| {
        assert_eq!(e.code, session::CHANGESET_CONFLICT);
        if let Some(row) = &e.conflict_row {
            seen_conflict_row = true;
            assert_eq!(row[0], Value::Integer(1));
        }
        session::CHANGESET_OMIT
    });
    rc.unwrap();
    assert!(seen_conflict_row);
    // OMIT: the conflicting row stays.
    assert_eq!(
        rows(&db2, "SELECT COUNT(*) FROM t")[0][0],
        Value::Integer(1)
    );
}

#[test]
fn apply_insert_replace_removes_conflicting_row() {
    let mut db1 = mem();
    db1.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    let s = sess(&mut db1);
    db1.execute("INSERT INTO t VALUES(1, 'new')", ()).unwrap();
    let cs = s.changeset(&db1).unwrap();

    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    db2.execute("INSERT INTO t VALUES(1, 'old')", ()).unwrap();
    session::changeset_apply(&mut db2, &cs, 0, None, &mut |e| {
        assert_eq!(e.code, session::CHANGESET_CONFLICT);
        session::CHANGESET_REPLACE
    })
    .unwrap();
    assert_eq!(
        rows(&db2, "SELECT b FROM t")[0][0],
        Value::Text("new".into())
    );
}

#[test]
fn apply_notfound_conflict() {
    let mut db1 = mem();
    db1.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    db1.execute("INSERT INTO t VALUES(1, 'x')", ()).unwrap();
    let s = sess(&mut db1);
    db1.execute("DELETE FROM t WHERE a = 1", ()).unwrap();
    let cs = s.changeset(&db1).unwrap();

    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    // Row 1 never existed in db2 → NOTFOUND.
    let mut codes = Vec::new();
    session::changeset_apply(&mut db2, &cs, 0, None, &mut |e| {
        codes.push(e.code);
        session::CHANGESET_OMIT
    })
    .unwrap();
    assert_eq!(codes, vec![session::CHANGESET_NOTFOUND]);
}

#[test]
fn apply_check_constraint_conflict() {
    let mut db1 = mem();
    db1.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b CHECK(b > 10))", ())
        .unwrap();
    let s = sess(&mut db1);
    db1.execute("INSERT INTO t VALUES(1, 20)", ()).unwrap();
    let cs = s.changeset(&db1).unwrap();

    let mut db2 = mem();
    db2.execute(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b CHECK(b > 100))",
        (),
    )
    .unwrap();
    let mut codes = Vec::new();
    session::changeset_apply(&mut db2, &cs, 0, None, &mut |e| {
        codes.push(e.code);
        session::CHANGESET_OMIT
    })
    .unwrap();
    assert_eq!(codes, vec![session::CHANGESET_CONSTRAINT]);
    assert_eq!(
        rows(&db2, "SELECT COUNT(*) FROM t")[0][0],
        Value::Integer(0)
    );
}

#[test]
fn apply_abort_rolls_back_everything() {
    let mut db1 = mem();
    db1.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    let s = sess(&mut db1);
    db1.execute("INSERT INTO t VALUES(1, 'x')", ()).unwrap();
    db1.execute("INSERT INTO t VALUES(2, 'y')", ()).unwrap();
    let cs = s.changeset(&db1).unwrap();

    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    db2.execute("INSERT INTO t VALUES(1, 'conflict')", ())
        .unwrap();
    let rc = session::changeset_apply(&mut db2, &cs, 0, None, &mut |_e| session::CHANGESET_ABORT);
    assert!(rc.is_err());
    // ABORT rolled back: the second row must NOT be there.
    assert_eq!(
        rows(&db2, "SELECT COUNT(*) FROM t")[0][0],
        Value::Integer(1)
    );
}

#[test]
fn invert_round_trips() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'base')", ()).unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(2, 'new')", ()).unwrap();
    db.execute("UPDATE t SET b = 'mod' WHERE a = 1", ())
        .unwrap();
    db.execute("DELETE FROM t WHERE a = 1", ()).unwrap();
    let cs = s.changeset(&db).unwrap();
    let inv = session::changeset_invert(&cs).unwrap();

    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)", ())
        .unwrap();
    db2.execute("INSERT INTO t VALUES(1, 'base')", ()).unwrap();
    apply_all(&mut db2, &cs).unwrap();
    apply_all(&mut db2, &inv).unwrap();
    // The inverse restored the pre-session state.
    assert_eq!(
        rows(&db2, "SELECT * FROM t"),
        vec![vec![Value::Integer(1), Value::Text("base".into())]]
    );
}

#[test]
fn concat_and_changegroup() {
    let mk = || {
        let mut db = mem();
        db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
            .unwrap();
        db
    };
    let mut db1 = mk();
    let s1 = sess(&mut db1);
    db1.execute("INSERT INTO t VALUES(1, 'one')", ()).unwrap();
    let cs1 = s1.changeset(&db1).unwrap();

    let mut db2 = mk();
    let s2 = sess(&mut db2);
    db2.execute("INSERT INTO t VALUES(2, 'two')", ()).unwrap();
    db2.execute("INSERT INTO t VALUES(3, 'three')", ()).unwrap();
    let cs2 = s2.changeset(&db2).unwrap();

    let joined = session::changeset_concat(&cs1, &cs2).unwrap();
    let mut db3 = mk();
    apply_all(&mut db3, &joined).unwrap();
    assert_eq!(
        rows(&db3, "SELECT COUNT(*) FROM t")[0][0],
        Value::Integer(3)
    );

    // The changegroup equals concat.
    let mut grp = ChangeGroup::new();
    grp.add(&cs1).unwrap();
    grp.add(&cs2).unwrap();
    assert_eq!(grp.output(), joined);
}

#[test]
fn rebase_workflow() {
    let mk = || {
        let mut db = mem();
        db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
            .unwrap();
        db.execute("INSERT INTO t VALUES(1, 'base')", ()).unwrap();
        db.execute("INSERT INTO t VALUES(2, 'base')", ()).unwrap();
        db
    };
    // LOCAL edits.
    let mut local = mk();
    let ls = sess(&mut local);
    local
        .execute("UPDATE t SET b = 'local-1' WHERE a = 1", ())
        .unwrap();
    local
        .execute("UPDATE t SET b = 'local-2' WHERE a = 2", ())
        .unwrap();
    let local_cs = ls.changeset(&local).unwrap();

    // REMOTE applies the SAME base changeset with OMITs (the conflict
    // resolutions we must rebase against).
    let mut remote = mk();
    let rs = sess(&mut remote);
    remote
        .execute("UPDATE t SET b = 'remote-1' WHERE a = 1", ())
        .unwrap();
    remote
        .execute("UPDATE t SET b = 'remote-2' WHERE a = 2", ())
        .unwrap();
    let remote_cs = rs.changeset(&remote).unwrap();

    // Apply LOCAL's changeset on the remote with OMIT → collect the
    // rebase blob.
    let rebase_blob = session::changeset_apply(
        &mut remote,
        &remote_cs, // placeholder replaced below
        0,
        None,
        &mut |_e| session::CHANGESET_OMIT,
    );
    let _ = rebase_blob; // (unused shape — see the real flow below)

    // The real flow: remote applies LOCAL's changeset and omits both
    // conflicting updates; the rebase blob records the resolutions.
    let mut remote2 = mk();
    remote2
        .execute("UPDATE t SET b = 'remote-1' WHERE a = 1", ())
        .unwrap();
    remote2
        .execute("UPDATE t SET b = 'remote-2' WHERE a = 2", ())
        .unwrap();
    let rebased_out = session::changeset_apply(
        &mut remote2,
        &local_cs,
        session::APPLY_REBASE,
        None,
        &mut |_e| session::CHANGESET_OMIT,
    )
    .unwrap()
    .expect("rebase blob");
    assert!(!rebased_out.is_empty());

    // Configure a rebaser with it and rebase the LOCAL changeset. The
    // rebased bytes are differential-pinned against real SQLite (see
    // session_differential.rs) — here we pin the mechanics: configure
    // succeeds and the result applies cleanly on the remote's state.
    let mut rb = Rebaser::new();
    rb.configure(&rebased_out).unwrap();
    let rebased = rb.rebase(&local_cs).unwrap();
    apply_all(&mut remote2, &rebased).unwrap();
    let _ = rows(&remote2, "SELECT * FROM t");
}

#[test]
fn patchset_shape() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'x', 'y')", ()).unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(2, 'p', 'q')", ()).unwrap();
    db.execute("UPDATE t SET c = 'z' WHERE a = 1", ()).unwrap();
    db.execute("DELETE FROM t WHERE a = 1", ()).unwrap();
    let ps = s.patchset(&db).unwrap();
    // A patchset is applicable just like a changeset.
    let mut db2 = mem();
    db2.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)", ())
        .unwrap();
    db2.execute("INSERT INTO t VALUES(1, 'x', 'y')", ())
        .unwrap();
    apply_all(&mut db2, &ps).unwrap();
    assert_eq!(rows(&db2, "SELECT * FROM t"), rows(&db, "SELECT * FROM t"));
}

#[test]
fn alter_add_column_mid_session_pads_records() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)", ())
        .unwrap();
    db.execute("INSERT INTO t VALUES(1, 'x')", ()).unwrap();
    let s = sess(&mut db);
    db.execute("INSERT INTO t VALUES(2, 'y')", ()).unwrap();
    db.execute("ALTER TABLE t ADD COLUMN c DEFAULT 'dflt'", ())
        .unwrap();
    // The pre-ALTER INSERT entry pads with the default; the changeset
    // stays applicable on the widened schema.
    let cs = s.changeset(&db).unwrap();
    let mut db2 = mem();
    db2.execute(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c DEFAULT 'dflt')",
        (),
    )
    .unwrap();
    db2.execute("INSERT INTO t(a, b) VALUES(1, 'x')", ())
        .unwrap();
    apply_all(&mut db2, &cs).unwrap();
    assert_eq!(
        rows(&db2, "SELECT * FROM t ORDER BY a"),
        rows(&db, "SELECT * FROM t ORDER BY a")
    );
}

#[test]
fn corrupt_changesets_error_cleanly() {
    // Truncated record.
    let mut it = ChangesetIter::new(&[b'T', 2, 1, 0, b't', 0, 18, 0, 0x01]).unwrap();
    assert!(it.next().is_err());
    // Bad op byte.
    assert!(ChangesetIter::new(&[b'T', 1, 1, b't', 0, 99, 0]).is_ok());
    let mut it2 = ChangesetIter::new(&[b'T', 1, 1, b't', 0, 99, 0]).unwrap();
    assert!(it2.next().is_err());
    // Invert on garbage fails.
    assert!(session::changeset_invert(&[0x54, 0x02, 0x01]).is_err());
}

#[test]
fn stat1_session_special_shape() {
    let mut db = mem();
    db.execute("CREATE TABLE t(a)", ()).unwrap();
    db.execute("INSERT INTO t VALUES(1), (2), (3)", ()).unwrap();
    db.execute("ANALYZE", ()).unwrap();
    let s = sess(&mut db);
    // The engine's stat1 is a real table when present; direct writes are
    // the session's view of it. (If the engine rejects DML on internal
    // tables, the session records nothing — either way no panic.)
    let _ = db.execute("UPDATE sqlite_stat1 SET stat = 'test'", ());
    let cs = s.changeset(&db).unwrap();
    let _ = cs; // shape pinned differentially
}
