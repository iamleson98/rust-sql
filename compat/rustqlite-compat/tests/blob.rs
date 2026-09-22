//! sqlite3_blob_* C ABI tests — incremental blob I/O driven the C way:
//!
//! - open/read/write/bytes/reopen/close round trips on BLOB and TEXT
//!   columns, including large overflow-chain blobs (64 KB).
//! - Writes are visible to the writer's own reads, to the database
//!   (SELECT after write), to OTHER connections, and survive a reopen
//!   of the file (durable).
//! - Error contracts: out-of-range read/write, writing a readonly
//!   handle, opening a missing row / missing column / non-blob value,
//!   reopen to a missing row.
//! - Writes participate in transactions: a blob write inside BEGIN
//!   CONCURRENT rides the regime (implicit join), plain BEGIN rolls
//!   back with the transaction.

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::ptr;

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
        sql: *const c_char,
        n_byte: c_int,
        stmt: *mut *mut compat::sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_bytes(stmt: *mut compat::sqlite3_stmt, i: c_int) -> c_int;
    fn sqlite3_column_blob(stmt: *mut compat::sqlite3_stmt, i: c_int) -> *const c_void;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_blob_open(
        db: *mut compat::sqlite3,
        db_name: *const c_char,
        table: *const c_char,
        column: *const c_char,
        rowid: i64,
        flags: c_int,
        pp_blob: *mut *mut compat::sqlite3_blob,
    ) -> c_int;
    fn sqlite3_blob_read(
        blob: *mut compat::sqlite3_blob,
        z: *mut c_void,
        n: c_int,
        offset: c_int,
    ) -> c_int;
    fn sqlite3_blob_write(
        blob: *mut compat::sqlite3_blob,
        z: *const c_void,
        n: c_int,
        offset: c_int,
    ) -> c_int;
    fn sqlite3_blob_bytes(blob: *mut compat::sqlite3_blob) -> c_int;
    fn sqlite3_blob_reopen(blob: *mut compat::sqlite3_blob, rowid: i64) -> c_int;
    fn sqlite3_blob_close(blob: *mut compat::sqlite3_blob) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_READONLY: c_int = 8;
const SQLITE_ROW: c_int = 100;

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn exec(db: *mut compat::sqlite3, sql: &str) -> c_int {
    let sql_c = cstr(sql);
    let mut errmsg: *mut c_char = ptr::null_mut();
    unsafe {
        sqlite3_exec(
            db,
            sql_c.as_ptr(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut errmsg,
        )
    }
}

fn err_of(db: *mut compat::sqlite3) -> String {
    unsafe {
        CStr::from_ptr(sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned()
    }
}

/// The BLOB value of `SELECT <expr>` (single row, single column).
fn query_blob(db: *mut compat::sqlite3, sql: &str) -> Vec<u8> {
    let sql_c = cstr(sql);
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe {
        sqlite3_prepare_v2(
            db,
            sql_c.as_ptr(),
            -1,
            &mut stmt as *mut *mut compat::sqlite3_stmt,
            &mut tail as *mut *const c_char,
        )
    };
    assert_eq!(rc, SQLITE_OK, "{}", err_of(db));
    assert_eq!(unsafe { sqlite3_step(stmt) }, SQLITE_ROW, "{}", err_of(db));
    let n = unsafe { sqlite3_column_bytes(stmt, 0) } as usize;
    let p = unsafe { sqlite3_column_blob(stmt, 0) } as *const u8;
    let bytes = if n == 0 || p.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(p, n) }.to_vec()
    };
    assert_eq!(unsafe { sqlite3_finalize(stmt) }, SQLITE_OK);
    bytes
}

/// The INTEGER value of `SELECT <expr>` (single row, single column).
fn query_int(db: *mut compat::sqlite3, sql: &str) -> i64 {
    let sql_c = cstr(sql);
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let mut tail: *const c_char = ptr::null();
    let rc = unsafe {
        sqlite3_prepare_v2(
            db,
            sql_c.as_ptr(),
            -1,
            &mut stmt as *mut *mut compat::sqlite3_stmt,
            &mut tail as *mut *const c_char,
        )
    };
    assert_eq!(rc, SQLITE_OK, "{}", err_of(db));
    assert_eq!(unsafe { sqlite3_step(stmt) }, SQLITE_ROW, "{}", err_of(db));
    let v = unsafe { sqlite3_column_int64(stmt, 0) };
    assert_eq!(unsafe { sqlite3_finalize(stmt) }, SQLITE_OK);
    v
}

fn open_db(path: &str) -> *mut compat::sqlite3 {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(path);
    assert_eq!(
        unsafe { sqlite3_open(name.as_ptr(), &mut db as *mut *mut compat::sqlite3) },
        SQLITE_OK,
        "open {path}"
    );
    db
}

fn temp_db(tag: &str) -> String {
    let pid = std::process::id();
    format!("/tmp/compat_blob_{tag}_{pid}.db")
}

/// open a blob handle the C way.
fn blob_open(
    db: *mut compat::sqlite3,
    table: &str,
    column: &str,
    rowid: i64,
    flags: c_int,
) -> (*mut compat::sqlite3_blob, c_int) {
    let mut b: *mut compat::sqlite3_blob = ptr::null_mut();
    let t = cstr(table);
    let c = cstr(column);
    let rc = unsafe {
        sqlite3_blob_open(
            db,
            ptr::null(),
            t.as_ptr(),
            c.as_ptr(),
            rowid,
            flags,
            &mut b as *mut *mut compat::sqlite3_blob,
        )
    };
    (b, rc)
}

/// Round trip on a BLOB column, including a 64 KB overflow-chain value:
/// write a patch in the middle, read slices back, verify the database
/// and the file.
#[test]
fn blob_round_trip_and_overflow() {
    let path = temp_db("rt");
    let _ = std::fs::remove_file(&path);
    let db = open_db(&path);
    assert_eq!(
        exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, payload BLOB)"),
        SQLITE_OK
    );
    let big: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
    // Insert the big blob via hex literals in chunks is awkward — bind it
    // through the SQL layer with a CAST of a big string is lossy; use
    // many small literal pieces through the concat trick instead:
    // simplest correct route — the engine's own SQL can build blobs via
    // a recursive CTE-free generator. For the test, 4 KB is plenty for
    // overflow; 64 KB built by string repetition.
    let hex: String = big.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        exec(db, &format!("INSERT INTO t VALUES (1, x'{hex}')")),
        SQLITE_OK
    );

    // Read the whole thing through the handle.
    let (b, rc) = blob_open(db, "t", "payload", 1, 0);
    assert_eq!(rc, SQLITE_OK, "{}", err_of(db));
    assert_eq!(unsafe { sqlite3_blob_bytes(b) }, 65536);
    let mut buf = vec![0u8; 4096];
    assert_eq!(
        unsafe { sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, 4096, 1000) },
        SQLITE_OK
    );
    assert_eq!(&buf[..], &big[1000..1000 + 4096]);
    // O(1) second read from the same handle.
    assert_eq!(
        unsafe { sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, 16, 65520) },
        SQLITE_OK
    );
    assert_eq!(&buf[..16], &big[65520..]);
    // Out-of-range reads.
    assert_eq!(
        unsafe { sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, 2, 65535) },
        SQLITE_ERROR
    );
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);

    // Read-write: patch the middle 16 bytes.
    let (b, rc) = blob_open(db, "t", "payload", 1, 1);
    assert_eq!(rc, SQLITE_OK);
    let patch: Vec<u8> = (0xDEu8..0xEE).collect();
    assert_eq!(
        unsafe { sqlite3_blob_write(b, patch.as_ptr() as *const c_void, 14, 32000) },
        SQLITE_OK
    );
    // The handle sees its own write.
    let mut check = [0u8; 14];
    assert_eq!(
        unsafe { sqlite3_blob_read(b, check.as_mut_ptr() as *mut c_void, 14, 32000) },
        SQLITE_OK
    );
    assert_eq!(&check, &patch[..14]);
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);

    // The DATABASE sees the write.
    let now = query_blob(db, "SELECT payload FROM t WHERE id = 1");
    assert_eq!(now.len(), 65536);
    assert_eq!(&now[32000..32014], &patch[..14]);
    assert_eq!(&now[1000..1004], &big[1000..1004]);

    assert_eq!(unsafe { sqlite3_close(db) }, SQLITE_OK);
    let back = open_db(&path);
    let persisted = query_blob(back, "SELECT payload FROM t WHERE id = 1");
    assert_eq!(&persisted[32000..32014], &patch[..14]);
    assert_eq!(unsafe { sqlite3_close(back) }, SQLITE_OK);
    let _ = std::fs::remove_file(&path);
}

/// TEXT columns work too (UTF-8 safe writes), and reopen moves the
/// handle to another row.
#[test]
fn blob_text_column_and_reopen() {
    let path = temp_db("text");
    let _ = std::fs::remove_file(&path);
    let db = open_db(&path);
    assert_eq!(
        exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)"),
        SQLITE_OK
    );
    assert_eq!(
        exec(db, "INSERT INTO t VALUES (1, 'hello blob world')"),
        SQLITE_OK
    );
    assert_eq!(
        exec(db, "INSERT INTO t VALUES (2, 'second row')"),
        SQLITE_OK
    );

    let (b, rc) = blob_open(db, "t", "note", 1, 1);
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_blob_bytes(b) }, 16);
    let repl = cstr("BLOB");
    assert_eq!(
        unsafe { sqlite3_blob_write(b, repl.as_ptr() as *const c_void, 4, 6) },
        SQLITE_OK
    );
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);
    // SELECT the patched text back.
    assert_eq!(
        query_blob(db, "SELECT note FROM t WHERE id = 1"),
        b"hello BLOB world"
    );

    // Reopen to row 2 and read it.
    let (b, rc) = blob_open(db, "t", "note", 1, 0);
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_blob_reopen(b, 2) }, SQLITE_OK);
    let mut buf = vec![0u8; 10];
    assert_eq!(
        unsafe { sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, 10, 0) },
        SQLITE_OK
    );
    assert_eq!(&buf, b"second row");
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);

    // Reopen to a MISSING row: SQLITE_ERROR.
    let (b, rc) = blob_open(db, "t", "note", 1, 0);
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_blob_reopen(b, 99) }, SQLITE_ERROR);
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);

    assert_eq!(unsafe { sqlite3_close(db) }, SQLITE_OK);
    let _ = std::fs::remove_file(&path);
}

/// The error contract: missing row / missing column / non-blob value /
/// readonly handle / out-of-range writes.
#[test]
fn blob_error_contract() {
    let path = temp_db("err");
    let _ = std::fs::remove_file(&path);
    let db = open_db(&path);
    assert_eq!(
        exec(
            db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, payload BLOB, n INTEGER)"
        ),
        SQLITE_OK
    );
    assert_eq!(
        exec(db, "INSERT INTO t VALUES (1, x'01020304', 7)"),
        SQLITE_OK
    );

    // Missing row.
    let (b, rc) = blob_open(db, "t", "payload", 42, 0);
    assert_eq!(rc, SQLITE_ERROR);
    assert!(b.is_null());

    // Missing column (engine's own error text surfaces).
    let (b, rc) = blob_open(db, "t", "nope", 1, 0);
    assert_eq!(rc, SQLITE_ERROR);
    assert!(b.is_null());

    // Non-blob value (INTEGER column).
    let (b, rc) = blob_open(db, "t", "n", 1, 0);
    assert_eq!(rc, SQLITE_ERROR);
    assert!(b.is_null());

    // Write on a READONLY handle.
    let (b, rc) = blob_open(db, "t", "payload", 1, 0);
    assert_eq!(rc, SQLITE_OK);
    let bytes = [9u8, 9];
    assert_eq!(
        unsafe { sqlite3_blob_write(b, bytes.as_ptr() as *const c_void, 2, 0) },
        SQLITE_READONLY
    );
    // A failed write leaves the value untouched.
    assert_eq!(
        query_blob(db, "SELECT payload FROM t WHERE id = 1"),
        [1u8, 2, 3, 4]
    );
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);

    // Out-of-range write on a writable handle.
    let (b, rc) = blob_open(db, "t", "payload", 1, 1);
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(
        unsafe { sqlite3_blob_write(b, bytes.as_ptr() as *const c_void, 2, 3) },
        SQLITE_ERROR
    );
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);

    assert_eq!(unsafe { sqlite3_close(db) }, SQLITE_OK);
    let _ = std::fs::remove_file(&path);
}

/// Blob writes participate in transactions: plain BEGIN rolls back with
/// the transaction, and a write during a foreign BEGIN CONCURRENT
/// regime joins it (visible + durable immediately, no BUSY).
#[test]
fn blob_write_transaction_semantics() {
    let path = temp_db("tx");
    let _ = std::fs::remove_file(&path);
    let db = open_db(&path);
    let other = open_db(&path);
    assert_eq!(
        exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, payload BLOB)"),
        SQLITE_OK
    );
    assert_eq!(exec(db, "INSERT INTO t VALUES (1, x'00000000')"), SQLITE_OK);

    // Plain BEGIN: the blob write is INSIDE it and rolls back.
    assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
    let (b, rc) = blob_open(db, "t", "payload", 1, 1);
    assert_eq!(rc, SQLITE_OK);
    let patch = [0xAAu8, 0xBB];
    assert_eq!(
        unsafe { sqlite3_blob_write(b, patch.as_ptr() as *const c_void, 2, 1) },
        SQLITE_OK
    );
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);
    // The writer's connection sees it inside the txn.
    assert_eq!(
        query_blob(db, "SELECT payload FROM t WHERE id = 1"),
        [0x00, 0xAA, 0xBB, 0x00]
    );
    assert_eq!(exec(db, "ROLLBACK"), SQLITE_OK);
    // Rolled back.
    assert_eq!(
        query_blob(db, "SELECT payload FROM t WHERE id = 1"),
        [0x00, 0x00, 0x00, 0x00]
    );

    // Foreign concurrent regime on `other`: the blob write on `db`
    // JOINS it (no BUSY at timeout 0), lands durably.
    assert_eq!(exec(other, "PRAGMA journal_mode = WAL"), SQLITE_OK);
    assert_eq!(exec(other, "BEGIN CONCURRENT"), SQLITE_OK);
    assert_eq!(exec(other, "INSERT INTO t VALUES (2, x'ff')"), SQLITE_OK);
    let (b, rc) = blob_open(db, "t", "payload", 1, 1);
    assert_eq!(rc, SQLITE_OK);
    let patch = [0x11u8, 0x22, 0x33];
    assert_eq!(
        unsafe { sqlite3_blob_write(b, patch.as_ptr() as *const c_void, 3, 0) },
        SQLITE_OK,
        "{}",
        err_of(db)
    );
    assert_eq!(unsafe { sqlite3_blob_close(b) }, SQLITE_OK);
    // Durable immediately: a third handle sees it while the regime is
    // still open.
    let third = open_db(&path);
    assert_eq!(
        query_blob(third, "SELECT payload FROM t WHERE id = 1"),
        [0x11, 0x22, 0x33, 0x00]
    );
    assert_eq!(unsafe { sqlite3_close(third) }, SQLITE_OK);
    // The regime owner's row is NOT visible to `db` (committed view).
    assert_eq!(query_int(db, "SELECT count(*) FROM t WHERE id = 2"), 0);
    assert_eq!(exec(other, "COMMIT"), SQLITE_OK);
    assert_eq!(
        query_blob(db, "SELECT payload FROM t WHERE id = 1"),
        [0x11, 0x22, 0x33, 0x00]
    );

    assert_eq!(unsafe { sqlite3_close(other) }, SQLITE_OK);
    assert_eq!(unsafe { sqlite3_close(db) }, SQLITE_OK);
    let _ = std::fs::remove_file(&path);
}
