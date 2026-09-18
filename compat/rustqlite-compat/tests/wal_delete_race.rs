//! Regression: the WAL-deletion engine-retirement race (2026-09-18
//! datxevui.com outage).
//!
//! Production shape: a sqlx pool over the compat layer retires its whole
//! connection cohort at `max_lifetime` (default 30 min). The last close
//! drops the shared engine (`Weak` in the engines registry): its
//! `Pager::drop` ran `checkpoint + remove_file(<db>-wal)` with NO
//! coordination with the pool opener that was already creating the NEXT
//! engine generation — whose `PRAGMA journal_mode=WAL` had just created
//! a fresh WAL file at the same path. The dying generation's
//! `remove_file` then deleted the NEW engine's WAL, stranding it on a
//! deleted inode: every commit landed in the unlinked file (invisible,
//! non-durable), and the layout degraded into
//! `(code 10) failed to fill whole buffer` and
//! `(code 11) WAL frame salt mismatch` on every write.
//!
//! The test engineers the exact interleaving: thread A's close is the
//! LAST connection (engine retirement: slow checkpoint + sidecar
//! removal inside `sqlite3_close`), while thread B — released by a
//! handoff flag — opens the next generation and creates its WAL inside
//! that same close window. Pre-fix, B's fresh WAL gets deleted (the
//! watchdog catches the live fd to a deleted `-wal`); post-fix the
//! lifecycle guard + ownership-checked removal keep every generation's
//! WAL alive, and every committed row stays durable.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_char, c_int, CStr, CString};
use std::os::raw::c_void;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

extern crate sqlite3 as compat;

unsafe extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        ppdb: *mut *mut compat::sqlite3,
        flags: c_int,
        zvfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut compat::sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut compat::sqlite3,
        sql: *const c_char,
        cb: *const c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_errmsg(db: *mut compat::sqlite3) -> *const c_char;
    fn sqlite3_prepare_v2(
        db: *mut compat::sqlite3,
        zsql: *const c_char,
        nbyte: c_int,
        ppstmt: *mut *mut compat::sqlite3_stmt,
        pztail: *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut compat::sqlite3_stmt) -> c_int;
    fn sqlite3_column_int64(stmt: *mut compat::sqlite3_stmt, col: c_int) -> i64;
    fn sqlite3_finalize(stmt: *mut compat::sqlite3_stmt) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;

struct Db(*mut compat::sqlite3);
impl Drop for Db {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sqlite3_close(self.0) };
        }
    }
}

fn open_rw(path: &std::path::Path) -> Result<Db, String> {
    let c = CString::new(path.to_string_lossy().as_bytes().to_vec()).unwrap();
    let mut db: *mut compat::sqlite3 = ptr::null_mut();
    let rc = unsafe {
        sqlite3_open_v2(
            c.as_ptr(),
            &mut db,
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
            ptr::null(),
        )
    };
    if rc != SQLITE_OK {
        let msg = unsafe { CStr::from_ptr(sqlite3_errmsg(db)) }
            .to_string_lossy()
            .into_owned();
        unsafe { sqlite3_close(db) };
        return Err(msg);
    }
    Ok(Db(db))
}

/// Execute, mapping corruption signatures to a loud panic (they are the
/// prod failure strings and must never surface).
fn exec(db: &Db, sql: &str) -> Result<(), String> {
    let c = CString::new(sql).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rc = unsafe { sqlite3_exec(db.0, c.as_ptr(), ptr::null(), ptr::null_mut(), &mut err) };
    if rc != SQLITE_OK {
        let msg = if err.is_null() {
            format!("exec rc={rc}")
        } else {
            let m = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { libc_free(err as *mut c_void) };
            m
        };
        return Err(msg);
    }
    Ok(())
}

#[allow(non_snake_case)]
unsafe fn libc_free(p: *mut c_void) {
    extern "C" {
        fn free(p: *mut c_void);
    }
    free(p);
}

/// SELECT COUNT(*) round-trip.
fn count(db: &Db, sql: &str) -> Result<i64, String> {
    let c = CString::new(sql).unwrap();
    let mut stmt: *mut compat::sqlite3_stmt = ptr::null_mut();
    let rc = unsafe { sqlite3_prepare_v2(db.0, c.as_ptr(), -1, &mut stmt, ptr::null()) };
    if rc != SQLITE_OK {
        return Err("prepare failed".into());
    }
    let rc = unsafe { sqlite3_step(stmt) };
    let v = if rc == SQLITE_ROW {
        unsafe { sqlite3_column_int64(stmt, 0) }
    } else {
        -1
    };
    unsafe { sqlite3_finalize(stmt) };
    Ok(v)
}

fn wal_path(db: &std::path::Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

#[test]
fn wal_survives_engine_retirement_race() {
    let dir = std::env::temp_dir().join(format!("walgen-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("app.db");
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(wal_path(&db_path));

    // Schema once.
    {
        let db = open_rw(&db_path).unwrap();
        exec(
            &db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT NOT NULL)",
        )
        .unwrap();
        exec(&db, "PRAGMA journal_mode=WAL;").unwrap();
    }

    const ITERATIONS: usize = 60;
    const BATCH_ROWS: usize = 120; // slow close-time checkpoint on retirement

    // Handoff: A signals "closing NOW" right before sqlite3_close (the
    // retirement drop: checkpoint + sidecar removal runs INSIDE it); B
    // opens the next generation inside that window.
    let closing = Arc::new(AtomicBool::new(false));
    let b_done = Arc::new(AtomicBool::new(false));

    // Watchdog: consequence-based detector. The broken state is an
    // engine committing into an UNLINKED WAL: the -wal path stays absent
    // from disk for a SUSTAINED period while a connection is held (the
    // 2026-09-18 prod signature: WAL missing for ~97% of ticks while
    // inserts "succeeded"). Legitimate gaps (last-connection-close
    // removes the WAL; the next generation recreates it) are brief —
    // under 50 ms here — so only sustained absence fails. A transient
    // "(deleted)" fd on a RETIRING engine is not a failure: the handle
    // outlives the unlink only until the struct drops.
    let stop = Arc::new(AtomicBool::new(false));
    let bug_seen = Arc::new(AtomicBool::new(false));
    let open_conns = Arc::new(AtomicUsize::new(0));
    {
        let stop = stop.clone();
        let bug_seen = bug_seen.clone();
        let open_conns = open_conns.clone();
        let wal = wal_path(&db_path);
        std::thread::spawn(move || {
            let mut missing_while_open_since: Option<std::time::Instant> = None;
            while !stop.load(Ordering::Relaxed) {
                let wal_missing = std::fs::metadata(&wal).is_err();
                let conns = open_conns.load(Ordering::Relaxed);
                if wal_missing && conns > 0 {
                    let since = *missing_while_open_since.get_or_insert(std::time::Instant::now());
                    if since.elapsed() > std::time::Duration::from_millis(500) {
                        bug_seen.store(true, Ordering::Relaxed);
                    }
                } else {
                    missing_while_open_since = None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
    }

    // Thread B: opens the next generation inside A's close window.
    let expected;
    {
        let b_db_path = db_path.clone();
        let b_closing = closing.clone();
        let b_done_b = b_done.clone();
        let b_conns = open_conns.clone();
        let b = std::thread::spawn(move || -> i64 {
            let mut inserted = 0i64;
            for _ in 0..ITERATIONS {
                // Wait for A's "closing now" signal.
                let mut spins = 0;
                while !b_closing.load(Ordering::Acquire) && spins < 5_000_000 {
                    std::hint::spin_loop();
                    spins += 1;
                }
                // Open the next generation + create its WAL — racing A's
                // close-time sidecar removal.
                if let Ok(db) = open_rw(&b_db_path) {
                    b_conns.fetch_add(1, Ordering::Relaxed);
                    let _ = exec(&db, "PRAGMA journal_mode=WAL;");
                    if exec(&db, "INSERT INTO t (v) VALUES ('gen')").is_ok() {
                        inserted += 1;
                    }
                    b_conns.fetch_sub(1, Ordering::Relaxed);
                    drop(db);
                }
                b_done_b.store(true, Ordering::Release);
                // Wait for A to clear the handshake for the next round.
                let mut spins = 0;
                while b_closing.load(Ordering::Acquire) && spins < 5_000_000 {
                    std::hint::spin_loop();
                    spins += 1;
                }
            }
            inserted
        });
        // Thread A: batch insert, then retire (LAST connection).
        let a_conns = open_conns.clone();
        for i in 0..ITERATIONS {
            let db = open_rw(&db_path).unwrap();
            a_conns.fetch_add(1, Ordering::Relaxed);
            let _ = exec(&db, "PRAGMA journal_mode=WAL;");
            let mut sql = String::with_capacity(BATCH_ROWS * 30);
            for r in 0..BATCH_ROWS {
                if r > 0 {
                    sql.push(';');
                }
                sql.push_str(&format!("INSERT INTO t (v) VALUES ('a{i}-{r}')"));
            }
            sql.push(';');
            if exec(&db, &sql).is_err() {
                panic!("A's batch failed on iteration {i}");
            }
            b_done.store(false, Ordering::Release);
            closing.store(true, Ordering::Release); // "closing NOW"
            a_conns.fetch_sub(1, Ordering::Relaxed);
            drop(db); // retirement: checkpoint + remove_file inside
                      // Wait for B to finish this round.
            let mut spins = 0;
            while !b_done.load(Ordering::Acquire) && spins < 5_000_000 {
                std::hint::spin_loop();
                spins += 1;
            }
            closing.store(false, Ordering::Release);
        }
        expected = b.join().unwrap() as i64;
    }
    stop.store(true, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(30));

    // Durability: reopen and count everything committed.
    let total = {
        let db = open_rw(&db_path).unwrap();
        let _ = exec(&db, "PRAGMA journal_mode=WAL;");
        count(&db, "SELECT COUNT(*) FROM t").unwrap()
    };
    let want = expected + (ITERATIONS * BATCH_ROWS) as i64;

    assert!(
        !bug_seen.load(Ordering::Relaxed),
        "live fd to a DELETED -wal file: an engine generation ran on an \
         unlinked inode (the 2026-09-18 datxevui outage signature)"
    );
    assert_eq!(
        total, want,
        "committed rows lost across generations (got {total}, want {want})"
    );
}
