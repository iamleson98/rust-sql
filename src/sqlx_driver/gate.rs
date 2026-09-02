//! Cooperative async transaction-gate waiting.
//!
//! Problem: a statement blocked by another connection's open transaction
//! must wait (busy timeout) before failing with SQLITE_BUSY. A plain
//! condvar wait BLOCKS the calling thread — fine on multi-threaded
//! runtimes, but on a single-threaded runtime (e.g. `#[tokio::test]`,
//! `LocalSet`) the transaction owner is another task ON THE SAME THREAD,
//! and blocking the thread starves it forever.
//!
//! Solution: async waiters register `(db, me, only_dirty, deadline, waker)`
//! with a single background *gate thread*. The waiter's future returns
//! `Pending`; the async runtime stays free to run the transaction owner.
//! When the owner commits/rolls back (`SharedDb::notify_tx_change`) or a
//! deadline passes, the gate thread wakes the waiter's waker and the
//! future re-checks the gate. 100% runtime-agnostic (std + wakers only).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::SharedDb;
use crate::sqlx_driver::error;

/// One registered async waiter.
struct GateEntry {
    db: Arc<SharedDb>,
    me: usize,
    only_dirty: bool,
    deadline: Instant,
    waker: Waker,
    /// Registration generation — lets a future re-poll (replace its entry)
    /// and deregister on drop without touching other waiters.
    gen: u64,
}

struct Gate {
    entries: parking_lot::Mutex<Vec<GateEntry>>,
    cv: parking_lot::Condvar,
    thread_started: std::sync::atomic::AtomicBool,
}

static GATE: OnceLock<Gate> = OnceLock::new();

fn gate() -> &'static Gate {
    GATE.get_or_init(|| Gate {
        entries: parking_lot::Mutex::new(Vec::new()),
        cv: parking_lot::Condvar::new(),
        thread_started: std::sync::atomic::AtomicBool::new(false),
    })
}

static NEXT_GEN: AtomicU64 = AtomicU64::new(1);

fn ensure_gate_thread() {
    let g = gate();
    if g.thread_started.load(Ordering::Acquire) {
        return;
    }
    if g
        .thread_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let spawned = std::thread::Builder::new()
            .name("rustqlite-sqlx-gate".into())
            .spawn(gate_thread_main);
        if spawned.is_err() {
            // Could not spawn (resource limits): fall back to mark unused so
            // waiters degrade to the sync condvar path via `wait_timeout`.
            g.thread_started.store(false, Ordering::Release);
        }
    }
}

/// The background gate thread: sleeps until the nearest deadline or a
/// notify, then wakes every waiter whose gate has cleared (or expired).
fn gate_thread_main() {
    let g = gate();
    loop {
        let mut entries = g.entries.lock();
        let now = Instant::now();
        let mut i = 0;
        while i < entries.len() {
            let clear = !entries[i].db.foreign_tx_blocked(entries[i].me, entries[i].only_dirty);
            let expired = now >= entries[i].deadline;
            if clear || expired {
                let entry = entries.remove(i);
                // Wake the waiter: its future re-checks and resolves.
                entry.waker.wake();
            } else {
                i += 1;
            }
        }
        if entries.is_empty() {
            // Nothing to watch: park until a new registration arrives.
            g.cv.wait(&mut entries);
        } else {
            let next = entries.iter().map(|e| e.deadline).min().unwrap();
            let dur = next.saturating_duration_since(Instant::now());
            // Floor the sleep so a just-registered entry with a tiny
            // remainder still re-scans promptly.
            let dur = dur.max(Duration::from_micros(100));
            g.cv.wait_for(&mut entries, dur);
        }
    }
}

/// Register (or re-register) an async waiter.
fn gate_register(db: &Arc<SharedDb>, me: usize, only_dirty: bool, deadline: Instant, waker: Waker, gen: u64) {
    ensure_gate_thread();
    let g = gate();
    let mut entries = g.entries.lock();
    // Replace any existing entry for the same (db, me, gen).
    if let Some(slot) = entries
        .iter_mut()
        .find(|e| e.me == me && e.gen == gen && Arc::ptr_eq(&e.db, db))
    {
        slot.waker = waker;
        slot.deadline = deadline;
        slot.only_dirty = only_dirty;
    } else {
        entries.push(GateEntry {
            db: Arc::clone(db),
            me,
            only_dirty,
            deadline,
            waker,
            gen,
        });
    }
    g.cv.notify_all();
}

/// Remove a waiter registration (future dropped / resolved elsewhere).
fn gate_deregister(db: &Arc<SharedDb>, me: usize, gen: u64) {
    let g = gate();
    let mut entries = g.entries.lock();
    entries.retain(|e| !(e.me == me && e.gen == gen && Arc::ptr_eq(&e.db, db)));
}

/// Wake the gate thread so it re-scans waiters (call on transaction state
/// changes — commit/rollback/begin — and on new registrations).
pub(super) fn gate_notify() {
    let g = gate();
    let _entries = g.entries.lock();
    g.cv.notify_all();
}

/// Future: wait until this connection's transaction gate is passable
/// (no blocking foreign transaction), or the deadline passes.
///
/// `only_dirty`: readers only wait for foreign transactions that have
/// actually written (read-only foreign transactions never block them).
pub(super) struct GateWait {
    db: Arc<SharedDb>,
    me: usize,
    only_dirty: bool,
    deadline: Instant,
    gen: u64,
    registered: bool,
}

impl GateWait {
    pub(super) fn new(
        db: Arc<SharedDb>,
        me: usize,
        only_dirty: bool,
        busy_timeout: Duration,
    ) -> Self {
        Self {
            db,
            me,
            only_dirty,
            deadline: Instant::now() + busy_timeout,
            gen: NEXT_GEN.fetch_add(1, Ordering::Relaxed),
            registered: false,
        }
    }

    fn blocked(&self) -> bool {
        self.db.foreign_tx_blocked(self.me, self.only_dirty)
    }

    fn deregister(&mut self) {
        if self.registered {
            self.registered = false;
            gate_deregister(&self.db, self.me, self.gen);
        }
    }
}

impl Future for GateWait {
    type Output = Result<(), sqlx_core::error::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.blocked() {
            self.deregister();
            return Poll::Ready(Ok(()));
        }
        if Instant::now() >= self.deadline {
            self.deregister();
            return Poll::Ready(Err(error::busy()));
        }
        gate_register(
            &self.db,
            self.me,
            self.only_dirty,
            self.deadline,
            cx.waker().clone(),
            self.gen,
        );
        self.registered = true;
        Poll::Pending
    }
}

impl Drop for GateWait {
    fn drop(&mut self) {
        self.deregister();
    }
}
