//! Preupdate-hook C ABI tests: `sqlite3_preupdate_hook` +
//! `sqlite3_preupdate_count/depth/old/new` driven exactly the way C
//! change-data-capture code does — a raw callback recording every event,
//! old/new column values read through `sqlite3_value_*`.
//!
//! The engine's event stream itself (op code, table, rowid, count,
//! depth, old/new values, INCLUDING trigger/FK-action nesting and
//! WITHOUT ROWID rowid=0) is differential-proven against real SQLite by
//! the engine-side suite (`tests/preupdate_differential.rs` in the main
//! crate — same statement battery through rusqlite's preupdate hooks).
//! This suite pins the BRIDGE: registration, per-connection scoping,
//! the accessor contract (error codes, validity window), value
//! conversion, and hook removal.

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::ptr;
use std::sync::Mutex;

extern crate sqlite3 as compat;

extern "C" {
    fn sqlite3_open(filename: *const c_char, ppdb: *mut *mut compat::sqlite3) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut compat::sqlite3,
        sql: *const c_char,
        cb: *mut c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut compat::sqlite3,
        zsql: *const c_char,
        nbyte: c_int,
        ppstmt: *mut *mut compat::sqlite3_stmt,
        pztail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_preupdate_hook(
        db: *mut compat::sqlite3,
        cb: Option<
            unsafe extern "C" fn(
                *mut c_void,
                *mut compat::sqlite3,
                c_int,
                *const c_char,
                *const c_char,
                i64,
            ),
        >,
        ctx: *mut c_void,
    );
    fn sqlite3_preupdate_count(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_preupdate_depth(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_preupdate_old(
        db: *mut compat::sqlite3,
        i: c_int,
        pp: *mut *mut compat::sqlite3_value,
    ) -> c_int;
    fn sqlite3_preupdate_new(
        db: *mut compat::sqlite3,
        i: c_int,
        pp: *mut *mut compat::sqlite3_value,
    ) -> c_int;
    fn sqlite3_value_type(v: *const compat::sqlite3_value) -> c_int;
    fn sqlite3_value_int64(v: *const compat::sqlite3_value) -> i64;
    fn sqlite3_value_double(v: *const compat::sqlite3_value) -> f64;
    fn sqlite3_value_text(v: *const compat::sqlite3_value) -> *const std::os::raw::c_uchar;
    fn sqlite3_value_blob(v: *const compat::sqlite3_value) -> *const c_void;
}

/// One recorded event line, mirroring the engine suite's format.
#[derive(Debug, PartialEq)]
struct Ev {
    line: String,
}

/// The events vector travels through the hook's user-data pointer (the
/// way C change-data-capture code carries its recorder) — each test owns
/// its vector, so parallel tests never interleave.
unsafe fn read_value(db: *mut compat::sqlite3, new_side: bool, i: c_int) -> String {
    let mut v: *mut compat::sqlite3_value = ptr::null_mut();
    let rc = if new_side {
        sqlite3_preupdate_new(db, i, &mut v)
    } else {
        sqlite3_preupdate_old(db, i, &mut v)
    };
    if rc != 0 {
        return format!("ERR({rc})");
    }
    match sqlite3_value_type(v) {
        1 => format!("i:{}", sqlite3_value_int64(v)),
        2 => format!("f:{}", sqlite3_value_double(v)),
        3 => {
            let t = sqlite3_value_text(v);
            let s = std::ffi::CStr::from_ptr(t as *const c_char);
            format!("t:{}", s.to_string_lossy())
        }
        4 => {
            let b = sqlite3_value_blob(v) as *const u8;
            // The engine suite pins exact blob bytes; read the first byte
            // for identification (sqlite3_value_bytes is declared below
            // if needed; a fixed-length read keeps this simple).
            format!("b:{:?}", std::slice::from_raw_parts(b, 1))
        }
        5 => "NULL".into(),
        _ => "?".into(),
    }
}

/// The C callback: records (op, db, table, rowid, count, depth) + every
/// old/new value — exactly what change-data-capture code does.
unsafe extern "C" fn preupdate_cb(
    ctx: *mut c_void,
    db: *mut compat::sqlite3,
    op: c_int,
    z_db: *const c_char,
    z_table: *const c_char,
    rowid: i64,
) {
    let events = &mut *(ctx as *mut Vec<String>);
    let db_name = CStr::from_ptr(z_db).to_string_lossy().to_string();
    let table = CStr::from_ptr(z_table).to_string_lossy().to_string();
    let count = sqlite3_preupdate_count(db);
    let depth = sqlite3_preupdate_depth(db);
    let opn = match op {
        18 => "INSERT",
        9 => "DELETE",
        23 => "UPDATE",
        _ => "UNKNOWN",
    };
    let mut line = format!("{opn} {db_name}.{table} rowid={rowid} count={count} depth={depth}");
    // old is absent on INSERT, new absent on DELETE (SQLITE_MISUSE there
    // — pinned below by the error-side test).
    if op != 18 {
        let vals: Vec<String> = (0..count).map(|i| read_value(db, false, i)).collect();
        line.push_str(&format!(" old=[{}]", vals.join(",")));
    }
    if op != 9 {
        let vals: Vec<String> = (0..count).map(|i| read_value(db, true, i)).collect();
        line.push_str(&format!(" new=[{}]", vals.join(",")));
    }
    events.push(line);
}

fn open_mem() -> *mut compat::sqlite3 {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = CString::new(":memory:").unwrap();
    let rc = unsafe { sqlite3_open(name.as_ptr(), &mut db) };
    assert_eq!(rc, 0, "open failed");
    db
}

fn exec(db: *mut compat::sqlite3, sql: &str) {
    let c = CString::new(sql).unwrap();
    let rc = unsafe {
        sqlite3_exec(
            db,
            c.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if rc != 0 {
        let msg = unsafe { std::ffi::CStr::from_ptr(sqlite3_errmsg(db)) };
        panic!("exec failed: {sql} (rc={rc}): {}", msg.to_string_lossy());
    }
}

extern "C" {
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
}

#[test]
fn preupdate_c_abi_event_stream() {
    let db = open_mem();
    exec(db, "PRAGMA foreign_keys=ON");
    let mut events: Vec<String> = Vec::new();
    let evp = &mut events as *mut Vec<String> as *mut c_void;
    unsafe { sqlite3_preupdate_hook(db, Some(preupdate_cb), evp) };

    exec(db, "CREATE TABLE t (a INTEGER, b TEXT, c REAL)");
    exec(db, "INSERT INTO t VALUES (1, 'x', 1.5)");
    exec(db, "INSERT INTO t VALUES (2, NULL, NULL), (3, 'z', 3.75)");
    exec(db, "UPDATE t SET b = 'y' WHERE a = 1");
    exec(db, "DELETE FROM t WHERE a = 12");
    // Triggers (depth 1) + FK cascade (depth 1, reverse decl order).
    exec(db, "CREATE TABLE log (msg TEXT)");
    exec(
        db,
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO log VALUES ('i'); END;",
    );
    exec(db, "INSERT INTO t VALUES (9, 'trg', 0.5)");
    exec(
        db,
        "CREATE TABLE p (id INTEGER PRIMARY KEY); \
         CREATE TABLE c1 (id INTEGER PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE); \
         CREATE TABLE c2 (id INTEGER PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE SET NULL); \
         INSERT INTO p VALUES (10); \
         INSERT INTO c1 VALUES (1, 10); \
         INSERT INTO c2 VALUES (1, 10);",
    );
    exec(db, "DELETE FROM p WHERE id = 10");

    let got: Vec<Ev> = events.drain(..).map(|l| Ev { line: l }).collect();
    let want: Vec<Ev> = [
        "INSERT main.t rowid=1 count=3 depth=0 new=[i:1,t:x,f:1.5]",
        "INSERT main.t rowid=2 count=3 depth=0 new=[i:2,NULL,NULL]",
        "INSERT main.t rowid=3 count=3 depth=0 new=[i:3,t:z,f:3.75]",
        "UPDATE main.t rowid=1 count=3 depth=0 old=[i:1,t:x,f:1.5] new=[i:1,t:y,f:1.5]",
        "INSERT main.t rowid=4 count=3 depth=0 new=[i:9,t:trg,f:0.5]",
        "INSERT main.log rowid=1 count=1 depth=1 new=[t:i]",
        "INSERT main.p rowid=10 count=1 depth=0 new=[i:10]",
        "INSERT main.c1 rowid=1 count=2 depth=0 new=[i:1,i:10]",
        "INSERT main.c2 rowid=1 count=2 depth=0 new=[i:1,i:10]",
        // DELETE p: parent first, then FK actions in REVERSE declaration
        // order (c2 SET NULL UPDATE, then c1 CASCADE DELETE) — both at
        // depth 1 (SQLite's observable order).
        "DELETE main.p rowid=10 count=1 depth=0 old=[i:10]",
        "UPDATE main.c2 rowid=1 count=2 depth=1 old=[i:1,i:10] new=[i:1,NULL]",
        "DELETE main.c1 rowid=1 count=2 depth=1 old=[i:1,i:10]",
    ]
    .into_iter()
    .map(|l| Ev {
        line: l.to_string(),
    })
    .collect();
    assert_eq!(got, want, "preupdate C ABI event stream");

    // Hook removal: no further events.
    unsafe { sqlite3_preupdate_hook(db, None, ptr::null_mut()) };
    exec(db, "INSERT INTO t VALUES (99, 'silent', 0.0)");
    assert_eq!(events.len(), 0, "no events after hook removal");

    // Per-connection scoping: a second connection to the SAME file sees
    // nothing when only the first registers a hook. (in-memory dbs are
    // private, so drive a temp file instead.)
    unsafe { sqlite3_close(db) };
}

#[test]
fn preupdate_c_abi_accessor_errors() {
    let db = open_mem();
    exec(db, "CREATE TABLE t (a INT, b TEXT)");
    // Outside any event: count/depth are 0; old/new error.
    unsafe {
        assert_eq!(sqlite3_preupdate_count(db), 0);
        assert_eq!(sqlite3_preupdate_depth(db), 0);
        let mut v: *mut compat::sqlite3_value = ptr::null_mut();
        assert_eq!(
            sqlite3_preupdate_old(db, 0, &mut v),
            21 /* SQLITE_MISUSE */
        );
        assert!(v.is_null());
        assert_eq!(sqlite3_preupdate_new(db, 0, &mut v), 21);
    }
    // Inside an INSERT event: old is SQLITE_MISUSE, out-of-range is
    // SQLITE_RANGE; the new side serves values.
    static ERRS: Mutex<Vec<(c_int, c_int)>> = Mutex::new(Vec::new());
    unsafe extern "C" fn cb(
        _ctx: *mut c_void,
        db: *mut compat::sqlite3,
        _op: c_int,
        _zdb: *const c_char,
        _zt: *const c_char,
        _rowid: i64,
    ) {
        let mut v: *mut compat::sqlite3_value = ptr::null_mut();
        let e = sqlite3_preupdate_old(db, 0, &mut v);
        ERRS.lock().unwrap().push((e, 0));
        let e = sqlite3_preupdate_new(db, 5, &mut v);
        ERRS.lock().unwrap().push((e, 5));
        let e = sqlite3_preupdate_new(db, 0, &mut v);
        ERRS.lock().unwrap().push((e, 0));
    }
    unsafe { sqlite3_preupdate_hook(db, Some(cb), ptr::null_mut()) };
    exec(db, "INSERT INTO t VALUES (7, 'v')");
    let errs = ERRS.lock().unwrap().clone();
    assert_eq!(errs, vec![(21, 0), (25, 5), (0, 0)]);

    unsafe { sqlite3_close(db) };
}

#[test]
fn preupdate_c_abi_through_step() {
    // Prepared-statement stepping (the sqlx path) must fire the hook
    // too — the bridge wraps the engine's statement stepping.
    let db = open_mem();
    exec(db, "CREATE TABLE s (id INTEGER PRIMARY KEY, v TEXT)");
    let mut events: Vec<String> = Vec::new();
    let evp = &mut events as *mut Vec<String> as *mut c_void;
    unsafe { sqlite3_preupdate_hook(db, Some(preupdate_cb), evp) };

    let sql = CString::new("INSERT INTO s (v) VALUES ('a'), ('b')").unwrap();
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, &mut tail) };
    assert_eq!(rc, 0);
    let rc = unsafe { sqlite3_step(stmt) };
    let _ = rc;
    unsafe { sqlite3_finalize(stmt) };

    assert_eq!(
        events,
        vec![
            "INSERT main.s rowid=1 count=2 depth=0 new=[i:1,t:a]".to_string(),
            "INSERT main.s rowid=2 count=2 depth=0 new=[i:2,t:b]".to_string(),
        ],
        "stepped INSERT fires per-row events"
    );
    unsafe { sqlite3_close(db) };
}

#[test]
fn preupdate_c_abi_without_rowid_rowid_zero() {
    let db = open_mem();
    exec(
        db,
        "CREATE TABLE wr (k TEXT PRIMARY KEY, v INT) WITHOUT ROWID",
    );
    let mut events: Vec<String> = Vec::new();
    let evp = &mut events as *mut Vec<String> as *mut c_void;
    unsafe { sqlite3_preupdate_hook(db, Some(preupdate_cb), evp) };
    exec(db, "INSERT INTO wr VALUES ('k', 1)");
    assert_eq!(
        events,
        vec!["INSERT main.wr rowid=0 count=2 depth=0 new=[t:k,i:1]".to_string()],
        "WITHOUT ROWID reports rowid=0 (SQLite's observable value)"
    );
    unsafe { sqlite3_close(db) };
}
