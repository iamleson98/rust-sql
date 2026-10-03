//! Cross-process file locking for WAL-mode native databases.
//!
//! The engine's WAL writer lease (see [`crate::storage::wal`]) serializes
//! writers **within one process**. Two different *processes* opening the
//! same native file, however, were previously outside the engine's
//! contract entirely — the second process's page cache and WAL committed
//! map were built at ITS open and never saw the first process's commits,
//! and two concurrent writers could interleave frames with no arbiter at
//! all.
//!
//! This module closes that gap with OS-level advisory byte-range locks on
//! a dedicated lock file (`<db>-rsqllock`, created next to the database,
//! never removed — the same lifecycle as SQLite's `-shm`). Kernel
//! semantics do the heavy lifting: locks are released automatically when
//! a process dies (crash-safety without any liveness protocol), and they
//! are per-open-file-description, so two handles in one process behave
//! exactly like two handles in two processes.
//!
//! # Protocol
//!
//! | byte | name  | mode      | holder                      | lifetime
//! |------|-------|-----------|-----------------------------|---------------------------
//! | 0    | LIVE  | shared    | every file-backed WAL pager | whole handle (open→drop)
//! | 1    | WRITE | exclusive | the WAL writer              | writer-lease acquire→release
//! | 2    | READ  | shared    | each statement (non-writer) | statement scope
//!
//! - **WRITE (byte 1)** is taken wherever the in-process WAL writer lease
//!   is taken (`flush_wal_opts`, `disable_wal`, `checkpoint_wal`, the
//!   close-time fold). A foreign holder means another process is the
//!   writer: the statement fails with a busy-class error, exactly like
//!   the in-process second-handle case. Held until the writer retires —
//!   the same single-writer-handle model, now enforced system-wide.
//! - **LIVE (byte 0)** marks "a handle exists". Before a CHECKPOINT
//!   writes main-file pages in place, the checkpointing handle upgrades
//!   its own byte-0 shared lock to exclusive (a same-fd upgrade replaces
//!   its own lock; it succeeds only when NO other handle — any process —
//!   holds the shared byte). Sole handles checkpoint freely; otherwise
//!   the fold is deferred (frames stay durable in the sidecar) or, for
//!   explicit requests (`PRAGMA wal_checkpoint`, `journal_mode=DELETE`),
//!   retried on a bounded backoff — SQLite's own "cannot change journal
//!   mode while another connection is open" busy class.
//! - **READ (byte 2)** is held for the duration of a statement on handles
//!   that do not hold the writer role. It is what a deferred checkpoint
//!   would otherwise race: a reader mid-statement paging from the MAIN
//!   file while a checkpoint overwrites those pages. Writers skip it
//!   (they are the only possible writer, and their own commits update
//!   their committed map in-process), so the single-connection fast path
//!   pays nothing.
//!
//! # Crash safety
//!
//! All three locks are kernel-owned: a killed process's descriptors close
//! and every lock it held evaporates. The WAL's checksummed frames +
//! last-commit visibility rule already discard a crashed writer's torn
//! tail at recovery, so the next process that takes WRITE inherits a
//! consistent log with no liveness protocol at all.
//!
//! # Reader freshness
//!
//! Locks make coexistence SAFE; visibility of the other process's
//! commits comes from the freshness probe: each statement start on a
//! non-writer handle stats the sidecar (size + identity). Growth means
//! foreign committed frames — the committed map is re-derived
//! (checksummed) and affected cached pages dropped under the same
//! install-gate discipline an in-process concurrent commit uses. A
//! missing/replaced sidecar means a foreign checkpoint folded the log —
//! the main file is the newer committed truth and the map is rebuilt
//! from it. (See `Pager::resync_foreign_commits`.)

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Lock-file byte 0: shared while any handle has the database open.
pub(crate) const LIVE_BYTE: u64 = 0;
/// Lock-file byte 1: exclusive while a process holds the WAL writer role.
pub(crate) const WRITE_BYTE: u64 = 1;
/// Lock-file byte 2: shared while a non-writer handle runs a statement.
pub(crate) const READ_BYTE: u64 = 2;

const RANGE_LEN: u64 = 1;

/// The fd-level mode of one lock byte, as this handle last established
/// it (userspace mirror of the kernel state — see [`XLock::state`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
enum Mode {
    #[default]
    None = 0,
    Shared = 1,
    Exclusive = 2,
}

/// Userspace mirror of the kernel lock state for THIS handle's bytes.
/// Two platform gaps force the bookkeeping:
///
/// * **Windows re-locks**: `LockFileEx` on a byte-range THE SAME HANDLE
///   already locks exclusively fails with `ERROR_LOCK_VIOLATION` — it
///   has no Linux-OFD "a same-fd request replaces the fd's own lock"
///   semantics. The concurrent regime re-takes the WRITE byte through
///   the same handle on every writer identity's flush (one engine, one
///   pager, one lock file descriptor); on Windows that second take saw
///   its OWN lock as a foreign conflict and the whole
///   `concurrent_edge_interleavings` suite failed with a bogus
///   "held by a connection in ANOTHER process" BUSY. A same-mode
///   re-request is satisfied from the mirror — zero syscalls, and the
///   same idempotency Linux OFD gives natively.
/// * **Windows upgrades**: shared→exclusive on the same handle must be
///   a manual unlock→try→restore dance (Linux OFD replaces atomically).
///
/// The mirror is guarded by one non-blocking-syscall mutex; every
/// `ofd` call checks and updates it under that lock, so it can never
/// disagree with the kernel about what THIS fd holds. READ (byte 2) is
/// additionally reference-counted: fd-level locks have no counts, so
/// overlapping per-statement guards on one pager would otherwise drop
/// the byte at the FIRST guard's drop, not the last statement's end.
#[derive(Default)]
struct LockState {
    modes: [Mode; 3],
    read_holds: u32,
}

/// The lock file path for a database: `<db>-rsqllock`.
pub fn xlock_path_for<P: AsRef<Path>>(db_path: P) -> PathBuf {
    let p = db_path.as_ref();
    let mut s = p.as_os_str().to_os_string();
    s.push("-rsqllock");
    PathBuf::from(s)
}

/// A handle on the cross-process lock file. Cheap to clone-free-share
/// (callers keep it in an `Arc`): every method is one or two syscalls on
/// the held descriptor.
pub struct XLock {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
    state: parking_lot::Mutex<LockState>,
}

impl XLock {
    /// Open (creating if absent) the lock file for a database. Fails only
    /// for the same reasons the WAL sidecar open fails (unwritable
    /// directory) — a database that can run WAL mode can always take a
    /// lock file.
    pub fn open<P: AsRef<Path>>(db_path: P) -> io::Result<Self> {
        let path = xlock_path_for(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            state: parking_lot::Mutex::new(LockState::default()),
        })
    }

    /// Take a shared lock on `byte` (blocking is not used anywhere in the
    /// protocol; shared locks never conflict with each other).
    pub fn lock_shared(&self, byte: u64) -> io::Result<()> {
        self.ofd(byte, true, false).map(|_| ())
    }

    /// Try to take an exclusive lock on `byte`. `Ok(false)` = someone
    /// else holds the byte (busy), not an error.
    pub fn try_lock_exclusive(&self, byte: u64) -> io::Result<bool> {
        self.ofd(byte, false, false)
    }

    /// Try to take an exclusive lock on `byte`, retrying on a small
    /// backoff until `deadline`. `Ok(false)` = still busy at the deadline.
    pub fn lock_exclusive_wait(&self, byte: u64, deadline: Instant) -> io::Result<bool> {
        loop {
            if self.ofd(byte, false, false)? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Release the lock on `byte` (whatever mode this fd held there).
    pub fn unlock(&self, byte: u64) -> io::Result<()> {
        self.ofd(byte, false, true).map(|_| ())
    }

    /// SOLE-HANDLE PROBE: upgrade byte 0 (which this fd holds shared) to
    /// EXCLUSIVE. A same-fd OFD lock request replaces the fd's own lock,
    /// and it succeeds only when no OTHER descriptor (any process —
    /// other processes' shared byte-0 locks conflict with an exclusive
    /// request) holds the byte. `Ok(true)` = we are the only handle on
    /// this database in the whole system (safe to checkpoint in place).
    pub fn try_upgrade_sole(&self) -> io::Result<bool> {
        self.ofd(LIVE_BYTE, false, false)
    }

    /// Restore the shared LIVE lock after a sole probe (or fold).
    pub fn restore_shared(&self) -> io::Result<()> {
        self.lock_shared(LIVE_BYTE)
    }

    // ----------------------------------------------------------------- core
    /// Tracker-fronted lock operation (see [`LockState`] for why the
    /// userspace mirror exists). `Ok(true)` = the requested state now
    /// holds on this fd; `Ok(false)` = a foreign holder blocks an
    /// exclusive request (busy); `Err` = a real syscall failure.
    fn ofd(&self, byte: u64, shared: bool, unlock: bool) -> io::Result<bool> {
        let mut st = self.state.lock();
        let idx = byte as usize;
        if idx >= 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "xlock protocol uses exactly three bytes",
            ));
        }

        // READ (byte 2) is the per-statement shared byte: refcounted so
        // overlapping statement guards keep the fd-level lock until the
        // LAST one drops (fd-level locks carry no counts of their own).
        if byte == READ_BYTE {
            if unlock {
                st.read_holds = st.read_holds.saturating_sub(1);
                if st.read_holds == 0 && st.modes[idx] != Mode::None {
                    let r = self.raw_ofd(byte, false, true).map(|_| ());
                    if r.is_ok() {
                        st.modes[idx] = Mode::None;
                    }
                    return r.map(|_| true);
                }
                return Ok(true);
            }
            if st.modes[idx] == Mode::Shared {
                // Held (possibly by an overlapping statement guard) — a
                // shared take never conflicts with it.
                st.read_holds += 1;
                return Ok(true);
            }
            let r = self.raw_ofd(byte, true, false);
            if matches!(r, Ok(true)) {
                st.modes[idx] = Mode::Shared;
                st.read_holds = 1;
            }
            return r;
        }

        let want = if unlock {
            Mode::None
        } else if shared {
            Mode::Shared
        } else {
            Mode::Exclusive
        };
        let cur = st.modes[idx];

        if unlock {
            if cur == Mode::None {
                // Idempotent release of a byte this fd does not hold
                // (fd semantics: unlocking an unlocked range is a no-op).
                return Ok(true);
            }
            let r = self.raw_ofd(byte, false, true).map(|_| ());
            if r.is_ok() {
                st.modes[idx] = Mode::None;
            }
            return r.map(|_| true);
        }

        if cur == want {
            // SAME-MODE re-request: satisfied from the mirror. Linux
            // OFD would replace the fd's own lock (a kernel round-trip
            // for a no-op); Windows would FAIL it as a self-conflict
            // (ERROR_LOCK_VIOLATION) — the concurrent regime's repeated
            // WRITE-byte takes through one handle depend on this arm.
            return Ok(true);
        }

        if cur != Mode::None && want == Mode::Exclusive {
            // UPGRADE (shared → exclusive) on a byte this fd holds.
            // Unix: a same-fd OFD request replaces atomically — direct.
            // Windows: no in-place upgrade — unlock, try, restore the
            // shared lock when the exclusive draw fails (the POSIX
            // failed-request-leaves-locks-untouched contract, emulated).
            #[cfg(windows)]
            {
                let unlocked = self.raw_ofd(byte, false, true).map(|_| ());
                if let Err(e) = unlocked {
                    return Err(e);
                }
                st.modes[idx] = Mode::None;
                match self.raw_ofd(byte, false, false) {
                    Ok(true) => {
                        st.modes[idx] = Mode::Exclusive;
                        return Ok(true);
                    }
                    Ok(false) => {
                        // Restore the shared lock we dropped for the
                        // attempt. A failure here is a lock-file I/O
                        // error on a byte we DO want back — surface it.
                        let restored = self.raw_ofd(byte, true, false).map(|_| ());
                        if restored.is_ok() {
                            st.modes[idx] = Mode::Shared;
                            return Ok(false);
                        }
                        return restored.map(|_| false);
                    }
                    Err(e) => return Err(e),
                }
            }
            #[cfg(unix)]
            {
                let r = self.raw_ofd(byte, false, false);
                if matches!(r, Ok(true)) {
                    st.modes[idx] = Mode::Exclusive;
                }
                return r;
            }
        }

        // Plain take (cur == None) or downgrade (exclusive → shared).
        // A downgrade via the same-fd replace is atomic on Unix; on
        // Windows LockFileEx on a range the handle holds exclusively
        // also draws ERROR_LOCK_VIOLATION, so the dance applies there
        // too — unlock first, then take the requested mode.
        #[cfg(windows)]
        if cur != Mode::None {
            let unlocked = self.raw_ofd(byte, false, true).map(|_| ());
            if let Err(e) = unlocked {
                return Err(e);
            }
            st.modes[idx] = Mode::None;
        }
        let r = self.raw_ofd(byte, shared, false);
        if matches!(r, Ok(true)) {
            st.modes[idx] = want;
        }
        r
    }

    // ---------------------------------------------------------------- unix
    #[cfg(unix)]
    fn raw_ofd(&self, byte: u64, shared: bool, unlock: bool) -> io::Result<bool> {
        use std::os::raw::c_int;
        use std::os::unix::io::AsRawFd;
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = if unlock {
            libc::F_UNLCK as std::os::raw::c_short
        } else if shared {
            libc::F_RDLCK as std::os::raw::c_short
        } else {
            libc::F_WRLCK as std::os::raw::c_short
        };
        fl.l_whence = libc::SEEK_SET as std::os::raw::c_short;
        fl.l_start = byte as libc::off_t;
        fl.l_len = RANGE_LEN as libc::off_t;
        // F_OFD_GETLK is never used; the pid field is only meaningful for
        // F_GETLK — zeroed above.
        let cmd: c_int = libc::F_OFD_SETLK;
        loop {
            let rc = unsafe { libc::fcntl(self.file.as_raw_fd(), cmd, &fl) };
            if rc == 0 {
                return Ok(true);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                // EAGAIN == EWOULDBLOCK on Linux (same constant), and
                // macOS reports EACCES for a conflicting OFD request —
                // all three spell "another descriptor holds the byte".
                Some(libc::EAGAIN) | Some(libc::EACCES) => return Ok(false),
                Some(libc::EINTR) => continue,
                _ => return Err(err),
            }
        }
    }

    // ------------------------------------------------------------- windows
    #[cfg(windows)]
    fn raw_ofd(&self, byte: u64, shared: bool, unlock: bool) -> io::Result<bool> {
        // Raw kernel32 FFI (the wal.rs `file_identity` pattern — the
        // crate deliberately has no windows-sys/winapi dependency).
        use std::os::raw::c_void;
        use std::os::windows::io::AsRawHandle;

        const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
        const ERROR_LOCK_VIOLATION: i32 = 33;

        #[repr(C)]
        #[allow(non_snake_case)]
        struct Overlapped {
            internal: usize,
            internal_high: usize,
            offset: u32,
            offset_high: u32,
            event: *mut c_void,
        }

        #[allow(non_snake_case)]
        #[link(name = "kernel32")]
        extern "system" {
            fn LockFileEx(
                hFile: *mut c_void,
                dwFlags: u32,
                dwReserved: u32,
                nBytesLow: u32,
                nBytesHigh: u32,
                lpOverlapped: *mut Overlapped,
            ) -> i32;
            fn UnlockFileEx(
                hFile: *mut c_void,
                dwReserved: u32,
                nBytesLow: u32,
                nBytesHigh: u32,
                lpOverlapped: *mut Overlapped,
            ) -> i32;
        }

        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: byte as u32,
            offset_high: (byte >> 32) as u32,
            event: std::ptr::null_mut(),
        };
        let handle = self.file.as_raw_handle() as *mut c_void;
        let ok = if unlock {
            unsafe { UnlockFileEx(handle, 0, RANGE_LEN as u32, 0, &mut ov) }
        } else {
            let mut flags = LOCKFILE_FAIL_IMMEDIATELY;
            if !shared {
                flags |= LOCKFILE_EXCLUSIVE_LOCK;
            }
            unsafe { LockFileEx(handle, flags, 0, RANGE_LEN as u32, 0, &mut ov) }
        };
        if ok != 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
            return Ok(false);
        }
        Err(err)
    }
}

impl Drop for XLock {
    fn drop(&mut self) {
        // The descriptor close releases every lock this fd held; nothing
        // else to do. (Locks are per-fd: closing does NOT disturb locks
        // held by other descriptors on the same file.)
    }
}

/// Shared per-statement READ guard: releases byte 2 when dropped.
///
/// Lock-file syscall failures after acquisition are protocol
/// best-effort (the alternative — failing a pure read because an
/// advisory lock could not be released — trades availability for
/// nothing: the kernel releases the byte at process death regardless).
pub struct XReadGuard {
    lock: std::sync::Arc<XLock>,
}

impl XReadGuard {
    pub(crate) fn new(lock: std::sync::Arc<XLock>) -> Self {
        Self { lock }
    }
}

impl Drop for XReadGuard {
    fn drop(&mut self) {
        let _ = self.lock.unlock(READ_BYTE);
    }
}

/// Exclusive-LIVE guard held across a checkpoint's main-file writes:
/// restores the shared LIVE lock on drop. `none()` constructs the
/// vacuous guard for pagers without a lock file (in-memory stores,
/// DELETE-mode files) so the checkpoint path stays branch-free.
pub struct XSoleGuard {
    lock: Option<std::sync::Arc<XLock>>,
}

impl XSoleGuard {
    pub(crate) fn new(lock: std::sync::Arc<XLock>) -> Self {
        Self { lock: Some(lock) }
    }

    pub(crate) fn none() -> Self {
        Self { lock: None }
    }
}

impl Drop for XSoleGuard {
    fn drop(&mut self) {
        if let Some(l) = self.lock.as_ref() {
            let _ = l.restore_shared();
        }
    }
}
