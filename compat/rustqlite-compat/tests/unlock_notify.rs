//! sqlite3_unlock_notify C ABI tests — SQLite's shared-cache wait
//! machinery, driven the way C code does:
//!
//! - The immediate case: a connection with NO blocker fires the callback
//!   from within sqlite3_unlock_notify (before it returns).
//! - The blocking case (SQLITE_OPEN_SHAREDCACHE): conn A holds a
//!   transaction; conn B's write returns SQLITE_LOCKED immediately (no
//!   busy-handler polling — SQLite's shared-cache table-lock semantics);
//!   B's registration DEFERS; A's COMMIT delivers it on A's thread.
//! - Batching: a connection's multiple pending registrations deliver as
//!   ONE invocation with apArg = [arg1, arg2, ...], nArg = 2.
//! - Delivery ordering: the callback fires while the releaser's
//!   sqlite3_exec(COMMIT) is still on the stack (same thread id).
//! - Private-cache connections (the default) are untouched: they keep
//!   BUSY/busy_timeout semantics, never SQLITE_LOCKED, and never see a
//!   deferred notify.
//! - A NULL callback is a SQLITE_OK no-op; a NULL db is SQLITE_MISUSE.

#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

extern crate sqlite3 as compat;

extern "C" {
    fn sqlite3_open(filename: *const c_char, ppdb: *mut *mut compat::sqlite3) -> c_int;
    fn sqlite3_open_v2(
        filename: *const c_char,
        ppdb: *mut *mut compat::sqlite3,
        flags: c_int,
        z_vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut compat::sqlite3,
        sql: *const c_char,
        cb: *mut c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_unlock_notify(
        db: *mut compat::sqlite3,
        x_notify: Option<unsafe extern "C" fn(*mut *mut c_void, c_int)>,
        p_arg: *mut c_void,
    ) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_LOCKED: c_int = 6;
const SQLITE_MISUSE: c_int = 21;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_SHAREDCACHE: c_int = 0x0002_0000;

/// One delivered invocation: (args, narg, delivering thread).
static DELIVERED: Mutex<Vec<(Vec<usize>, usize, usize)>> = Mutex::new(Vec::new());
/// The notify-recording tests share the global DELIVERED sink — run them
/// one at a time (they are millisecond-scale).
static TEST_LOCK: Mutex<()> = Mutex::new(());
static DELIVERY_THREAD: AtomicUsize = AtomicUsize::new(0);
static MAIN_THREAD: AtomicUsize = AtomicUsize::new(0);

extern "C" fn record_notify(ap_arg: *mut *mut c_void, n_arg: c_int) {
    let n = n_arg.max(0) as usize;
    let args: Vec<usize> = (0..n).map(|i| unsafe { *ap_arg.add(i) as usize }).collect();
    DELIVERED
        .lock()
        .unwrap()
        .push((args, n, DELIVERY_THREAD.load(Ordering::Relaxed)));
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn err_of(db: *mut compat::sqlite3) -> String {
    unsafe {
        CStr::from_ptr(sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned()
    }
}

fn delivered() -> std::sync::MutexGuard<'static, Vec<(Vec<usize>, usize, usize)>> {
    DELIVERED.lock().unwrap_or_else(|e| e.into_inner())
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

fn temp_db(tag: &str) -> String {
    let pid = std::process::id();
    format!("/tmp/unlock_notify_{tag}_{pid}.db")
}

#[test]
fn unlock_notify_immediate_when_not_blocked() {
    let _test_guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    MAIN_THREAD.store(thread_id(), Ordering::Relaxed);
    let path = temp_db("immediate");
    let _ = std::fs::remove_file(&path);
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(&path);
    unsafe {
        assert_eq!(
            sqlite3_open(name.as_ptr(), &mut db as *mut *mut compat::sqlite3),
            SQLITE_OK
        );
    }
    delivered().clear();
    // No other connection, no transaction: the callback fires from
    // within sqlite3_unlock_notify itself, on THIS thread.
    DELIVERY_THREAD.store(thread_id(), Ordering::Relaxed);
    let rc = unsafe { sqlite3_unlock_notify(db, Some(record_notify), 0xdead_beef as *mut c_void) };
    assert_eq!(rc, SQLITE_OK);
    let d = delivered();
    assert_eq!(d.len(), 1, "one immediate invocation: {d:?}");
    assert_eq!(d[0].0, vec![0xdead_beef]);
    assert_eq!(d[0].1, 1);
    unsafe {
        sqlite3_close(db);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn shared_cache_locked_then_deferred_notify_on_commit() {
    let _test_guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = temp_db("blocking");
    let _ = std::fs::remove_file(&path);

    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(&path);
    let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_SHAREDCACHE;
    unsafe {
        assert_eq!(
            sqlite3_open_v2(
                name.as_ptr(),
                &mut a as *mut *mut compat::sqlite3,
                flags,
                ptr::null()
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_open_v2(
                name.as_ptr(),
                &mut b as *mut *mut compat::sqlite3,
                flags,
                ptr::null()
            ),
            SQLITE_OK
        );
    }
    assert_eq!(exec(a, "CREATE TABLE t (x INTEGER)"), SQLITE_OK);

    // A holds a write transaction.
    assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES(1)"), SQLITE_OK);

    // B's write hits the table lock: SQLITE_LOCKED immediately, the
    // exact message, and NO busy-handler wait (shared-cache semantics).
    let rc = exec(b, "INSERT INTO t VALUES(2)");
    assert_eq!(rc, SQLITE_LOCKED, "err: {}", err_of(b));
    assert_eq!(err_of(b), "database table is locked");

    // B registers. A's transaction is still live -> DEFERRED (no
    // invocation yet).
    delivered().clear();
    let rc = unsafe { sqlite3_unlock_notify(b, Some(record_notify), 777 as *mut c_void) };
    assert_eq!(rc, SQLITE_OK);
    assert!(delivered().is_empty(), "deferred, not fired");

    // A commits. The callback delivers on A's thread (the thread running
    // sqlite3_exec(COMMIT) — SQLite's documented delivery model).
    DELIVERY_THREAD.store(thread_id(), Ordering::Relaxed);
    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    let d = delivered();
    assert_eq!(d.len(), 1, "delivered once on commit: {d:?}");
    assert_eq!(d[0].0, vec![777]);
    assert_eq!(d[0].1, 1);
    drop(d);

    // B retries: the lock is gone, the write lands.
    assert_eq!(exec(b, "INSERT INTO t VALUES(2)"), SQLITE_OK);

    // Same dance with ROLLBACK as the releaser.
    assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
    assert_eq!(exec(a, "INSERT INTO t VALUES(3)"), SQLITE_OK);
    assert_eq!(exec(b, "INSERT INTO t VALUES(4)"), SQLITE_LOCKED);
    delivered().clear();
    unsafe {
        assert_eq!(
            sqlite3_unlock_notify(b, Some(record_notify), 888 as *mut c_void),
            SQLITE_OK
        );
    }
    assert_eq!(exec(a, "ROLLBACK"), SQLITE_OK);
    let d = delivered();
    assert_eq!(d.len(), 1, "delivered once on rollback: {d:?}");
    assert_eq!(d[0].0, vec![888]);

    unsafe {
        sqlite3_close(b);
        sqlite3_close(a);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unlock_notify_batches_one_connection_args() {
    let _test_guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = temp_db("batching");
    let _ = std::fs::remove_file(&path);
    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(&path);
    let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_SHAREDCACHE;
    unsafe {
        assert_eq!(
            sqlite3_open_v2(
                name.as_ptr(),
                &mut a as *mut *mut compat::sqlite3,
                flags,
                ptr::null()
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_open_v2(
                name.as_ptr(),
                &mut b as *mut *mut compat::sqlite3,
                flags,
                ptr::null()
            ),
            SQLITE_OK
        );
    }
    assert_eq!(exec(a, "CREATE TABLE t (x INTEGER)"), SQLITE_OK);
    assert_eq!(exec(a, "BEGIN"), SQLITE_OK);

    // Two registrations from the SAME connection while blocked.
    assert_eq!(exec(b, "INSERT INTO t VALUES(1)"), SQLITE_LOCKED);
    delivered().clear();
    unsafe {
        assert_eq!(
            sqlite3_unlock_notify(b, Some(record_notify), 10 as *mut c_void),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_unlock_notify(b, Some(record_notify), 20 as *mut c_void),
            SQLITE_OK
        );
    }
    // ... and one from a third blocked connection (delivered separately,
    // grouped by connection).
    let mut c: *mut compat::sqlite3 = ptr::null_mut();
    unsafe {
        assert_eq!(
            sqlite3_open_v2(
                name.as_ptr(),
                &mut c as *mut *mut compat::sqlite3,
                flags,
                ptr::null()
            ),
            SQLITE_OK
        );
    }
    assert_eq!(exec(c, "INSERT INTO t VALUES(2)"), SQLITE_LOCKED);
    unsafe {
        assert_eq!(
            sqlite3_unlock_notify(c, Some(record_notify), 30 as *mut c_void),
            SQLITE_OK
        );
    }

    DELIVERY_THREAD.store(thread_id(), Ordering::Relaxed);
    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    let d = delivered();
    // B: ONE invocation carrying BOTH args (apArg=[10,20], nArg=2);
    // C: its own invocation.
    assert_eq!(d.len(), 2, "grouped deliveries: {d:?}");
    let b_inv = d.iter().find(|(args, _, _)| args.contains(&10)).unwrap();
    assert_eq!(b_inv.0, vec![10, 20], "both args in one invocation");
    assert_eq!(b_inv.1, 2, "nArg = 2");
    let c_inv = d.iter().find(|(args, _, _)| args.contains(&30)).unwrap();
    assert_eq!(c_inv.0, vec![30]);
    drop(d);

    unsafe {
        sqlite3_close(c);
        sqlite3_close(b);
        sqlite3_close(a);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn private_cache_keeps_busy_semantics() {
    let _test_guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The DEFAULT open discipline is untouched: no SHAREDCACHE flag ->
    // cross-connection contention is BUSY + busy_timeout, never
    // SQLITE_LOCKED, and unlock_notify fires immediately.
    let path = temp_db("private");
    let _ = std::fs::remove_file(&path);
    let mut a: *mut compat::sqlite3 = ptr::null_mut();
    let mut b: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(&path);
    unsafe {
        assert_eq!(
            sqlite3_open(name.as_ptr(), &mut a as *mut *mut compat::sqlite3),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_open(name.as_ptr(), &mut b as *mut *mut compat::sqlite3),
            SQLITE_OK
        );
    }
    eprintln!("T1");
    assert_eq!(exec(a, "CREATE TABLE t (x INTEGER)"), SQLITE_OK);
    eprintln!("T2");
    assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
    eprintln!("T3");
    // With B's busy_timeout at 0 the write returns SQLITE_BUSY (never
    // SQLITE_LOCKED).
    exec(b, "PRAGMA busy_timeout = 0");
    eprintln!("T4");
    let rc = exec(b, "INSERT INTO t VALUES(1)");
    assert!(
        rc == 5 || rc == 5 | (2 << 8),
        "private cache: BUSY-family, got {rc} ({})",
        err_of(b)
    );
    // unlock_notify: not blocked in the shared-cache sense -> immediate.
    delivered().clear();
    unsafe {
        assert_eq!(
            sqlite3_unlock_notify(b, Some(record_notify), ptr::null_mut()),
            SQLITE_OK
        );
    }
    assert_eq!(delivered().len(), 1);

    assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
    unsafe {
        sqlite3_close(b);
        sqlite3_close(a);
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn null_callback_and_misuse() {
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let name = cstr(":memory:");
    unsafe {
        assert_eq!(
            sqlite3_open(name.as_ptr(), &mut db as *mut *mut compat::sqlite3),
            SQLITE_OK
        );
        // NULL callback: SQLITE_OK no-op (SQLite contract).
        assert_eq!(sqlite3_unlock_notify(db, None, ptr::null_mut()), SQLITE_OK);
        sqlite3_close(db);
        // NULL db: SQLITE_MISUSE.
        assert_eq!(
            sqlite3_unlock_notify(ptr::null_mut(), Some(record_notify), ptr::null_mut()),
            SQLITE_MISUSE
        );
    }
}

fn thread_id() -> usize {
    // A stable per-thread identity (the pointer-sized tid on Linux).
    format!("{:?}", std::thread::current().id()).hash_identity()
}

trait ThreadIdHash {
    fn hash_identity(&self) -> usize;
}

impl ThreadIdHash for String {
    fn hash_identity(&self) -> usize {
        let mut h: usize = 0xcbf2_9ce4_8422_2325;
        for b in self.bytes() {
            h ^= b as usize;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        h
    }
}
