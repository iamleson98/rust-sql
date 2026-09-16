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

/// One journaled row-level write, recorded at the B-tree entry-function
/// boundary (the last point where the full semantic row — rowid + payload
/// or index key — is in hand).
///
/// The journal is what upgrades conflict detection from PAGE granularity
/// to ROW granularity: two transactions writing DIFFERENT rows of the same
/// hot leaf page both commit — the second one MERGES (replays its journal
/// onto the current committed trees) instead of retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JournalKind {
    Insert,
    /// Payload-level replace of an existing rowid (UPDATE).
    Replace,
    Delete,
}

#[derive(Debug, Clone)]
pub(crate) struct JournalOp {
    /// Canonical tree identity: the tree's root page as THIS transaction
    /// first saw it (root splits inside the transaction are folded back to
    /// the origin via `root_lineage`). Two overlapping transactions on the
    /// same tree share the same canonical root, so row-stamp keys match.
    /// Root 0 = the schema/catalog tree (its rows are schema rows).
    pub root: PageId,
    pub is_index: bool,
    pub kind: JournalKind,
    pub rowid: i64,
    /// Index ops: the encoded index key (empty for table ops).
    pub key: Vec<u8>,
    /// Table ops: the full row payload (empty for index ops / deletes).
    pub payload: Vec<u8>,
}

/// What `COMMIT` reports back to the engine so it can fix up its
/// bookkeeping maps after a MERGE (the merged trees may have different
/// roots than the transaction's private view).
#[derive(Debug, Default)]
pub struct ConcurrentCommitOutcome {
    /// True when the transaction MERGED (replayed its row journal onto the
    /// current committed trees) instead of installing its page shadows.
    pub merged: bool,
    /// `(old_root_viewed_by_engine_maps, current_committed_root)` pairs —
    /// the engine replaces these values in its table/index root maps.
    pub root_fixups: Vec<(PageId, PageId)>,
}

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
    /// Row-level write journal, in execution order. Populated by
    /// [`Pager::note_row_write`] at the B-tree entry-function boundary
    /// while a writer scope is armed. Pages whose changes are fully
    /// described by journal ops can be MERGED at commit (see
    /// [`Pager::commit_concurrent`]).
    pub(crate) row_journal: Vec<JournalOp>,
    /// Parallel to `row_journal`: the ORIGINAL outcome of update/delete
    /// class ops (did the row exist / was it applied). The merge's replay
    /// must reproduce these outcomes — a divergence means the current
    /// state drifted in a way row validation did not model, and the merge
    /// aborts (retry) instead of guessing.
    pub(crate) op_ok: Vec<bool>,
    /// Root moves performed by THIS transaction: new root -> old root.
    /// Used to canonicalize journal keys (every op keys back to the tree's
    /// origin root as seen at first touch) and, at install time, to update
    /// the manager's origin -> current-root map.
    pub(crate) root_lineage: HashMap<PageId, PageId>,
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
            row_journal: Vec::new(),
            op_ok: Vec::new(),
            root_lineage: HashMap::new(),
        }
    }

    /// Canonical (origin) root for a CURRENT root page id: chases this
    /// transaction's own root-split lineage backwards to the fixed point.
    /// A root that never moved canonicalizes to itself.
    fn canonical_root(&self, root: PageId) -> PageId {
        let mut cur = root;
        // Lineage chains are short (splits are rare); guard the loop total.
        for _ in 0..64 {
            match self.root_lineage.get(&cur) {
                Some(&old) if old != cur => cur = old,
                _ => break,
            }
        }
        cur
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
    /// Row-level write stamps: (canonical tree root, rowid) → epoch of the
    /// commit that last wrote that row. The ROW-granularity refinement of
    /// `stamps`: a dirty-page conflict is only a REAL conflict when the
    /// same (tree, rowid) was written by a concurrent commit — different
    /// rows on the same hot page MERGE instead of retrying. Index writes
    /// key by (index root, rowid): entries of one row collide (any key),
    /// entries of different rows do not.
    row_stamps: RwLock<HashMap<(PageId, i64), u64>>,
    /// Origin root → CURRENT committed root for that tree. Updated at
    /// every concurrent commit that moved a root (splits fold into the
    /// origin's entry). Absent = the origin root is still current. The
    /// merge's replay resolves each journal op's tree through this map.
    live_roots: RwLock<HashMap<PageId, PageId>>,
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
            row_stamps: RwLock::new(HashMap::new()),
            live_roots: RwLock::new(HashMap::new()),
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

    /// Stamp the rows a committing transaction's journal wrote (row-level
    /// first-committer-wins bookkeeping). Index and table ops both key by
    /// (root, rowid).
    fn stamp_rows(&self, ops: impl Iterator<Item = (PageId, i64)>, epoch: u64) {
        let mut rows = self.row_stamps.write();
        for key in ops {
            rows.insert(key, epoch);
        }
    }

    /// Committed row stamp of (canonical root, rowid). 0 = never written
    /// by a concurrent commit.
    pub(crate) fn row_stamp(&self, root: PageId, rowid: i64) -> u64 {
        self.row_stamps
            .read()
            .get(&(root, rowid))
            .copied()
            .unwrap_or(0)
    }

    /// The CURRENT committed root for the tree whose ORIGIN root is
    /// `origin` (folds root splits across commits).
    pub(crate) fn current_root_of(&self, origin: PageId) -> PageId {
        self.live_roots
            .read()
            .get(&origin)
            .copied()
            .unwrap_or(origin)
    }

    /// Record a committed root move (origin -> new current), folding any
    /// intermediate chain: each lineage entry (new -> old) resolves old's
    /// origin and points it at `new`.
    fn install_lineage(&self, lineage: &HashMap<PageId, PageId>) {
        if lineage.is_empty() {
            return;
        }
        let mut live = self.live_roots.write();
        for (&new_root, &old_root) in lineage.iter() {
            // Chase the old root's origin through the SAME lineage batch
            // (entries are in split order, so the chain resolves).
            let mut origin = old_root;
            for _ in 0..64 {
                match lineage.get(&origin) {
                    Some(&older) if older != origin => origin = older,
                    _ => break,
                }
            }
            live.insert(origin, new_root);
        }
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
        self.pager.refresh_concurrent_gate();
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
        let id = self.concurrent.begin(begin_epoch, begin_n_pages);
        self.refresh_concurrent_gate();
        Ok(id)
    }

    /// Arm this thread's writer scope for `txn_id` on THIS pager.
    /// `get_page` then serves the transaction's shadows; `allocate_page`
    /// / `free_page` / `note_dirty` route into the transaction.
    pub fn arm_writer_scope(&self, txn_id: u64) -> WriterScopeGuard<'_> {
        let prev = WRITER_SCOPE.with(|c| c.replace(Some((self.instance_id, txn_id))));
        self.concurrent.note_scope_armed();
        // Kill this thread's table-leaf hints: a hint recorded by a
        // PREVIOUS statement (or another connection sharing this thread —
        // the identity-switching harness) pins a PageRef that predates
        // this statement's shadow routing. update_table's hint path
        // patches the hinted page DIRECTLY (no get_page), so a stale hint
        // would write the LIVE page (or another txn's shadow) inside the
        // concurrent regime — an isolation breach. The epoch bump makes
        // every stored hint fail its `s.epoch == epoch` check; within THIS
        // statement the hints re-establish normally (same txn's shadows).
        self.write_version.fetch_add(1, Ordering::Relaxed);
        self.refresh_concurrent_gate();
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

    /// Recompute the combined fast-path gate from the manager's
    /// authoritative counters (bit 0 = regime active, bit 1 = scope
    /// armed). Called at regime boundaries — statement granularity at
    /// most. Transient staleness is harmless in BOTH directions:
    /// - a stale-zero regime bit serves readers `n_pages`, which equals
    ///   `committed_n_pages` until the first scoped allocation (a regime
    ///   that just began has allocated nothing);
    /// - a stale-one regime bit serves readers `committed_n_pages`,
    ///   which equals `n_pages` after the ending commit's flush (the
    ///   refresh runs after it);
    /// - the scope bit is stored by the SAME thread that arms (owners
    ///   always observe their own arm via same-thread coherence), and
    ///   every refresh recomputes it from the authoritative count, so
    ///   one owner's boundary can never drop another's bit.
    fn refresh_concurrent_gate(&self) {
        let active = self.concurrent.any_active();
        let armed = self.concurrent.scope_armed_any();
        let gate = (active as usize) | ((armed as usize) << 1);
        self.concurrent_gate.store(gate, Ordering::Release);
    }

    /// True while the concurrent regime is active (any open concurrent
    /// transaction). Readers use the plain live path (the live cache IS
    /// the committed store in this regime) but must bound page ids by
    /// the COMMITTED page count, not the live allocation high-water.
    pub fn concurrent_writers_active(&self) -> bool {
        self.concurrent_gate.load(Ordering::Acquire) & 0x1 != 0
    }

    /// COMMIT a concurrent transaction: validate, then either install the
    /// page shadows (fast path — no conflicts) or MERGE (row-level
    /// refinement: replay the transaction's row journal onto the current
    /// committed trees), and append the WAL frames. The WAL append happens
    /// inside the commit critical section; the DURABILITY wait (fsync under
    /// `PRAGMA synchronous=FULL`) is deferred to [`Pager::group_sync`] so
    /// consecutive committers amortize one fsync across the group. On
    /// conflict the transaction is fully rolled back (shadows dropped,
    /// allocations recycled) before the error surfaces — the caller never
    /// needs a separate ROLLBACK.
    pub fn commit_concurrent(&self, txn_id: u64) -> Result<ConcurrentCommitOutcome> {
        let (outcome, ticket) = self.commit_concurrent_inner(txn_id)?;
        self.group_sync(ticket)?;
        Ok(outcome)
    }

    /// [`Self::commit_concurrent`] split for the driver's group-commit
    /// pipelining: runs validate + install/merge + WAL append under the
    /// commit critical section and returns the durability ticket WITHOUT
    /// waiting for the fsync. The caller (holding no engine locks) then
    /// calls [`Pager::group_sync`] with the ticket — meanwhile the next
    /// committer's append can already overlap the fsync.
    pub fn commit_concurrent_deferred(
        &self,
        txn_id: u64,
    ) -> Result<(ConcurrentCommitOutcome, u64)> {
        self.commit_concurrent_inner(txn_id)
    }

    fn commit_concurrent_inner(&self, txn_id: u64) -> Result<(ConcurrentCommitOutcome, u64)> {
        let _commit = self.concurrent.commit_lock.lock();
        let Some(txn) = self.concurrent.take(txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        // Refresh the combined gate IMMEDIATELY after the take: every
        // early return below (the conflict abort in particular) must
        // leave a gate consistent with the registry, or the engine's
        // `concurrent_writers_active` check wedges every plain writer on
        // BUSY forever.
        self.refresh_concurrent_gate();

        // ---- validation: first-committer-wins on the WRITE set.
        //
        // Textbook snapshot isolation: reads come from the transaction's
        // BEGIN-time snapshot (guaranteed by the fetch-time stamp gate —
        // every materialized page predates begin_epoch, or the fetch
        // aborted), so only WRITE-WRITE overlaps need commit-time
        // validation. A dirty shadow page whose committed stamp moved
        // since it was fetched means another connection committed a write
        // to the same page. That is the END of the story at PAGE
        // granularity — but ROW granularity refines it: if the row
        // journal fully describes this transaction's writes and none of
        // those (tree, rowid) pairs were re-stamped by the concurrent
        // commit, the transaction MERGES (replays its journal onto the
        // current trees) instead of retrying.
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
            // Any resolution of a page conflict (merge or abort) discards
            // the page shadows, so this transaction's fresh allocations
            // are garbage either way — recycle them up front.
            self.splice_txn_freed(&txn.allocated)?;
            return self.resolve_page_conflict(
                txn.row_journal.clone(),
                txn.op_ok.clone(),
                txn.root_lineage.clone(),
                txn.begin_epoch,
                &conflicts,
            );
        }

        // ---- fast path: install shadows → live cache (content-replace
        // under the page mutex, so any Arc clones held elsewhere stay
        // valid).
        let installed = self.install_txn_shadows(&txn);
        // Page-0 header refresh rides the flush (n_pages high-water,
        // freelist, cookie) — the standard flush_wal path does it.

        // ---- deferred frees → live freelist (trunk-format splice).
        self.splice_txn_freed(&txn.freed)?;

        // ---- WAL commit: frames for every installed page + splice
        // writes + header, one commit marker. The fsync is deferred to
        // group_sync (ticket).
        let ticket = self.flush_wal_deferred_concurrent()?;

        // ---- publish version stamps for the installed pages + rows.
        let epoch = self.concurrent.stamp_installed(installed.into_iter());
        self.concurrent
            .stamp_rows(txn.row_journal.iter().map(|o| (o.root, o.rowid)), epoch);
        self.concurrent.install_lineage(&txn.root_lineage);

        // Advisory caches (leaf hints, memos, chains) must re-derive
        // against the new committed state — one epoch bump covers all.
        self.write_version.fetch_add(1, Ordering::Relaxed);
        self.clear_committed_view();

        self.note_tx_committed();
        self.refresh_concurrent_gate();
        Ok((ConcurrentCommitOutcome::default(), ticket))
    }

    /// Install a transaction's dirty shadow pages into the live cache.
    /// Read-only shadow copies (never dirtied) install nothing: the live
    /// bytes are identical by construction. Returns the installed page
    /// ids (for stamp publication).
    fn install_txn_shadows(&self, txn: &ConcurrentTxn) -> Vec<PageId> {
        let psz = self.page_size() as usize;
        let mut installed: Vec<PageId> = Vec::with_capacity(txn.shadows.len());
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
        installed
    }

    /// Resolve a dirty-page conflict with ROW granularity: validate the
    /// journal's (tree, rowid) keys against the committed row stamps and,
    /// when none moved, MERGE — discard the shadows and replay the journal
    /// into a FRESH shadow transaction armed at the current epoch (so a
    /// mid-replay failure rolls back by dropping the fresh shadows, the
    /// live cache is never touched non-atomically), then install that
    /// fresh transaction through the standard path.
    ///
    /// Caller contract: the original transaction's shadows are already
    /// discarded and its allocations recycled; `journal` / `op_ok` /
    /// `lineage` were cloned out of it.
    #[allow(clippy::too_many_arguments)]
    fn resolve_page_conflict(
        &self,
        journal: Vec<JournalOp>,
        op_ok: Vec<bool>,
        lineage: HashMap<PageId, PageId>,
        begin_epoch: u64,
        conflicts: &[PageId],
    ) -> Result<(ConcurrentCommitOutcome, u64)> {
        let page_conflict = || {
            Error::SnapshotConflict(format!(
                "page{} {} changed by a concurrent commit since this transaction began; \
                 the transaction was rolled back — retry it",
                if conflicts.len() == 1 { "" } else { "s" },
                conflicts
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
        // Row effects must be journaled for a merge to be possible at
        // all (empty journal + page conflict = structural-only writes —
        // today's page-granularity semantics).
        if journal.is_empty() {
            return Err(page_conflict());
        }
        // ---- row validation: first-committer-wins at (tree, rowid).
        for op in &journal {
            if self.concurrent.row_stamp(op.root, op.rowid) > begin_epoch {
                return Err(Error::SnapshotConflict(format!(
                    "row {} of tree {} was changed by a concurrent commit since this \
                     transaction began; the transaction was rolled back — retry it",
                    op.rowid, op.root
                )));
            }
        }

        // ---- MERGE: replay the journal onto the current committed trees
        // inside a fresh shadow transaction.
        let fresh_epoch = self.concurrent.current_epoch();
        let fresh_pages = self.committed_n_pages.load(Ordering::Acquire);
        let fresh_id = self.concurrent.begin(fresh_epoch, fresh_pages);
        self.refresh_concurrent_gate();
        let replay = {
            let _guard = self.arm_writer_scope(fresh_id);
            self.replay_journal(&journal, &op_ok, &lineage)
        };
        let fresh = match self.concurrent.take(fresh_id) {
            Some(t) => t,
            None => {
                // Unreachable (we just created it), but be total.
                return Err(page_conflict());
            }
        };
        self.refresh_concurrent_gate();
        match replay {
            Ok(()) => {}
            Err(e) => {
                // Atomic undo of the partial replay: the fresh shadows
                // drop here (live cache untouched); recycle the replay's
                // own fresh allocations.
                self.splice_txn_freed(&fresh.allocated)?;
                // Surface as a retriable snapshot conflict — the merge's
                // replay diverged from the original outcomes (state moved
                // in a way the row journal did not model).
                return Err(Error::SnapshotConflict(format!(
                    "merge replay diverged ({}); the transaction was rolled back — retry it",
                    e
                )));
            }
        }

        // ---- install the replayed state through the standard path.
        let installed = self.install_txn_shadows(&fresh);
        self.splice_txn_freed(&fresh.freed)?;
        let ticket = self.flush_wal_deferred_concurrent()?;
        let epoch = self.concurrent.stamp_installed(installed.into_iter());
        // The fresh transaction's journal re-recorded the replayed ops
        // (with keys canonicalized against ITS lineage) — stamping those
        // keys publishes exactly the rows the merged commit wrote.
        self.concurrent
            .stamp_rows(fresh.row_journal.iter().map(|o| (o.root, o.rowid)), epoch);
        self.concurrent.install_lineage(&fresh.root_lineage);

        // ---- engine bookkeeping fixups: the merged trees may be rooted
        // elsewhere than the transaction's private view (replayed splits).
        // For every canonical tree this transaction touched, tell the
        // engine which of its cached root values to replace.
        let mut outcome = ConcurrentCommitOutcome {
            merged: true,
            root_fixups: Vec::new(),
        };
        {
            let mut canonicals: HashSet<PageId> = journal.iter().map(|o| o.root).collect();
            for &new_root in lineage.keys() {
                // Fold my private split roots back to their origins.
                let mut cur = new_root;
                for _ in 0..64 {
                    match lineage.get(&cur) {
                        Some(&old) if old != cur => cur = old,
                        _ => break,
                    }
                }
                canonicals.insert(cur);
            }
            for c in canonicals {
                if c == 0 {
                    continue; // the catalog root is fixed at page 0
                }
                let current = self.concurrent.current_root_of(c);
                if current != c {
                    outcome.root_fixups.push((c, current));
                }
                // The engine's overlay may hold MY private root for this
                // tree (if I split it); map it to the merged root too.
                let mut my_view = c;
                for (&new_root, &old_root) in lineage.iter() {
                    let _ = old_root;
                    // newest key whose canonical == c
                    let mut cur = new_root;
                    for _ in 0..64 {
                        match lineage.get(&cur) {
                            Some(&old) if old != cur => cur = old,
                            _ => break,
                        }
                    }
                    if cur == c && new_root > my_view {
                        my_view = new_root;
                    }
                }
                if my_view != c && my_view != current {
                    outcome.root_fixups.push((my_view, current));
                }
            }
        }

        // Advisory caches must re-derive against the new committed state.
        self.write_version.fetch_add(1, Ordering::Relaxed);
        self.clear_committed_view();
        self.note_tx_committed();
        self.refresh_concurrent_gate();
        Ok((outcome, ticket))
    }

    /// Replay the row journal (inside a freshly armed writer scope —
    /// every page fetch/materialize/allocate/free routes into the fresh
    /// transaction's shadows, so a failure drops atomically). Catalog
    // rows (root 0) replay LAST, with their rootpage column patched to
    /// the merged trees' current roots (the private view's roots were
    /// discarded with the shadows).
    fn replay_journal(
        &self,
        journal: &[JournalOp],
        op_ok: &[bool],
        lineage: &HashMap<PageId, PageId>,
    ) -> Result<()> {
        // Canonical origin -> the fresh transaction's CURRENT view root.
        // The replay's own splits move roots mid-flight; descending from
        // the frozen origin root would insert later rows into a stale
        // subtree (ordering violation + unreachable rows). Updated after
        // every op from the Btree's post-op root.
        let mut view_roots: HashMap<PageId, PageId> = HashMap::new();
        // Phase 1: data trees.
        for (i, op) in journal.iter().enumerate() {
            if op.root == 0 {
                continue;
            }
            self.replay_one(op, op_ok.get(i).copied().unwrap_or(true), &mut view_roots)?;
        }
        // Phase 2: catalog rows (schema-row rewrites from mid-transaction
        // root syncs), rootpage patched to the post-replay current roots.
        for (i, op) in journal.iter().enumerate() {
            if op.root != 0 {
                continue;
            }
            let patched = self.patch_schema_row_rootpage(op, lineage, &view_roots)?;
            self.replay_one(
                &patched,
                op_ok.get(i).copied().unwrap_or(true),
                &mut view_roots,
            )?;
        }
        Ok(())
    }

    fn replay_one(
        &self,
        op: &JournalOp,
        expected_ok: bool,
        view_roots: &mut HashMap<PageId, PageId>,
    ) -> Result<()> {
        use crate::storage::btree::Btree;
        let root = if op.root == 0 {
            0
        } else {
            view_roots
                .get(&op.root)
                .copied()
                .unwrap_or_else(|| self.concurrent.current_root_of(op.root))
        };
        let post_root = if op.is_index {
            let mut bt = Btree::new(self, root, true);
            match op.kind {
                JournalKind::Insert => bt.insert_index(&op.key, op.rowid)?,
                JournalKind::Delete => {
                    let ok = bt.delete_index(&op.key, op.rowid)?;
                    if ok != expected_ok {
                        return Err(Error::Transaction(format!(
                            "index entry ({}, {}) existence diverged during replay",
                            op.rowid, op.root
                        )));
                    }
                }
                JournalKind::Replace => {
                    return Err(Error::Transaction("index ops have no Replace".into()))
                }
            }
            bt.root
        } else {
            let mut bt = Btree::new(self, root, false);
            match op.kind {
                JournalKind::Insert => bt.insert_table(op.rowid, &op.payload)?,
                JournalKind::Replace => {
                    // No parity check: the in-place "fits" decision depends
                    // on the CURRENT page layout (a concurrent delete may
                    // have freed room), so false->true is a legal drift —
                    // the effect (row bytes) is identical either way.
                    bt.update_table(op.rowid, &op.payload)?;
                }
                JournalKind::Delete => {
                    let ok = bt.delete_table(op.rowid)?;
                    if ok != expected_ok {
                        return Err(Error::Transaction(format!(
                            "row {} existence diverged during replay",
                            op.rowid
                        )));
                    }
                }
            }
            bt.root
        };
        if op.root != 0 {
            view_roots.insert(op.root, post_root);
        }
        Ok(())
    }

    /// Patch a catalog (schema) row's rootpage column to the merged tree's
    /// current root. The stale value is this transaction's private view
    /// root; its canonical origin is resolved through the ORIGINAL
    /// transaction's lineage, and the origin's current root through the
    /// manager's committed root map.
    fn patch_schema_row_rootpage(
        &self,
        op: &JournalOp,
        lineage: &HashMap<PageId, PageId>,
        view_roots: &HashMap<PageId, PageId>,
    ) -> Result<JournalOp> {
        // Only Insert/Replace ops carry a payload worth patching.
        if op.kind == JournalKind::Delete || (!op.is_index && op.payload.is_empty()) {
            return Ok(op.clone());
        }
        let mut row = crate::storage::row_codec::decode_row(&op.payload, 5, op.rowid, None)
            .map_err(|e| {
                Error::Transaction(format!(
                    "merge: catalog row {} does not decode as a schema row: {}",
                    op.rowid, e
                ))
            })?;
        let Some(crate::Value::Integer(stale_page)) = row.get(3) else {
            return Ok(op.clone()); // not a schema-shaped row — replay as-is
        };
        // The schema row carries SQLite's 1-BASED rootpage (internal root
        // + 1 — see `rewrite_schema_row_root` / `encode_schema_row_opt`).
        // Canonicalize the INTERNAL root, patch back in the same
        // convention.
        let Some(stale_page) = u32::try_from(*stale_page).ok() else {
            return Ok(op.clone());
        };
        if stale_page == 0 {
            return Ok(op.clone()); // root 0 = the schema tree itself
        }
        let stale_root = stale_page - 1;
        // Canonicalize the stale root through the ORIGINAL txn's lineage.
        let mut cur = stale_root;
        for _ in 0..64 {
            match lineage.get(&cur) {
                Some(&old) if old != cur => cur = old,
                _ => break,
            }
        }
        let current = view_roots
            .get(&cur)
            .copied()
            .unwrap_or_else(|| self.concurrent.current_root_of(cur));
        if current != stale_root {
            row[3] = crate::Value::Integer(current as i64 + 1);
            let mut patched = op.clone();
            patched.payload = crate::storage::row_codec::encode_row_aliased(&row, None);
            return Ok(patched);
        }
        Ok(op.clone())
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
        // See commit_concurrent: refresh immediately after the take.
        self.refresh_concurrent_gate();
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
        self.refresh_concurrent_gate();
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
        // Disk-backed page count (the DURABLE length — deliberately NOT
        // `committed_n_pages`/`n_pages`, which a mid-regime sibling commit
        // inflates with OTHER transactions' still-uncommitted allocations:
        // the flush publishes the live high-water as the committed bound).
        let psz = self.page_size();
        let disk_pages = (self.store_len() / psz as u64) as u32;
        for &id in ids {
            if id == 0 {
                continue;
            }
            // The page may exist only as a concurrent txn's shadow (fresh
            // allocation rolled back or conflicted): it has no live
            // presence yet. Publish a zeroed live page through the normal
            // freelist path so the trunk entry + on-disk image exist.
            // Servable = cached, in the committed WAL map, or below the
            // durable file length; anything else would make free_page's
            // get_page short-read the file.
            let wal_has = {
                let wal = self.wal.read();
                wal.as_ref()
                    .map(|s| s.map.contains_key(&id))
                    .unwrap_or(false)
            };
            if !self.cache.read().contains_key(id) && !wal_has && id >= disk_pages {
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

    /// The DEFERRED WAL commit for the concurrent critical section:
    /// appends the frames + commit marker and publishes the committed
    /// page count, but does NOT fsync and does NOT auto-checkpoint —
    /// both are the group-sync leader's job (see [`Pager::group_sync`]).
    /// Returns the durability ticket (the cumulative WAL frame count
    /// this commit covers; `group_sync(ticket)` waits for the fsync).
    fn flush_wal_deferred_concurrent(&self) -> Result<u64> {
        if self.wal.read().is_none() {
            // Journal mode cannot flip while a concurrent txn is open
            // (PRAGMA is rejected), but be total anyway: the DELETE-journal
            // path has no deferral point — run it inline.
            self.flush_inner_delete()?;
            return Ok(self.wal_frames_total());
        }
        let ticket = self.flush_wal_opts(false, false)?;
        self.committed_n_pages
            .store(self.n_pages.load(Ordering::Acquire), Ordering::Release);
        Ok(ticket)
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

    /// Record a row-level write for the armed transaction's journal —
    /// the ROW-granularity refinement of the page-shadow scheme. Called
    /// from the B-tree entry functions (the boundary where the full
    /// semantic row is in hand); a no-op unless a concurrent writer
    /// scope is armed on this pager (one atomic gate load otherwise).
    ///
    /// `root` is the tree root AS THE CALLER SEES IT (the transaction's
    /// current view — its own splits have moved it); it is canonicalized
    /// back to the origin root here so keys match across transactions.
    pub(crate) fn note_row_write(
        &self,
        root: PageId,
        is_index: bool,
        kind: JournalKind,
        rowid: i64,
        key: &[u8],
        payload: &[u8],
    ) {
        let Some(txn_id) = self.armed_writer_scope() else {
            return; // plain-write path: no journal, zero overhead
        };
        if let Some(mut guard) = self.concurrent.get(txn_id) {
            if let Some(txn) = guard.get_mut(&txn_id) {
                let canonical = txn.canonical_root(root);
                txn.row_journal.push(JournalOp {
                    root: canonical,
                    is_index,
                    kind,
                    rowid,
                    key: if is_index { key.to_vec() } else { Vec::new() },
                    payload: if is_index {
                        Vec::new()
                    } else {
                        payload.to_vec()
                    },
                });
                txn.op_ok.push(true);
            }
        }
    }

    /// Record the OUTCOME of the just-executed update/delete class op
    /// (parallel to the journal entry pushed by the matching
    /// `note_row_write` call): did the row exist / was the op applied.
    /// The merge's replay must reproduce these outcomes exactly — a
    /// divergence aborts the merge (retry) rather than guessing.
    pub(crate) fn note_row_outcome(&self, ok: bool) {
        let Some(txn_id) = self.armed_writer_scope() else {
            return;
        };
        if let Some(mut guard) = self.concurrent.get(txn_id) {
            if let Some(txn) = guard.get_mut(&txn_id) {
                if let Some(last) = txn.op_ok.last_mut() {
                    *last = ok;
                }
            }
        }
    }

    /// Invalidate the armed transaction's row journal: an entry function
    /// failed mid-operation (or produced an outcome this boundary cannot
    /// attribute), so the journal may no longer describe the page shadows
    /// exactly. The commit falls back to PAGE-granularity conflict
    /// semantics (any conflicting dirty page aborts) — never a merge
    /// replayed from a possibly-divergent journal.
    pub(crate) fn note_journal_invalidated(&self) {
        let Some(txn_id) = self.armed_writer_scope() else {
            return;
        };
        if let Some(mut guard) = self.concurrent.get(txn_id) {
            if let Some(txn) = guard.get_mut(&txn_id) {
                txn.row_journal.clear();
                txn.op_ok.clear();
            }
        }
    }

    /// Record a root split performed by the armed transaction
    /// (`new_root` now sits above `old_root`). Page 0 is the permanent
    /// catalog root and never moves — its lineage entry is skipped.
    pub(crate) fn note_root_split(&self, old_root: PageId, new_root: PageId) {
        if old_root == 0 || new_root == 0 {
            return;
        }
        let Some(txn_id) = self.armed_writer_scope() else {
            return; // plain-mode splits need no lineage (no journal)
        };
        if let Some(mut guard) = self.concurrent.get(txn_id) {
            if let Some(txn) = guard.get_mut(&txn_id) {
                txn.root_lineage.insert(new_root, old_root);
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
