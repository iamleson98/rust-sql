//! Session-extension DIFFERENTIAL tests — every byte and every conflict
//! pinned against real SQLite (the bundled oracle, built with
//! SQLITE_ENABLE_SESSION through the vendored `libsqlite3-sys`).
//!
//! The law under test: for identical schemas, statements and session
//! configurations, rustqlite's session extension produces BYTE-IDENTICAL
//! changesets / patchsets / inverts / concats / rebases, fires the SAME
//! conflict codes in the same order, and cross-applies in both
//! directions with identical final states.
//!
//! The oracle side drives the raw C API (`libsqlite3_sys`) directly —
//! the same surface this engine's `compat` layer exports.

use std::ffi::c_void;
use std::os::raw::c_int;

use libsqlite3_sys as sys;
use rustqlite::session as eng;
use rustqlite::{Database, Value};

const OK: c_int = sys::SQLITE_OK as c_int;
const OMIT: c_int = sys::SQLITE_CHANGESET_OMIT as c_int;

fn oracle() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().unwrap()
}

/// Run the same script on both engines.
fn run_both(script: &[&str]) -> (Database, rusqlite::Connection) {
    let mut db = Database::open_in_memory().unwrap();
    let conn = oracle();
    for sql in script {
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    (db, conn)
}

// ---- Oracle-side session handle (the raw C API) ------------------------

/// Borrows the oracle connection (the caller keeps it alive; the
/// session is deleted before the connection in every test's scope).
struct OracleSession {
    p: *mut sys::sqlite3_session,
}

impl OracleSession {
    fn new(conn: &rusqlite::Connection) -> Self {
        let mut p: *mut sys::sqlite3_session = std::ptr::null_mut();
        let rc =
            unsafe { sys::sqlite3session_create(conn.handle(), b"main\0".as_ptr().cast(), &mut p) };
        assert_eq!(rc, OK, "sqlite3session_create: {rc}");
        OracleSession { p }
    }
    fn attach_all(&self) {
        let rc = unsafe { sys::sqlite3session_attach(self.p, std::ptr::null()) };
        assert_eq!(rc, OK);
    }
    fn set_indirect(&self, on: bool) {
        unsafe { sys::sqlite3session_indirect(self.p, on as c_int) };
    }
    fn changeset(&self) -> Vec<u8> {
        let mut n: c_int = 0;
        let mut p: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { sys::sqlite3session_changeset(self.p, &mut n, &mut p) };
        assert_eq!(rc, OK);
        take_buf(p, n)
    }
    fn patchset(&self) -> Vec<u8> {
        let mut n: c_int = 0;
        let mut p: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { sys::sqlite3session_patchset(self.p, &mut n, &mut p) };
        assert_eq!(rc, OK);
        take_buf(p, n)
    }
}

impl Drop for OracleSession {
    fn drop(&mut self) {
        unsafe { sys::sqlite3session_delete(self.p) };
    }
}

fn take_buf(p: *mut c_void, n: c_int) -> Vec<u8> {
    if p.is_null() || n == 0 {
        unsafe { sys::sqlite3_free(p) };
        return Vec::new();
    }
    let v = unsafe { std::slice::from_raw_parts(p as *const u8, n as usize).to_vec() };
    unsafe { sys::sqlite3_free(p) };
    v
}

// ---- Byte-comparison harness -------------------------------------------

fn compare_changesets(pre: &[&str], workload: &[&str]) -> Vec<u8> {
    let (mut db, conn) = run_both(pre);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();
    for sql in workload {
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(
        ours, theirs,
        "changeset bytes differ\n  pre: {pre:?}\n  workload: {workload:?}\n  ours:   {ours:?}\n  theirs: {theirs:?}"
    );
    ours
}

fn compare_patchsets(pre: &[&str], workload: &[&str]) {
    let (mut db, conn) = run_both(pre);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();
    for sql in workload {
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let ours = s.patchset(&db).unwrap();
    let theirs = os.patchset();
    assert_eq!(
        ours, theirs,
        "patchset bytes differ\n  workload: {workload:?}"
    );
}

// ---- byte-identical changesets ----------------------------------------

#[test]
fn diff_basic_insert_update_delete() {
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "INSERT INTO t VALUES(2, 'y')",
            "UPDATE t SET b = 'X' WHERE a = 1",
            "DELETE FROM t WHERE a = 2",
        ],
    );
}

#[test]
fn diff_multi_table_attach_order() {
    compare_changesets(
        &[
            "CREATE TABLE zz(a INTEGER PRIMARY KEY)",
            "CREATE TABLE aa(a INTEGER PRIMARY KEY)",
        ],
        &[
            "INSERT INTO zz VALUES(1)",
            "INSERT INTO aa VALUES(1)",
            "INSERT INTO zz VALUES(2)",
        ],
    );
}

#[test]
fn diff_value_types() {
    compare_changesets(
        &["CREATE TABLE t(k INTEGER PRIMARY KEY, i, r, t, b, n)"],
        &[
            "INSERT INTO t VALUES(1, -9223372036854775808, 3.5, 'héllo', x'00ff10', NULL)",
            "INSERT INTO t VALUES(2, 9223372036854775807, -0.0, '', x'', NULL)",
            "INSERT INTO t VALUES(3, 0, 1e308, 'a''b', x'deadbeef', 'txt')",
        ],
    );
}

#[test]
fn diff_no_pk_table_ignored_by_default() {
    // SQLite's default: tables with no explicit PRIMARY KEY are simply
    // ignored by the sessions module.
    compare_changesets(
        &["CREATE TABLE t(a, b TEXT)"],
        &[
            "INSERT INTO t(rowid, a, b) VALUES(5, 1, 'x')",
            "INSERT INTO t(rowid, a, b) VALUES(-7, 2, 'y')",
            "UPDATE t SET b = 'z' WHERE rowid = 5",
            "DELETE FROM t WHERE rowid = -7",
        ],
    );
}

#[test]
fn diff_rowid_table_with_objconfig_rowid_opt_in() {
    // OBJCONFIG_ROWID: the synthetic `_rowid_ INTEGER PRIMARY KEY`
    // column — both sides opt in, the changesets must be byte-equal.
    let pre = &["CREATE TABLE t(a, b TEXT)"];
    let workload = &[
        "INSERT INTO t(rowid, a, b) VALUES(5, 1, 'x')",
        "INSERT INTO t(rowid, a, b) VALUES(-7, 2, 'y')",
        "UPDATE t SET b = 'z' WHERE rowid = 5",
        "DELETE FROM t WHERE rowid = -7",
    ];
    let (mut db, conn) = run_both(pre);
    let s = db.create_session();
    s.set_implicit_rowid_pk(true);
    s.attach(None);
    let os = OracleSession::new(&conn);
    unsafe {
        let mut arg: c_int = 1;
        assert_eq!(
            sys::sqlite3session_object_config(os.p, 2, &mut arg as *mut c_int as *mut c_void),
            OK
        );
        assert_eq!(arg, 1);
    }
    os.attach_all();
    for sql in workload {
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(
        ours, theirs,
        "OBJCONFIG_ROWID changesets differ
 ours: {ours:?}\n theirs: {theirs:?}"
    );
}

#[test]
fn diff_declared_pk_text_and_composite() {
    compare_changesets(
        &[
            "CREATE TABLE p(k TEXT PRIMARY KEY, v)",
            "CREATE TABLE c(a, b, v, PRIMARY KEY(b, a))",
        ],
        &[
            "INSERT INTO p VALUES('key1', 'v1')",
            "INSERT INTO p VALUES('key2', 'v2')",
            "UPDATE p SET v = 'V' WHERE k = 'key1'",
            "INSERT INTO c VALUES(1, 'x', 'one')",
            "INSERT INTO c VALUES(2, 'y', 'two')",
            "UPDATE c SET v = 'ONE' WHERE a = 1 AND b = 'x'",
            "DELETE FROM c WHERE a = 2 AND b = 'y'",
        ],
    );
}

#[test]
fn diff_without_rowid() {
    compare_changesets(
        &["CREATE TABLE w(a TEXT, b INT, c TEXT, PRIMARY KEY(a, b)) WITHOUT ROWID"],
        &[
            "INSERT INTO w VALUES('k', 1, 'v')",
            "INSERT INTO w VALUES('k', 2, 'v2')",
            "UPDATE w SET c = 'v1b' WHERE a = 'k' AND b = 1",
            "DELETE FROM w WHERE a = 'k' AND b = 2",
        ],
    );
}

#[test]
fn diff_coalescing_shapes() {
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "UPDATE t SET b = 'final' WHERE a = 1",
        ],
    );
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &["INSERT INTO t VALUES(1, 'x')", "DELETE FROM t WHERE a = 1"],
    );
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c)"],
        &[
            "INSERT INTO t VALUES(1, 'x', 1)",
            "UPDATE t SET b = 'y' WHERE a = 1",
            "UPDATE t SET c = 2, b = 'y' WHERE a = 1",
        ],
    );
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "DELETE FROM t WHERE a = 1",
            "INSERT INTO t VALUES(1, 'new')",
        ],
    );
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "UPDATE t SET b = 'tmp' WHERE a = 1",
            "UPDATE t SET b = 'orig' WHERE a = 1",
        ],
    );
    // Same-row INSERT + DELETE + re-INSERT of a DIFFERENT value.
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'v1')",
            "DELETE FROM t WHERE a = 1",
            "INSERT INTO t VALUES(1, 'v2')",
            "UPDATE t SET b = 'v3' WHERE a = 1",
        ],
    );
}

#[test]
fn diff_pk_moving_update() {
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "UPDATE t SET a = 5 WHERE a = 1",
        ],
    );
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "UPDATE t SET a = 5 WHERE a = 1",
            "UPDATE t SET a = 1 WHERE a = 5",
        ],
    );
}

#[test]
fn diff_rowid_move_through_alias() {
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "UPDATE t SET a = 9 WHERE a = 1",
            "UPDATE t SET b = 'y' WHERE a = 9",
        ],
    );
}

#[test]
fn diff_triggers_indirect() {
    compare_changesets(
        &[
            "CREATE TABLE t(a INTEGER PRIMARY KEY)",
            "CREATE TABLE log(m TEXT)",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES('ins'); END",
        ],
        &["INSERT INTO t VALUES(1)", "INSERT INTO t VALUES(2)"],
    );
}

#[test]
fn diff_transaction_rollback_self_heals() {
    compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "BEGIN",
            "INSERT INTO t VALUES(2, 'tx')",
            "DELETE FROM t WHERE a = 1",
            "ROLLBACK",
        ],
    );
}

#[test]
fn diff_indirect_flag() {
    let (mut db, conn) = run_both(&["CREATE TABLE t(a INTEGER PRIMARY KEY)"]);
    let s = db.create_session();
    s.attach(None);
    s.set_indirect(true);
    let os = OracleSession::new(&conn);
    os.attach_all();
    os.set_indirect(true);
    {
        let sql = "INSERT INTO t VALUES(1)";
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    s.set_indirect(false);
    os.set_indirect(false);
    {
        let sql = "INSERT INTO t VALUES(2)";
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(ours, theirs);
}

#[test]
fn diff_many_rows_bucket_growth() {
    // 600 distinct rows: the change hash grows 256→512→1024 — the
    // re-hash order must match SQLite's byte-for-byte.
    let (mut db, conn) = run_both(&["CREATE TABLE t(a INTEGER PRIMARY KEY, b)"]);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();
    for i in 0..600 {
        let sql = format!("INSERT INTO t VALUES({i}, 'v{i}')");
        db.execute(&sql, ()).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    for i in (0..600).step_by(3) {
        let sql = format!("UPDATE t SET b = 'u{i}' WHERE a = {i}");
        db.execute(&sql, ()).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(ours, theirs, "bucket-growth ordering diverged");
}

#[test]
fn diff_hash_distribution_orders() {
    let (mut db, conn) = run_both(&["CREATE TABLE t(k PRIMARY KEY, v)"]);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();
    let vals = [
        "1",
        "-1",
        "9223372036854775807",
        "-9223372036854775808",
        "'a'",
        "'zz'",
        "x'00'",
        "x'ff00ab'",
        "2.5",
        "-2.5",
        "''",
    ];
    for (i, v) in vals.iter().enumerate() {
        let sql = format!("INSERT INTO t VALUES({v}, {i})");
        db.execute(&sql, ()).unwrap();
        conn.execute_batch(&sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(ours, theirs);
}

#[test]
fn diff_patchsets() {
    compare_patchsets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)"],
        &[
            "INSERT INTO t VALUES(1, 'x', 'y')",
            "INSERT INTO t VALUES(2, 'p', 'q')",
            "UPDATE t SET c = 'z' WHERE a = 1",
            "DELETE FROM t WHERE a = 1",
        ],
    );
}

// ---- invert / concat ----------------------------------------------------

fn oracle_invert(cs: &[u8]) -> Vec<u8> {
    let mut n: c_int = 0;
    let mut p: *mut c_void = std::ptr::null_mut();
    let rc = unsafe {
        sys::sqlite3changeset_invert(
            cs.len() as c_int,
            cs.as_ptr() as *mut c_void,
            &mut n,
            &mut p,
        )
    };
    assert_eq!(rc, OK);
    take_buf(p, n)
}

fn oracle_concat(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut n: c_int = 0;
    let mut p: *mut c_void = std::ptr::null_mut();
    let rc = unsafe {
        sys::sqlite3changeset_concat(
            a.len() as c_int,
            a.as_ptr() as *mut c_void,
            b.len() as c_int,
            b.as_ptr() as *mut c_void,
            &mut n,
            &mut p,
        )
    };
    assert_eq!(rc, OK);
    take_buf(p, n)
}

#[test]
fn diff_invert() {
    let cs = compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)"],
        &[
            "INSERT INTO t VALUES(1, 'x')",
            "UPDATE t SET b = 'mod' WHERE a = 1",
            "DELETE FROM t WHERE a = 1",
        ],
    );
    let ours = eng::changeset_invert(&cs).unwrap();
    let theirs = oracle_invert(&cs);
    assert_eq!(ours, theirs);
}

#[test]
fn diff_concat() {
    let a = compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b)"],
        &["INSERT INTO t VALUES(1, 'one')"],
    );
    let b = compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b)"],
        &[
            "INSERT INTO t VALUES(2, 'two')",
            "INSERT INTO t VALUES(3, 'three')",
        ],
    );
    let ours = eng::changeset_concat(&a, &b).unwrap();
    let theirs = oracle_concat(&a, &b);
    assert_eq!(ours, theirs);
}

#[test]
fn diff_concat_merges_same_row() {
    let a = compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)"],
        &["UPDATE t SET b = 'A1', c = 'c1' WHERE a = 1"],
    );
    let b = compare_changesets(
        &["CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)"],
        &["UPDATE t SET c = 'C2' WHERE a = 1"],
    );
    let ours = eng::changeset_concat(&a, &b).unwrap();
    let theirs = oracle_concat(&a, &b);
    assert_eq!(ours, theirs);
}

// ---- apply differentials ----------------------------------------------

fn dump(db: &Database, table: &str) -> Vec<Vec<Value>> {
    // Both engines order identically over the first two columns (the
    // tables under test carry distinct (a, b) prefixes).
    db.query(&format!("SELECT * FROM {table} ORDER BY 1, 2"), ())
        .unwrap()
}

fn dump_oracle(conn: &rusqlite::Connection, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))
        .unwrap();
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().unwrap() {
        let mut row = Vec::new();
        for i in 0..n {
            let v: rusqlite::types::Value = r.get(i).unwrap();
            row.push(match v {
                rusqlite::types::Value::Null => Value::Null,
                rusqlite::types::Value::Integer(i) => Value::Integer(i),
                rusqlite::types::Value::Real(f) => Value::Real(f),
                rusqlite::types::Value::Text(s) => Value::Text(s.into()),
                rusqlite::types::Value::Blob(b) => Value::Blob(b),
            });
        }
        out.push(row);
    }
    out
}

extern "C" fn omit_conflict(
    _ctx: *mut c_void,
    _code: c_int,
    _iter: *mut sys::sqlite3_changeset_iter,
) -> c_int {
    OMIT
}

fn oracle_apply_omit(conn: &rusqlite::Connection, cs: &[u8]) {
    let rc = unsafe {
        sys::sqlite3changeset_apply(
            conn.handle(),
            cs.len() as c_int,
            cs.as_ptr() as *mut c_void,
            None,
            Some(omit_conflict),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, OK, "oracle apply failed: {rc}");
}

fn ours_apply_omit(db: &mut Database, cs: &[u8]) {
    eng::changeset_apply(db, cs, 0, None, &mut |_e| eng::CHANGESET_OMIT)
        .map(|_| ())
        .unwrap();
}

#[test]
fn diff_cross_apply_both_directions() {
    let pre = &[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
        "INSERT INTO t VALUES(1, 'base1')",
        "INSERT INTO t VALUES(2, 'base2')",
    ];
    let (mut db, conn) = run_both(pre);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();
    let workload = &[
        "INSERT INTO t VALUES(3, 'new')",
        "UPDATE t SET b = 'mod1' WHERE a = 1",
        "DELETE FROM t WHERE a = 2",
    ];
    for sql in workload {
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(ours, theirs);

    // OUR changeset applied BY REAL SQLITE.
    let o2 = {
        let c = oracle();
        c.execute_batch(pre.join(";").as_str()).unwrap();
        c
    };
    oracle_apply_omit(&o2, &ours);
    assert_eq!(
        dump(&db, "t"),
        dump_oracle(&o2, "t"),
        "oracle apply of our changeset"
    );

    // SQLITE's changeset applied BY OUR ENGINE.
    let mut d2 = Database::open_in_memory().unwrap();
    for sql in pre {
        d2.execute(sql, ()).unwrap();
    }
    ours_apply_omit(&mut d2, &theirs);
    assert_eq!(
        dump(&db, "t"),
        dump(&d2, "t"),
        "our apply of sqlite's changeset"
    );
}

#[test]
fn diff_apply_conflicts_data_notfound_constraint() {
    let pre = &[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c CHECK(c < 100))",
        "INSERT INTO t VALUES(1, 'base', 1)",
        "INSERT INTO t VALUES(2, 'gone', 2)",
    ];
    let (mut db, conn) = run_both(pre);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();
    let workload = &[
        "UPDATE t SET b = 'local1' WHERE a = 1",
        "DELETE FROM t WHERE a = 2",
    ];
    for sql in workload {
        db.execute(sql, ()).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let ours = s.changeset(&db).unwrap();
    let theirs = os.changeset();
    assert_eq!(ours, theirs);

    // Diverged targets: row 1 changed remotely (DATA), row 2 gone
    // (NOTFOUND).
    let div = &[
        "UPDATE t SET b = 'remote1' WHERE a = 1",
        "DELETE FROM t WHERE a = 2",
    ];
    let mk = |div: &[&str]| {
        let mut db = Database::open_in_memory().unwrap();
        let conn = oracle();
        for sql in pre.iter().chain(div.iter()) {
            db.execute(sql, ()).unwrap();
            conn.execute_batch(sql).unwrap();
        }
        (db, conn)
    };
    let (mut d2, o2) = mk(div);
    let mut our_codes: Vec<i32> = Vec::new();
    eng::changeset_apply(&mut d2, &ours, 0, None, &mut |e| {
        our_codes.push(e.code);
        eng::CHANGESET_OMIT
    })
    .map(|_| ())
    .unwrap();

    let mut oracle_codes: Vec<i32> = Vec::new();
    unsafe {
        extern "C" fn record(
            ctx: *mut c_void,
            code: c_int,
            _iter: *mut sys::sqlite3_changeset_iter,
        ) -> c_int {
            let codes = unsafe { &mut *(ctx as *mut Vec<i32>) };
            codes.push(code);
            OMIT
        }
        let rc = sys::sqlite3changeset_apply(
            o2.handle(),
            ours.len() as c_int,
            ours.as_ptr() as *mut c_void,
            None,
            Some(record),
            &mut oracle_codes as *mut Vec<i32> as *mut c_void,
        );
        assert_eq!(rc, OK);
    }
    assert_eq!(our_codes, oracle_codes, "conflict codes diverged");
    assert_eq!(
        dump(&d2, "t"),
        dump_oracle(&o2, "t"),
        "post-conflict states diverged"
    );
}

#[test]
fn diff_apply_rebase_blob_and_rebased_bytes() {
    let pre = &[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b)",
        "INSERT INTO t VALUES(1, 'base')",
        "INSERT INTO t VALUES(2, 'base')",
    ];
    // LOCAL changeset.
    let (mut local, lconn) = run_both(pre);
    let ls = local.create_session();
    ls.attach(None);
    let los = OracleSession::new(&lconn);
    los.attach_all();
    let local_wl = &[
        "UPDATE t SET b = 'local-1' WHERE a = 1",
        "UPDATE t SET b = 'local-2' WHERE a = 2",
    ];
    for sql in local_wl {
        local.execute(sql, ()).unwrap();
        lconn.execute_batch(sql).unwrap();
    }
    let local_cs = ls.changeset(&local).unwrap();
    assert_eq!(local_cs, los.changeset());

    // Diverged remote states.
    let div = &[
        "UPDATE t SET b = 'remote-1' WHERE a = 1",
        "UPDATE t SET b = 'remote-2' WHERE a = 2",
    ];
    let mk = || {
        let mut db = Database::open_in_memory().unwrap();
        let conn = oracle();
        for sql in pre.iter().chain(div.iter()) {
            db.execute(sql, ()).unwrap();
            conn.execute_batch(sql).unwrap();
        }
        (db, conn)
    };

    // OUR apply_v2 with OMIT produces a rebase blob.
    let (mut rdb, _) = mk();
    let our_rebase =
        eng::changeset_apply(&mut rdb, &local_cs, eng::APPLY_REBASE, None, &mut |_e| {
            eng::CHANGESET_OMIT
        })
        .unwrap()
        .unwrap();

    // ORACLE apply_v2 with OMIT produces its rebase blob.
    let (_, rconn) = mk();
    let oracle_rebase: Vec<u8> = unsafe {
        let mut pp: *mut c_void = std::ptr::null_mut();
        let mut pn: c_int = 0;
        let rc = sys::sqlite3changeset_apply_v2(
            rconn.handle(),
            local_cs.len() as c_int,
            local_cs.as_ptr() as *mut c_void,
            None,
            Some(omit_conflict),
            std::ptr::null_mut(),
            &mut pp,
            &mut pn,
            0,
        );
        assert_eq!(rc, OK);
        take_buf(pp, pn)
    };
    assert_eq!(
        our_rebase, oracle_rebase,
        "rebase blobs differ\n ours: {our_rebase:?}\n theirs: {oracle_rebase:?}"
    );

    // Rebasing the LOCAL changeset: ours vs the oracle's.
    let mut rb = eng::Rebaser::new();
    rb.configure(&our_rebase).unwrap();
    let our_rebased = rb.rebase(&local_cs).unwrap();

    let oracle_rebased: Vec<u8> = unsafe {
        let mut pr: *mut sys::sqlite3_rebaser = std::ptr::null_mut();
        assert_eq!(sys::sqlite3rebaser_create(&mut pr), OK);
        assert_eq!(
            sys::sqlite3rebaser_configure(
                pr,
                oracle_rebase.len() as c_int,
                oracle_rebase.as_ptr() as *const c_void
            ),
            OK
        );
        let mut out: *mut c_void = std::ptr::null_mut();
        let mut nout: c_int = 0;
        let rc = sys::sqlite3rebaser_rebase(
            pr,
            local_cs.len() as c_int,
            local_cs.as_ptr() as *const c_void,
            &mut nout,
            &mut out,
        );
        assert_eq!(rc, OK);
        let v = take_buf(out, nout);
        sys::sqlite3rebaser_delete(pr);
        v
    };
    assert_eq!(
        our_rebased, oracle_rebased,
        "rebased changesets differ\n ours: {our_rebased:?}\n theirs: {oracle_rebased:?}"
    );
}

// ---- randomized differential -------------------------------------------

#[test]
fn diff_randomized_workload_rounds() {
    let mut seed = 0x5eed_cafe_u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 33
    };
    let (mut db, conn) = run_both(&[
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c)",
        "CREATE TABLE u(k TEXT PRIMARY KEY, v)",
        "INSERT INTO t VALUES(1, 'x', 1.5), (2, 'y', 2.5), (3, 'z', 3.5)",
        "INSERT INTO u VALUES('k1', 1), ('k2', 2)",
    ]);
    let s = db.create_session();
    s.attach(None);
    let os = OracleSession::new(&conn);
    os.attach_all();

    for round in 0..40 {
        let stmts: Vec<String> = (0..5)
            .map(|_| {
                let r = next() % 10;
                match r {
                    0 => format!(
                        "INSERT INTO t(a, b, c) VALUES({}, 'r{round}', {}.5)",
                        (next() % 8) + 1,
                        next() % 5
                    ),
                    1 => format!("DELETE FROM t WHERE a = {}", (next() % 8) + 1),
                    2 => format!("UPDATE t SET b = 'u{round}' WHERE a = {}", (next() % 8) + 1),
                    3 => format!(
                        "UPDATE t SET c = {} WHERE a = {}",
                        next() % 7,
                        (next() % 8) + 1
                    ),
                    4 => format!("INSERT INTO u VALUES('k{}', {})", next() % 4, next() % 9),
                    5 => format!("DELETE FROM u WHERE k = 'k{}'", next() % 4),
                    6 => format!(
                        "UPDATE u SET v = {} WHERE k = 'k{}'",
                        next() % 9,
                        next() % 4
                    ),
                    7 => "INSERT INTO t(b, c) VALUES('auto', 9.5)".to_string(),
                    8 => format!(
                        "INSERT OR REPLACE INTO t(a, b, c) VALUES({}, 'rep', 0.5)",
                        (next() % 8) + 1
                    ),
                    _ => format!(
                        "UPDATE OR IGNORE t SET a = a + 10 WHERE a = {}",
                        (next() % 8) + 1
                    ),
                }
            })
            .collect();
        for sql in &stmts {
            let _ = db.execute(sql, ());
            let _ = conn.execute_batch(sql);
        }
        let ours = s.changeset(&db).unwrap();
        let theirs = os.changeset();
        assert_eq!(
            ours, theirs,
            "round {round}: changeset diverged\n stmts: {stmts:?}\n ours: {ours:?}\n theirs: {theirs:?}"
        );
    }
}
