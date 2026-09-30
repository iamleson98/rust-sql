//! Session-extension C-ABI smoke tests — the `sqlite3session_*` /
//! `sqlite3changeset_*` / `sqlite3changegroup_*` / `sqlite3rebaser_*`
//! surface through the compat layer (the same symbols stock SQLite
//! programs link). Byte-format parity is differentially pinned on the
//! engine side (tests/session_differential.rs); these tests pin the ABI
//! plumbing: handles, buffers, value objects, flags.

#![allow(clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};

extern crate sqlite3 as compat;
use compat::*;

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

extern "C" fn omit(_ctx: *mut c_void, _code: c_int, _iter: *mut sqlite3_changeset_iter) -> c_int {
    SQLITE_CHANGESET_OMIT
}

unsafe fn open_db() -> *mut sqlite3 {
    let name = cstr(":memory:");
    let mut db: *mut sqlite3 = std::ptr::null_mut();
    assert_eq!(
        sqlite3_open(name.as_ptr(), &mut db),
        SQLITE_OK as c_int,
        "open failed"
    );
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let c = cstr(sql);
    let mut err: *mut c_char = std::ptr::null_mut();
    let rc = sqlite3_exec(db, c.as_ptr(), None, std::ptr::null_mut(), &mut err);
    if !err.is_null() {
        let msg = std::ffi::CStr::from_ptr(err).to_string_lossy();
        sqlite3_free(err as *mut c_void);
        panic!("exec {sql} failed: {msg}");
    }
    assert_eq!(rc, SQLITE_OK as c_int, "exec {sql}");
}

#[test]
fn session_capi_capture_and_iterate() {
    unsafe {
        let db = open_db();
        exec(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
        exec(db, "INSERT INTO t VALUES(1, 'base')");

        let mut sess: *mut sqlite3_session = std::ptr::null_mut();
        assert_eq!(
            sqlite3session_create(db, cstr("main").as_ptr(), &mut sess),
            SQLITE_OK as c_int
        );
        // object_config: query + set ROWID tracking (a no-PK table later).
        let mut arg: c_int = -1;
        assert_eq!(
            sqlite3session_object_config(sess, 2, &mut arg),
            SQLITE_OK as c_int
        );
        assert_eq!(arg, 0); // default OFF
        arg = 1;
        assert_eq!(
            sqlite3session_object_config(sess, 2, &mut arg),
            SQLITE_OK as c_int
        );
        assert_eq!(arg, 1);
        assert_eq!(
            sqlite3session_attach(sess, std::ptr::null()),
            SQLITE_OK as c_int
        );
        exec(db, "INSERT INTO t VALUES(2, 'new')");
        exec(db, "UPDATE t SET b = 'mod' WHERE a = 1");
        assert_eq!(sqlite3session_isempty(sess), 0);

        let mut n: c_int = 0;
        let mut buf: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sqlite3session_changeset(sess, &mut n, &mut buf),
            SQLITE_OK as c_int
        );
        assert!(n > 0 && !buf.is_null());
        let cs = std::slice::from_raw_parts(buf as *const u8, n as usize).to_vec();

        // Walk it through the C iterator.
        let mut it: *mut sqlite3_changeset_iter = std::ptr::null_mut();
        assert_eq!(sqlite3changeset_start(&mut it, n, buf), SQLITE_OK as c_int);
        let mut seen = 0;
        loop {
            let rc = sqlite3changeset_next(it);
            assert!(rc == SQLITE_ROW as c_int || rc == SQLITE_DONE as c_int);
            if rc != SQLITE_ROW as c_int {
                break;
            }
            seen += 1;
            let mut tab: *const c_char = std::ptr::null_mut();
            let mut ncol: c_int = 0;
            let mut op: c_int = 0;
            let mut indirect: c_int = 0;
            assert_eq!(
                sqlite3changeset_op(it, &mut tab, &mut ncol, &mut op, &mut indirect),
                SQLITE_OK as c_int
            );
            assert_eq!(std::ffi::CStr::from_ptr(tab).to_bytes(), b"t");
            assert_eq!(ncol, 2);
            let mut pk: *mut u8 = std::ptr::null_mut();
            let mut pn: c_int = 0;
            assert_eq!(
                sqlite3changeset_pk(it, &mut pk, &mut pn),
                SQLITE_OK as c_int
            );
            assert_eq!(pn, 2);
            assert_eq!(*pk, 1);
            // old value of an UPDATE / new of an INSERT.
            let mut val: *mut sqlite3_value = std::ptr::null_mut();
            assert_eq!(sqlite3changeset_old(it, 0, &mut val), SQLITE_OK as c_int);
            assert_eq!(sqlite3_value_int64(val), if op as u8 == 18 { 0 } else { 1 });
        }
        assert_eq!(seen, 2, "INSERT + UPDATE");
        assert_eq!(sqlite3changeset_finalize(it), SQLITE_OK as c_int);
        sqlite3_free(buf);

        sqlite3session_delete(sess);
        sqlite3_close(db);
        let _ = cs;
    }
}

#[test]
fn session_capi_apply_on_second_connection() {
    unsafe {
        let db1 = open_db();
        exec(db1, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
        exec(db1, "INSERT INTO t VALUES(1, 'base')");
        let mut sess: *mut sqlite3_session = std::ptr::null_mut();
        assert_eq!(
            sqlite3session_create(db1, cstr("main").as_ptr(), &mut sess),
            SQLITE_OK as c_int
        );
        sqlite3session_attach(sess, std::ptr::null());
        exec(db1, "INSERT INTO t VALUES(2, 'two')");
        exec(db1, "UPDATE t SET b = 'mod' WHERE a = 1");
        let mut n: c_int = 0;
        let mut buf: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sqlite3session_changeset(sess, &mut n, &mut buf),
            SQLITE_OK as c_int
        );

        // Apply on a fresh connection (its own engine): a second engine
        // identity — the apply path exercises the full C plumbing.
        let db2 = open_db();
        exec(db2, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
        exec(db2, "INSERT INTO t VALUES(1, 'base')");
        assert_eq!(
            sqlite3changeset_apply(db2, n, buf, None, omit, std::ptr::null_mut()),
            SQLITE_OK as c_int
        );
        // Verify the applied state through the C query surface.
        let stmt_sql = cstr("SELECT COUNT(*), MAX(b) FROM t");
        let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db2, stmt_sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut()),
            SQLITE_OK as c_int
        );
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW as c_int);
        assert_eq!(sqlite3_column_int64(stmt, 0), 2);
        sqlite3_finalize(stmt);

        sqlite3_free(buf);
        sqlite3session_delete(sess);
        sqlite3_close(db1);
        sqlite3_close(db2);
    }
}

#[test]
fn session_capi_invert_concat_changegroup_rebaser() {
    unsafe {
        let db = open_db();
        exec(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b)");
        let mut sess: *mut sqlite3_session = std::ptr::null_mut();
        assert_eq!(
            sqlite3session_create(db, cstr("main").as_ptr(), &mut sess),
            SQLITE_OK as c_int
        );
        sqlite3session_attach(sess, std::ptr::null());
        exec(db, "INSERT INTO t VALUES(1, 'x')");
        let mut n: c_int = 0;
        let mut buf: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sqlite3session_changeset(sess, &mut n, &mut buf),
            SQLITE_OK as c_int
        );

        // invert
        let mut inv_n: c_int = 0;
        let mut inv: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sqlite3changeset_invert(n, buf, &mut inv_n, &mut inv),
            SQLITE_OK as c_int
        );
        assert!(inv_n > 0);
        sqlite3_free(inv);

        // changegroup: add + output == concat(a, a) would double the row;
        // just verify the round shape.
        let mut grp: *mut sqlite3_changegroup = std::ptr::null_mut();
        assert_eq!(sqlite3changegroup_new(&mut grp), SQLITE_OK as c_int);
        assert_eq!(sqlite3changegroup_add(grp, n, buf), SQLITE_OK as c_int);
        let mut out_n: c_int = 0;
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sqlite3changegroup_output(grp, &mut out_n, &mut out),
            SQLITE_OK as c_int
        );
        assert!(out_n > 0);
        sqlite3_free(out);
        sqlite3changegroup_delete(grp);

        // rebaser: configure + rebase a no-op rebase blob (empty).
        let mut rb: *mut sqlite3_rebaser = std::ptr::null_mut();
        assert_eq!(sqlite3rebaser_create(&mut rb), SQLITE_OK as c_int);
        let empty: [u8; 0] = [];
        assert_eq!(
            sqlite3rebaser_configure(rb, 0, empty.as_ptr() as *const c_void),
            SQLITE_OK as c_int
        );
        let mut reb_n: c_int = 0;
        let mut reb: *mut c_void = std::ptr::null_mut();
        assert_eq!(
            sqlite3rebaser_rebase(rb, n, buf, &mut reb_n, &mut reb),
            SQLITE_OK as c_int
        );
        assert_eq!(reb_n, n, "empty rebase blob passes the changeset through");
        sqlite3_free(reb);
        sqlite3rebaser_delete(rb);

        sqlite3_free(buf);
        sqlite3session_delete(sess);
        sqlite3_close(db);
    }
}

#[test]
fn session_capi_misuse_shapes() {
    unsafe {
        // NULL handles report misuse, never crash.
        assert_eq!(sqlite3session_enable(std::ptr::null_mut(), 1), -1);
        assert_eq!(
            sqlite3changeset_finalize(std::ptr::null_mut()),
            SQLITE_MISUSE as c_int
        );
        // A corrupt changeset start reports SQLITE_CORRUPT.
        let mut it: *mut sqlite3_changeset_iter = std::ptr::null_mut();
        let garbage = [0x54u8, 0x02, 0x01];
        assert_eq!(
            sqlite3changeset_start(
                &mut it,
                garbage.len() as c_int,
                garbage.as_ptr() as *const c_void
            ),
            SQLITE_OK as c_int
        );
        assert_eq!(sqlite3changeset_next(it), SQLITE_CORRUPT as c_int);
        sqlite3changeset_finalize(it);
    }
}
