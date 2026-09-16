//! Optimistic multi-writer concurrency (`BEGIN CONCURRENT`).
//!
//! SQLite's WAL allows many readers but exactly ONE writer at a time; a
//! second write transaction waits for the first to commit or roll back.
//! This module lifts that limit with the same optimistic scheme SQLite's
//! own `begin_concurrent` branch pioneered:
//!
//! 1. A `BEGIN CONCURRENT` transaction does NOT take the engine's
//!    exclusive write gate for its whole lifetime. N transactions from N
//!    connections can be open at once.
//! 2. Every page such a writer fetches is materialized as a PRIVATE
//!    SHADOW copy of the committed bytes (the live page cache stays the
//!    committed store for the whole concurrent regime). All reads and
//!    mutations — B-tree descents, splits, leaf rewrites — happen on the
//!    shadows, so uncommitted state is never observable by other
//!    connections.
//! 3. Conflict tracking is PAGE-GRANULARITY, per the engine's page-level
//!    MVCC: every page carries a committed version stamp (the epoch of
//!    the commit that last installed it). A concurrent transaction
//!    records the stamp at first fetch. Two rules follow:
//!    - fetch-time: a page whose stamp is NEWER than the transaction's
//!      begin epoch cannot be served at its BEGIN-time bytes — the
//!      transaction fails fast with [`Error::SnapshotConflict`];
//!    - commit-time: any DIRTY shadow page whose recorded stamp MOVED
//!      means another connection committed a write to a page this
//!      transaction wrote — the classic first-committer-wins validation
//!      (SQLite surfaces this as SQLITE_BUSY_SNAPSHOT / SQLITE_SNAPSHOT
//!      at COMMIT).
//! 4. COMMIT runs under a short critical section (engine write lock +
//!    the commit mutex): validate, install shadows into the live cache,
//!    splice deferred freelist entries, and reuse the WAL commit path
//!    (frames + commit marker + fsync). Because the transaction body
//!    overlapped with other writers, the critical section is only
//!    microseconds of work plus the WAL append.
//! 5. ROLLBACK is nearly free: the live cache was never touched, so
//!    dropping the shadows IS the rollback (plus splicing the
//!    transaction's fresh page allocations into the freelist so the ids
//!    are not leaked).
//!
//! What this buys over single-writer WAL:
//! - Transactions overlap in wall-clock: the think-time between
//!   statements of one transaction no longer excludes every other
//!   writer (the shape web-app pools actually run).
//! - Disjoint write sets (different tables, different hot regions)
//!   commit without conflicts; same-hot-page writers serialize through
//!   retry, exactly like the SQLite branch.
//! - Readers keep running throughout (the live cache is always the
//!   committed state in this regime — readers skip the committed-view
//!   pre-image reconstruction entirely).
//!
//! Deliberate restrictions (v1, mirroring SQLite's branch):
//! - WAL journal mode on a file-backed database is required.
//! - DDL / PRAGMA / SAVEPOINT inside a concurrent transaction error out
//!   (the in-memory catalog and savepoint undo logs are single-writer
//!   structures). Plain DML and SELECT work.
//! - While any concurrent transaction is open, plain (non-concurrent)
//!   write transactions wait — the two regimes are mutually exclusive.
//!   The gate enforces it; readers never wait.

use crate::error::{Error, Result};
use crate::storage::page::PageId;
use crate::storage::pager::{PageRef, Pager};
use parking_lot::{Mutex, RwLock};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// One open `BEGIN CONCURRENT` transaction's pager-side state.
///
/// Lives in the [`ConcurrentManager`] keyed by the engine-issued
/// transaction id; the pager routes `get_page` / `allocate_page` /
/// `free_page` / `note_dirty` through the armed [`WriterScope`] TLS into
/// these structures.
pub struct ConcurrentTxn {
    /// Engine-issued transaction id (also the WAL-visibility key).
    pub id: u64,
    /// Global commit epoch at BEGIN. A page whose stamp exceeds this was
    /// written AFTER this transaction's snapshot — fetches fail fast.
    pub begin_epoch: u64,
    /// Committed page count at BEGIN: the materialization bound. Pages at
    /// or beyond it did not exist in this transaction's snapshot (the
    /// only exceptions are this transaction's own fresh allocations,
    /// which live in `shadows` from the start).
    pub begin_n_pages: u32,
    /// Private page copies: copy-on-first-fetch for existing pages,
    /// fresh zeroed pages for allocations. The B-tree layer mutates these
    /// through the same `Arc<Mutex<Page>>` handles it always uses — it
    /// cannot tell shadows from live pages.
    pub(crate) shadows: HashMap<PageId, PageRef>,
    /// Committed version stamp at first fetch, per page read. This is the
    /// validation reference set — commit compares DIRTY shadows against it.
    pub(crate) read_stamps: HashMap<PageId, u64>,
    /// Pages allocated fresh by this transaction (freelist is bypassed in
    /// concurrent mode; allocation is serialized by the engine's
    /// statement lock, so ids are disjoint across concurrent writers).
    pub(crate) allocated: Vec<PageId>,
    /// Pages freed by tree surgery in this transaction. Spliced into the
    /// LIVE freelist at commit/rollback — freeing must never touch the
    /// shared freelist state mid-transaction (another writer's snapshot
    /// still sees those pages as live tree members).
    pub(crate) freed: Vec<PageId>,
    /// Shadow ids dirtied (mirrors the shadow pages' own `dirty` flags;
    /// kept for O(1) install-path iteration and diagnostics).
    dirtied: HashSet<PageId>,
}

impl ConcurrentTxn {
    fn new(id: u64, begin_epoch: u64, begin_n_pages: u32) -> Self {
        Self {
            id,
            begin_epoch,
            begin_n_pages,
            shadows: HashMap::new(),
            read_stamps: HashMap::new(),
            allocated: Vec::new(),
            freed: Vec::new(),
            dirtied: HashSet::new(),
        }
    }
}

/// Registry + version-stamp store for the concurrent regime.
///
/// One instance lives inside each [`Pager`]. All methods take `&self`
/// (interior mutability) because the pager is shared by reader threads.
pub struct ConcurrentManager {
    txns: Mutex<HashMap<u64, ConcurrentTxn>>,
    /// Committed page → epoch of the install that last wrote it.
    /// Absent = never written by a concurrent commit (stamp 0 semantics:
    /// the pre-concurrent committed state).
    stamps: RwLock<HashMap<PageId, u64>>,
    /// Monotonic install epoch. Bumped once per concurrent COMMIT; pages
    /// installed by that commit carry the new value.
    commit_epoch: AtomicU64,
    next_txn: AtomicU64,
    /// Serializes the commit/rollback critical sections (validate +
    /// install + WAL append). The engine's own write lock already
    /// excludes other writer STATEMENTS, but the commit ordering stamp
    /// bump + validation pair must be atomic with respect to another
    /// committer that could sneak between them — this mutex is that
    /// linearization point.
    commit_lock: Mutex<()>,
    /// Number of armed writer scopes (fast-path gate mirroring the
    /// pager's `committed_scope_count`).
    scope_count: AtomicUsize,
    /// Number of OPEN concurrent transactions, as a single atomic load:
    /// `get_page`'s plain-reader path consults it on every fetch (the
    /// committed-bound decision), so it must not take the registry
    /// mutex.
    active: AtomicUsize,
}

impl ConcurrentManager {
    pub(crate) fn new() -> Self {
        Self {
            txns: Mutex::new(HashMap::new()),
            stamps: RwLock::new(HashMap::new()),
            commit_epoch: AtomicU64::new(1),
            next_txn: AtomicU64::new(1),
            commit_lock: Mutex::new(()),
            scope_count: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        }
    }

    /// Open a transaction. `begin_n_pages` / `begin_epoch` are captured
    /// by the caller (the pager) at BEGIN time.
    pub(crate) fn begin(&self, begin_epoch: u64, begin_n_pages: u32) -> u64 {
        let id = self.next_txn.fetch_add(1, Ordering::Relaxed);
        let txn = ConcurrentTxn::new(id, begin_epoch, begin_n_pages);
        self.txns.lock().insert(id, txn);
        self.active.fetch_add(1, Ordering::AcqRel);
        id
    }

    pub(crate) fn take(&self, id: u64) -> Option<ConcurrentTxn> {
        let removed = self.txns.lock().remove(&id);
        if removed.is_some() {
            self.active.fetch_sub(1, Ordering::AcqRel);
        }
        removed
    }

    pub(crate) fn get(
        &self,
        id: u64,
    ) -> Option<parking_lot::MutexGuard<'_, HashMap<u64, ConcurrentTxn>>> {
        // Scoped helper: callers must NOT hold this guard across page I/O.
        let g = self.txns.lock();
        if g.contains_key(&id) {
            Some(g)
        } else {
            None
        }
    }

    /// True when at least one concurrent transaction is open (the
    /// regime gate: plain writers must wait, readers pass freely).
    /// Single atomic load — this is on `get_page`'s hot path.
    pub fn any_active(&self) -> bool {
        self.active.load(Ordering::Acquire) > 0
    }

    /// Number of open concurrent transactions (diagnostics).
    pub fn active_count(&self) -> usize {
        self.txns.lock().len()
    }

    /// Current commit epoch (stamps compare against this at BEGIN).
    pub fn current_epoch(&self) -> u64 {
        self.commit_epoch.load(Ordering::Acquire)
    }

    /// Committed version stamp of a page (0 = never concurrently
    /// installed — the pre-regime committed bytes).
    pub(crate) fn stamp_of(&self, id: PageId) -> u64 {
        self.stamps.read().get(&id).copied().unwrap_or(0)
    }

    /// Bump the epoch and stamp every installed page with it. Called
    /// ONLY from the commit critical section (holding `commit_lock`).
    fn stamp_installed(&self, ids: impl Iterator<Item = PageId>) -> u64 {
        let epoch = self.commit_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let mut stamps = self.stamps.write();
        for id in ids {
            stamps.insert(id, epoch);
        }
        epoch
    }

    pub(crate) fn note_scope_armed(&self) {
        self.scope_count.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn note_scope_dropped(&self) {
        self.scope_count.fetch_sub(1, Ordering::AcqRel);
    }

    /// Fast-path gate for `get_page`: nonzero = some thread somewhere has
    /// a writer scope armed, consult the TLS.
    pub fn scope_armed_any(&self) -> bool {
        self.scope_count.load(Ordering::Acquire) > 0
    }
}

thread_local! {
    /// Armed writer scope: `(pager instance id, transaction id)`. `None`
    /// on every non-concurrent path — one TLS load only when the
    /// manager's gate count is nonzero.
    static WRITER_SCOPE: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
}

/// Current armed scope (instance, txn) — `None` when not armed.
pub(crate) fn writer_scope() -> Option<(u64, u64)> {
    WRITER_SCOPE.with(|c| c.get())
}

/// RAII guard returned by [`Pager::arm_writer_scope`]. Restores the
/// previous TLS value on drop (re-entrancy safe) and keeps the
/// manager's gate count balanced. Must be held for the duration of one
/// statement (or one API call) executing on behalf of the transaction.
#[must_use]
pub struct WriterScopeGuard<'a> {
    prev: Option<(u64, u64)>,
    pager: &'a Pager,
}

impl Drop for WriterScopeGuard<'_> {
    fn drop(&mut self) {
        WRITER_SCOPE.with(|c| c.set(self.prev.take()));
        self.pager.concurrent.note_scope_dropped();
    }
}

impl Pager {
    // -----------------------------------------------------------------
    // concurrent regime: lifecycle
    // -----------------------------------------------------------------

    /// Open a `BEGIN CONCURRENT` transaction. Fails unless the WAL is
    /// active (the commit protocol appends frames; DELETE-journal mode
    /// has no multi-writer commit point to reuse) and the database is
    /// file-backed. Returns the transaction id the engine uses for
    /// statement routing and COMMIT/ROLLBACK.
    pub fn begin_concurrent(&self) -> Result<u64> {
        if self.lazy_writeback.load(Ordering::Acquire) {
            return Err(Error::Unsupported(
                "BEGIN CONCURRENT requires a file-backed WAL-mode database (not :memory:)",
            ));
        }
        if self.wal.read().is_none() {
            return Err(Error::Unsupported(
                "BEGIN CONCURRENT requires journal_mode=WAL",
            ));
        }
        // The committed page count as of BEGIN. Plain commits keep this
        // in sync at every flush; concurrent installs update it at their
        // own flush.
        let begin_n_pages = self.committed_n_pages.load(Ordering::Acquire);
        let begin_epoch = self.concurrent.current_epoch();
        self.note_tx_begun();
        Ok(self.concurrent.begin(begin_epoch, begin_n_pages))
    }

    /// Arm this thread's writer scope for `txn_id` on THIS pager.
    /// `get_page` then serves the transaction's shadows; `allocate_page`
    /// / `free_page` / `note_dirty` route into the transaction.
    pub fn arm_writer_scope(&self, txn_id: u64) -> WriterScopeGuard<'_> {
        let prev = WRITER_SCOPE.with(|c| c.replace(Some((self.instance_id, txn_id))));
        self.concurrent.note_scope_armed();
        WriterScopeGuard { prev, pager: self }
    }

    /// The (instance, txn) this thread's scope is armed with, if any.
    pub fn armed_writer_scope(&self) -> Option<u64> {
        if !self.concurrent.scope_armed_any() {
            return None;
        }
        match writer_scope() {
            Some((inst, txn)) if inst == self.instance_id => Some(txn),
            _ => None,
        }
    }

    /// True while the concurrent regime is active (any open concurrent
    /// transaction). Readers use the plain live path (the live cache IS
    /// the committed store in this regime) but must bound page ids by
    /// the COMMITTED page count, not the live allocation high-water.
    pub fn concurrent_writers_active(&self) -> bool {
        self.concurrent.any_active()
    }

    /// COMMIT a concurrent transaction: validate the write-set stamps,
    /// install shadows into the live cache, splice deferred frees, and
    /// run the standard WAL commit. On conflict the transaction is fully
    /// rolled back (shadows dropped, allocations recycled) before the
    /// error surfaces — the caller never needs a separate ROLLBACK.
    pub fn commit_concurrent(&self, txn_id: u64) -> Result<()> {
        let _commit = self.concurrent.commit_lock.lock();
        let Some(txn) = self.concurrent.take(txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };

        // ---- validation: first-committer-wins on the WRITE set.
        //
        // Textbook snapshot isolation: reads come from the transaction's
        // BEGIN-time snapshot (guaranteed by the fetch-time stamp gate —
        // every materialized page predates begin_epoch, or the fetch
        // aborted), so only WRITE-WRITE overlaps need commit-time
        // validation. A dirty shadow page whose committed stamp moved
        // since it was fetched means another connection committed a write
        // to the same page: installing would clobber it — abort.
        //
        // PAGE 0 (dirty): the header region is rewritten by every commit
        // and only stamped when a schema-row rewrite installed, so a
        // moved stamp on a DIRTY page-0 shadow is a genuine lost-root
        // hazard — it conflicts. Clean page-0 fetchers never conflict.
        let mut conflicts: Vec<PageId> = Vec::new();
        {
            let stamps = self.concurrent.stamps.read();
            for (id, shadow) in txn.shadows.iter() {
                if !shadow.lock().dirty {
                    continue; // read-only snapshot copy — nothing to install
                }
                let recorded = txn.read_stamps.get(id).copied().unwrap_or(0);
                let now = stamps.get(id).copied().unwrap_or(0);
                if now != recorded {
                    conflicts.push(*id);
                }
            }
        }
        if !conflicts.is_empty() {
            // Full rollback before surfacing: recycle allocations so the
            // ids are not leaked, drop shadows (the live cache was never
            // touched — nothing else to undo).
            self.splice_txn_freed(&txn.allocated)?;
            return Err(Error::SnapshotConflict(format!(
                "page{} {} changed by a concurrent commit since this transaction began; \
                 the transaction was rolled back — retry it",
                if conflicts.len() == 1 { "" } else { "s" },
                conflicts
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        // ---- install: shadows → live cache (content-replace under the
        // page mutex, so any Arc clones held elsewhere stay valid).
        //
        // Read-only shadow copies (never dirtied) install nothing: the
        // live bytes are identical by construction.
        let psz = self.page_size() as usize;
        let mut installed: Vec<PageId> = Vec::with_capacity(txn.shadows.len());
        {
            let mut cache = self.cache.write();
            for (id, shadow) in txn.shadows.iter() {
                let dirty = shadow.lock().dirty;
                if !dirty {
                    continue; // clean copy — live bytes already match
                }
                installed.push(*id);
                match cache.get(*id).cloned() {
                    Some(live) => {
                        let mut dst = live.lock();
                        let src = shadow.lock();
                        dst.data.copy_from_slice(&src.data);
                        dst.dirty = true;
                    }
                    None => {
                        let src = shadow.lock();
                        let mut page = crate::storage::page::Page::new(*id, psz as u32);
                        page.data.copy_from_slice(&src.data);
                        page.dirty = true;
                        drop(src);
                        let pr: PageRef = Arc::new(Mutex::new(page));
                        self.maybe_evict_locked(&mut cache);
                        cache.insert(*id, pr);
                        self.lru.lock().push_back(*id);
                    }
                }
                self.note_dirty(*id);
            }
        }
        // Page-0 header refresh rides the flush (n_pages high-water,
        // freelist, cookie) — the standard flush_wal path does it.

        // ---- deferred frees → live freelist (trunk-format splice).
        self.splice_txn_freed(&txn.freed)?;

        // ---- WAL commit: frames for every installed page + splice
        // writes + header, one commit marker, fsync per synchronous.
        self.flush_wal_concurrent()?;

        // ---- publish version stamps for the installed pages.
        self.concurrent.stamp_installed(installed.into_iter());

        // Advisory caches (leaf hints, memos, chains) must re-derive
        // against the new committed state — one epoch bump covers all.
        self.write_version.fetch_add(1, Ordering::Relaxed);
        self.clear_committed_view();

        self.note_tx_committed();
        Ok(())
    }

    /// ROLLBACK a concurrent transaction: the live cache was never
    /// touched, so dropping the shadows is the whole content undo. The
    /// transaction's fresh allocations are recycled into the freelist
    /// (page ids are file-wide, so they must not leak) and one small WAL
    /// commit persists the freelist splice + header.
    pub fn rollback_concurrent(&self, txn_id: u64) -> Result<()> {
        let _commit = self.concurrent.commit_lock.lock();
        let Some(txn) = self.concurrent.take(txn_id) else {
            return Ok(()); // already ended
        };
        // Recycle THIS transaction's fresh allocations: every id in
        // `allocated` is unreferenced by the committed trees (the tree
        // surgery that linked them lived only in shadows), so freelist
        // recycling is safe. Pages in `txn.freed` were live in the
        // committed trees and the freeing lived only in shadows — they
        // stay live; do NOT recycle them.
        let recycle = txn.allocated.clone();
        self.splice_txn_freed(&recycle)?;

        if !recycle.is_empty() {
            // Persist the freelist growth + header (the WAL commit makes
            // the recycled ids durable).
            self.flush_wal_concurrent()?;
        }

        self.note_tx_rolled_back();
        Ok(())
    }

    /// Push pages onto the LIVE freelist in trunk format. Runs inside
    /// the commit/rollback critical section with NO writer scope armed,
    /// so the normal `get_page`/`free_page` machinery sees purely
    /// committed state. Reuses [`Pager::free_page`] per id — it already
    /// maintains trunk pages, atomics, and dirty tracking.
    fn splice_txn_freed(&self, ids: &[PageId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        for &id in ids {
            if id == 0 {
                continue;
            }
            // The page may exist only as a concurrent txn's shadow (fresh
            // allocation rolled back or conflicted): it has no live
            // presence yet. Publish a zeroed live page through the normal
            // freelist path so the trunk entry + on-disk image exist.
            if !self.cache.read().contains_key(id)
                && id >= self.committed_n_pages.load(Ordering::Acquire)
            {
                let psz = self.page_size();
                let mut page = crate::storage::page::Page::new(id, psz);
                page.dirty = true;
                let pr: PageRef = Arc::new(Mutex::new(page));
                let mut cache = self.cache.write();
                self.maybe_evict_locked(&mut cache);
                cache.insert(id, pr);
                drop(cache);
                self.lru.lock().push_back(id);
                self.note_dirty(id);
            }
            self.free_page(id)?;
        }
        Ok(())
    }

    /// The WAL commit for the concurrent critical section. Identical to
    /// the plain [`Pager::flush`] in WAL mode, minus the lazy-writeback
    /// and dirty-count fast paths that do not apply here (installs mark
    /// dirty explicitly), plus the committed-page-count publish.
    fn flush_wal_concurrent(&self) -> Result<()> {
        if self.wal.read().is_none() {
            // Journal mode cannot flip while a concurrent txn is open
            // (PRAGMA is rejected), but be total anyway.
            return self.flush_inner_delete();
        }
        let r = self.flush_wal();
        // The committed bound moves WITH the commit: every page the
        // install wrote is now committed, and the header's n_pages rides
        // the flush.
        self.committed_n_pages
            .store(self.n_pages.load(Ordering::Acquire), Ordering::Release);
        r
    }

    // -----------------------------------------------------------------
    // concurrent regime: page routing (called from get_page/alloc/free)
    // -----------------------------------------------------------------

    /// Fetch `id` from the armed transaction's shadows, materializing a
    /// private copy of the COMMITTED bytes on first touch. Returns
    /// [`Error::SnapshotConflict`] when the page's committed stamp moved
    /// past the transaction's begin epoch (another connection committed
    /// a write to it after this transaction's snapshot — the old bytes
    /// cannot be reconstructed without WAL version history, so the
    /// transaction fails now rather than at COMMIT).
    pub(crate) fn writer_shadow_fetch(&self, txn_id: u64, id: PageId) -> Result<PageRef> {
        let guard = match self.concurrent.get(txn_id) {
            Some(g) => g,
            None => {
                return Err(Error::Transaction(format!(
                    "concurrent transaction {} is not open",
                    txn_id
                )))
            }
        };
        let Some(txn) = guard.get(&txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        // Fast path: shadow hit (fetched before, or freshly allocated).
        if let Some(pr) = txn.shadows.get(&id) {
            return Ok(pr.clone());
        }
        // Bounds: pages beyond the BEGIN committed count did not exist
        // in this snapshot.
        if id >= txn.begin_n_pages {
            return Err(Error::corruption(format!(
                "page {} out of range (concurrent snapshot n_pages={})",
                id, txn.begin_n_pages
            )));
        }
        // Conflict gate: the page's committed version must not be newer
        // than the transaction's snapshot. PAGE 0 is exempt: its header
        // region is rewritten by every commit, and its schema rows only
        // move with root moves — a page-0 reader serves the newest
        // committed bytes instead of aborting (a read-committed page 0;
        // every other page keeps the strict BEGIN-time snapshot).
        let stamp = self.concurrent.stamp_of(id);
        if stamp > txn.begin_epoch && id != 0 {
            drop(guard);
            return Err(Error::SnapshotConflict(format!(
                "page {} was committed by another connection after this \
                 transaction began; the transaction must be retried",
                id
            )));
        }
        // Materialize the committed bytes WITHOUT the registry guard
        // held (the plain get_page path takes cache/WAL locks).
        let begin_n_pages = txn.begin_n_pages;
        drop(guard);

        // Read the committed bytes: the live path (no scope armed here —
        // we are inside get_page's writer branch, the committed-view TLS
        // is not set for the writer thread) serves exactly the
        // committed state in this regime.
        let live = self.fetch_committed_bytes(id, begin_n_pages)?;
        let psz = self.page_size();
        let mut page = crate::storage::page::Page::new(id, psz);
        page.data.copy_from_slice(&live);
        let pr: PageRef = Arc::new(Mutex::new(page));

        let mut guard = match self.concurrent.get(txn_id) {
            Some(g) => g,
            None => {
                return Err(Error::Transaction(format!(
                    "concurrent transaction {} is not open",
                    txn_id
                )))
            }
        };
        let Some(txn) = guard.get_mut(&txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        // Another statement of this txn may have raced the materialize
        // (statements serialize on the engine write lock, but be total).
        if let Some(existing) = txn.shadows.get(&id) {
            return Ok(existing.clone());
        }
        txn.read_stamps.insert(id, stamp);
        txn.shadows.insert(id, pr.clone());
        Ok(pr)
    }

    /// Read the committed bytes of `id` (bounded by `bound` pages) into
    /// a fresh buffer: live cache → WAL committed map → main file.
    /// During the concurrent regime the live cache holds only committed
    /// pages, so a hit IS the committed state.
    fn fetch_committed_bytes(&self, id: PageId, bound: u32) -> Result<Vec<u8>> {
        if id >= bound && id != 0 {
            return Err(Error::corruption(format!(
                "page {} out of range (committed n_pages={})",
                id, bound
            )));
        }
        let psz = self.page_size() as usize;
        if let Some(pr) = self.cache.read().get(id).cloned() {
            let borrowed = pr.lock();
            return Ok(borrowed.data.clone());
        }
        // WAL-served read.
        {
            let wal_guard = self.wal.read();
            if let Some(state) = wal_guard.as_ref() {
                if let Some(&offset) = state.map.get(&id) {
                    let mut buf = vec![0u8; psz];
                    state.wal.read_frame_at(offset, &mut buf)?;
                    return Ok(buf);
                }
            }
        }
        // Main file.
        let mut buf = vec![0u8; psz];
        let offset = id as u64 * psz as u64;
        let n = self.read_file_at(offset, &mut buf)?;
        if n != psz {
            return Err(Error::corruption(format!(
                "short read on page {}: {} of {} bytes",
                id, n, psz
            )));
        }
        Ok(buf)
    }

    /// Allocate a fresh page inside the armed transaction: bumps the
    /// live `n_pages` (allocation is serialized by the engine's
    /// statement lock; ids are disjoint across concurrent writers) and
    /// parks the page in the transaction's shadows. The freelist is
    /// deliberately bypassed — popping it would mutate shared state
    /// another writer's snapshot still depends on.
    pub(crate) fn writer_shadow_alloc(&self, txn_id: u64) -> Result<PageId> {
        let id = self.n_pages.fetch_add(1, Ordering::AcqRel);
        let psz = self.page_size();
        let mut page = crate::storage::page::Page::new(id, psz);
        page.dirty = true;
        let pr: PageRef = Arc::new(Mutex::new(page));
        let mut guard = self.concurrent.get(txn_id).ok_or_else(|| {
            Error::Transaction(format!("concurrent transaction {} is not open", txn_id))
        })?;
        let Some(txn) = guard.get_mut(&txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        txn.shadows.insert(id, pr.clone());
        txn.allocated.push(id);
        // Advisory-cache epoch: the txn's own hints and any cross-thread
        // cached state must re-derive (allocations change n_pages and
        // can appear in trees via splits).
        self.write_version.fetch_add(1, Ordering::Relaxed);
        Ok(id)
    }

    /// Free a page inside the armed transaction: defer it. The shared
    /// freelist is untouched until COMMIT/ROLLBACK splices.
    pub(crate) fn writer_shadow_free(&self, txn_id: u64, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(Error::InvalidArgument("cannot free page 0".into()));
        }
        let mut guard = self.concurrent.get(txn_id).ok_or_else(|| {
            Error::Transaction(format!("concurrent transaction {} is not open", txn_id))
        })?;
        let Some(txn) = guard.get_mut(&txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        txn.freed.push(id);
        // Drop the shadow: the page is no longer part of this txn's
        // tree state. (Keep the read stamp — the validation set must
        // still cover pages the txn once read.)
        txn.shadows.remove(&id);
        self.write_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Dirty-tracking for a shadow mutation (routed from `note_dirty`
    /// when a writer scope is armed).
    pub(crate) fn writer_shadow_note_dirty(&self, txn_id: u64, id: PageId) {
        if let Some(mut guard) = self.concurrent.get(txn_id) {
            if let Some(txn) = guard.get_mut(&txn_id) {
                txn.dirtied.insert(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_begin_take_roundtrip() {
        let m = ConcurrentManager::new();
        let id = m.begin(7, 42);
        assert_eq!(m.active_count(), 1);
        assert!(m.any_active());
        assert!(m.get(id).is_some());
        assert!(m.take(id).is_some());
        assert!(!m.any_active());
        assert!(m.take(id).is_none());
    }

    #[test]
    fn stamps_start_at_zero_and_bump() {
        let m = ConcurrentManager::new();
        assert_eq!(m.stamp_of(5), 0);
        assert_eq!(m.current_epoch(), 1);
        m.stamp_installed([5u32, 6u32].into_iter());
        assert_eq!(m.stamp_of(5), 2);
        assert_eq!(m.stamp_of(6), 2);
        assert_eq!(m.stamp_of(7), 0);
        assert_eq!(m.current_epoch(), 2);
    }
}
