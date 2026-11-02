//! In-process coordination between several `Database` handles on ONE
//! rollback-journal (DELETE-mode) file.
//!
//! Each `Database::open` builds an independent engine with its own page
//! cache. WAL-mode files coordinate through the sidecar log (see
//! `storage::xlock`), but a DELETE-mode commit rewrites main-file pages
//! in place and nothing told the other handles: they kept serving
//! stale cached pages, and a second handle's commit silently overwrote
//! the first's (a lost update — `a` inserts 2, `b` inserts 3 from a
//! stale view, the file ends up with only one of them).
//!
//! The protocol, per canonical path, in-process only:
//! - a shared COMMIT GENERATION bumped by every DELETE-mode commit; a
//!   handle remembers the generation its cache reflects. A statement
//!   that starts on a stale handle drops the cache and re-reads the
//!   header + schema (the same recipe the WAL freshness probe uses);
//! - a commit whose handle is STALE at commit time (a peer committed
//!   since this handle's view, with dirty pages built on the old view)
//!   fails with a retriable `SnapshotConflict` instead of overwriting;
//! - a commit excludes peers' in-flight statements (main-file pages are
//!   rewritten in place, so a concurrent reader could see a torn
//!   state) and is excluded by them. A handle's OWN statements never
//!   block its own commit (a `RETURNING` DML or an open cursor commits
//!   from inside its own read scope).
//!
//! A SOLE handle pays one relaxed atomic compare per statement and one
//! atomic add per commit.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    /// Statements currently inside a read scope, across all handles.
    readers: u32,
    /// A handle is rewriting main-file pages.
    committing: bool,
    /// Read scopes owned by handles that are blocked in `commit_enter`.
    /// A writer commits from inside its own statement's scope, so two
    /// writers each count as the other's "reader"; discounting the
    /// waiting committers' own scopes keeps them from deadlocking.
    waiting_own: u32,
}

pub(crate) struct PeerShared {
    gen: AtomicU64,
    st: Mutex<State>,
    cv: Condvar,
}

/// How long a statement or commit waits for the other side before it
/// reports a busy-class error.
const WAIT: Duration = Duration::from_millis(500);

impl PeerShared {
    pub(crate) fn generation(&self) -> u64 {
        self.gen.load(Ordering::Acquire)
    }

    /// Enter a read scope; blocks while a peer commits.
    pub(crate) fn read_enter(&self) -> bool {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + WAIT;
        while st.committing {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.readers += 1;
        true
    }

    pub(crate) fn read_exit(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.readers = st.readers.saturating_sub(1);
        drop(st);
        self.cv.notify_all();
    }

    /// Enter the commit section once no peer statement is in flight
    /// (`own_readers` of the readers are the committing handle's own).
    pub(crate) fn commit_enter(&self, own_readers: u32) -> bool {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.waiting_own += own_readers;
        self.cv.notify_all();
        let deadline = Instant::now() + WAIT;
        let mut ok = true;
        while st.committing || st.readers > st.waiting_own {
            let now = Instant::now();
            if now >= deadline {
                ok = false;
                break;
            }
            st = self
                .cv
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.waiting_own -= own_readers;
        if ok {
            st.committing = true;
        }
        ok
    }

    /// Leave the commit section; `bump` publishes a new generation and
    /// returns it.
    pub(crate) fn commit_exit(&self, bump: bool) -> u64 {
        let g = if bump {
            self.gen.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            self.gen.load(Ordering::Acquire)
        };
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.committing = false;
        drop(st);
        self.cv.notify_all();
        g
    }

    /// Publish a generation without the exclusion section (sole handle).
    pub(crate) fn bump(&self) -> u64 {
        self.gen.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// RAII read scope of one handle (see [`PeerShared::read_enter`]).
pub(crate) struct PeerReadGuard {
    shared: Arc<PeerShared>,
    own: Arc<AtomicU32>,
}

impl PeerReadGuard {
    pub(crate) fn new(shared: Arc<PeerShared>, own: Arc<AtomicU32>) -> Self {
        own.fetch_add(1, Ordering::AcqRel);
        Self { shared, own }
    }
}

impl Drop for PeerReadGuard {
    fn drop(&mut self) {
        self.own.fetch_sub(1, Ordering::AcqRel);
        self.shared.read_exit();
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<PeerShared>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Weak<PeerShared>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shared state for `key` (a [`crate::storage::wal::lease_key`]):
/// the live one when another handle is open, a fresh one otherwise.
pub(crate) fn attach(key: PathBuf) -> Arc<PeerShared> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(live) = reg.get(&key).and_then(Weak::upgrade) {
        return live;
    }
    // Entries of fully closed paths are dead Weak pointers: prune them
    // on the (rare) insert so the map tracks only live paths.
    reg.retain(|_, w| w.strong_count() > 0);
    let fresh = Arc::new(PeerShared {
        gen: AtomicU64::new(0),
        st: Mutex::new(State::default()),
        cv: Condvar::new(),
    });
    reg.insert(key, Arc::downgrade(&fresh));
    fresh
}
