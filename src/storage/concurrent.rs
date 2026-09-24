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
//! - WAL journal mode on a file-backed database is required (shared
//!   in-memory engines run the regime too — the page cache IS the
//!   committed state, so the commit flush is a no-op).
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

/// One `SAVEPOINT` level inside a concurrent transaction. The undo
/// discipline mirrors the plain pager's savepoints (`SavepointLevel`):
/// a page's pre-image is captured at FETCH time — the bytes at fetch are
/// the pre-mutation state, and the first fetch after the savepoint's
/// creation carries exactly the savepoint-time bytes (a page cannot be
/// mutated without being fetched first).
///
/// Everything this level must restore is PRIVATE to the transaction
/// (shadow pages, allocation/freelist intents, the row journal, the
/// root-split lineage) — the live page cache and the shared freelist are
/// untouched, so a concurrent-txn savepoint never interacts with another
/// connection's state.
#[derive(Debug, Clone)]
pub(crate) struct ConcurrentSavepoint {
    name: String,
    /// Shadow page id -> bytes as of this savepoint's creation (captured
    /// at first fetch after creation; `or_insert` keeps the FIRST capture
    /// = the savepoint-time state).
    undo: HashMap<PageId, Vec<u8>>,
    /// Mark into the transaction's `allocated` vector: pages past it were
    /// allocated AFTER this savepoint and are DROPPED (never restored) by
    /// `ROLLBACK TO` — they move to `discarded` for end-of-transaction
    /// freelist recycling.
    allocated_mark: usize,
    /// Mark into `freed`: tree-surgery frees after this savepoint are
    /// undone (the pages are live tree members again).
    freed_mark: usize,
    /// Mark into the row journal (`row_journal` + `op_ok`, parallel
    /// arrays): journal ops past it describe writes being undone.
    journal_mark: usize,
    /// Root-split lineage snapshot (splits are rare — a small map). A
    /// split performed after this savepoint is undone with the pages.
    lineage_snap: HashMap<PageId, PageId>,
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
    /// Global commit epoch at BEGIN. Pages whose stamps stay within it
    /// materialize the strict BEGIN-time snapshot; a page whose stamp
    /// exceeds it is a LATE touch (a sibling commit landed after BEGIN)
    /// — materialized from the newest committed bytes with that stamp
    /// as the shadow's base (see `writer_shadow_fetch`).
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
    /// SAVEPOINT stack (SQLite semantics: named, nested, last-wins on
    /// name lookup). See [`ConcurrentSavepoint`].
    pub(crate) savepoints: Vec<ConcurrentSavepoint>,
    /// Page ids allocated by this transaction and then DROPPED by a
    /// `ROLLBACK TO SAVEPOINT` (post-savepoint allocations whose tree
    /// links were undone). They are unreferenced garbage in the
    /// transaction's private view: COMMIT splices them into the live
    /// freelist alongside `freed` (the flush publishes the page-count
    /// high-water, so the ids exist in the committed file), and a full
    /// ROLLBACK recycles them alongside `allocated`.
    pub(crate) discarded: Vec<PageId>,
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
            savepoints: Vec::new(),
            discarded: Vec::new(),
        }
    }

    /// True when `id` was already part of the transaction's page state at
    /// the savepoint `sp`'s creation: fetched from committed state (below
    /// the BEGIN page count) or allocated before the savepoint's
    /// allocation mark. Per-transaction allocation ids are strictly
    /// ascending (monotonic `n_pages` counter under the engine statement
    /// lock), so `allocated[mark-1]` is the boundary.
    fn existed_at(&self, sp: &ConcurrentSavepoint, id: PageId) -> bool {
        id < self.begin_n_pages
            || (sp.allocated_mark > 0
                && self
                    .allocated
                    .get(sp.allocated_mark - 1)
                    .is_some_and(|&last| id <= last))
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

/// Shard count for the transaction registry: concurrent-transaction
/// statements hammer the registry mutex on EVERY page fetch and
/// row-write note (`writer_shadow_fetch` & co), so N parallel writers
/// on one shared `Arc<Database>` must not serialize on a single mutex.
/// Txn ids are sequential (`fetch_add`), so `id % N` spreads
/// simultaneous transactions across distinct shards (measured: the
/// 4-parallel-writer shape's statement phase ran ~9x its single-writer
/// time on one mutex; sharding restores true overlap).
const TXN_SHARDS: usize = 16;

/// One registry shard, padded to its own cache line: 16 unpadded
/// parking_lot Mutexes pack into 2 lines, and simultaneous writers
/// (sequential txn ids → adjacent shards) would false-share them —
/// every lock/unlock bounces the line across cores and the "sharded"
/// registry serializes anyway (measured on the 4-writer phase probe).
#[repr(align(64))]
struct TxnShard(Mutex<HashMap<u64, ConcurrentTxn>>);

/// Registry + version-stamp store for the concurrent regime.
///
/// One instance lives inside each [`Pager`]. All methods take `&self`
/// (interior mutability) because the pager is shared by reader threads.
pub struct ConcurrentManager {
    /// Open transactions, SHARDED BY TXN ID (see `TXN_SHARDS`): one
    /// mutex would serialize every parallel writer's page fetches.
    txns: [TxnShard; TXN_SHARDS],
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
    /// ANY known root of a tree → its ULTIMATE ORIGIN root (the reverse of
    /// `live_roots`, covering every intermediate split root). Row-stamp
    /// keys and root-view validation fold through this map so that
    /// transactions with DIFFERENT (stale vs current) root views of the
    /// SAME tree agree on the key: a row stamp published under the current
    /// root is visible to a validator holding an intermediate view, and
    /// vice versa. Without the fold, two overlapping transactions whose
    /// views straddled a split could both install/merge the SAME rowid —
    /// a duplicate cell that breaks ordered walks (mass DELETE skips).
    root_origin: RwLock<HashMap<PageId, PageId>>,
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
    /// COMMIT/ROLLBACK critical sections in flight. `take()` drops the
    /// committing transaction from `active` BEFORE the install + WAL
    /// append run — for the LAST open transaction that flips the regime
    /// gate OFF mid-commit, and a plain reader (no reader gate) could
    /// straddle the multi-page install (a torn tree), or a plain writer
    /// could be admitted mid-install. The commit/rollback critical
    /// sections hold this counter for their whole duration so
    /// [`Self::any_active`] stays true until the install + flush land.
    /// Serialized by `commit_lock`, so the counter is 0 or 1 in practice.
    committing: AtomicUsize,
    /// Number of entries in `live_roots` (one relaxed atomic load — the
    /// read path's fold gate). A concurrent commit that moved a tree root
    /// leaves an entry; the engine's root resolution then folds catalog
    /// roots through origin→current so readers can never descend a
    /// stale root (a truncated tree) after a merge re-rooted it.
    roots_moved: AtomicUsize,
    /// Reader-vs-install exclusion for the `&self` API world (a shared
    /// `Arc<Database>` with NO outer RwLock — `begin_concurrent_transaction`
    /// and `prepare`/`step` on `&self`). A concurrent COMMIT installs its
    /// shadow pages into the live cache ONE PAGE AT A TIME under the
    /// page-cache write lock: per-FETCH atomicity holds, but a plain
    /// reader's MULTI-page walk could otherwise straddle the install
    /// (root fetched before the install, leaf after) and observe a torn
    /// tree. Installers hold the WRITE side across the whole
    /// install+freelist-splice window; plain (no-transaction) readers
    /// hold the READ side for their whole walk (see
    /// [`Pager::plain_reader_gate`]). Owners are exempt by construction
    /// — their fetch-time version-stamp gate fails fast on any page a
    /// sibling commit installed — and the outer-RwLock worlds (the
    /// driver, the C ABI, `execute(&mut self)`) never overlap a reader
    /// with a commit in the first place, so the gate stays uncontended
    /// there.
    install_gate: RwLock<()>,
}

/// RAII decrement for [`ConcurrentManager::commit_window`] (unwind-safe).
pub(crate) struct CommitWindowGuard<'a>(&'a ConcurrentManager);

impl Drop for CommitWindowGuard<'_> {
    fn drop(&mut self) {
        self.0.committing.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ConcurrentManager {
    pub(crate) fn new() -> Self {
        Self {
            txns: std::array::from_fn(|_| TxnShard(Mutex::new(HashMap::new()))),
            stamps: RwLock::new(HashMap::new()),
            row_stamps: RwLock::new(HashMap::new()),
            live_roots: RwLock::new(HashMap::new()),
            root_origin: RwLock::new(HashMap::new()),
            commit_epoch: AtomicU64::new(1),
            next_txn: AtomicU64::new(1),
            commit_lock: Mutex::new(()),
            scope_count: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            committing: AtomicUsize::new(0),
            roots_moved: AtomicUsize::new(0),
            install_gate: RwLock::new(()),
        }
    }

    /// Open a transaction. `begin_n_pages` / `begin_epoch` are captured
    /// by the caller (the pager) at BEGIN time.
    pub(crate) fn begin(&self, begin_epoch: u64, begin_n_pages: u32) -> u64 {
        let id = self.next_txn.fetch_add(1, Ordering::Relaxed);
        let txn = ConcurrentTxn::new(id, begin_epoch, begin_n_pages);
        self.txns[id as usize % TXN_SHARDS].0.lock().insert(id, txn);
        self.active.fetch_add(1, Ordering::AcqRel);
        id
    }

    pub(crate) fn take(&self, id: u64) -> Option<ConcurrentTxn> {
        let removed = self.txns[id as usize % TXN_SHARDS].0.lock().remove(&id);
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
        let g = self.txns[id as usize % TXN_SHARDS].0.lock();
        if g.contains_key(&id) {
            Some(g)
        } else {
            None
        }
    }

    /// True when at least one concurrent transaction is open OR a
    /// commit/rollback critical section is mid-flight (the regime gate:
    /// plain writers must wait, readers pass freely but arm the
    /// reader gate). Single atomic loads — this is on `get_page`'s hot
    /// path.
    pub fn any_active(&self) -> bool {
        self.active.load(Ordering::Acquire) > 0 || self.committing.load(Ordering::Acquire) > 0
    }

    /// True when at least one committed root move is known (the engine's
    /// root-resolution fold gate — see `roots_moved`). One relaxed load.
    pub fn any_root_moves(&self) -> bool {
        self.roots_moved.load(Ordering::Acquire) > 0
    }

    /// Enter a COMMIT/ROLLBACK critical section: keeps [`Self::any_active`]
    /// true across the whole install + flush window (see `committing`).
    /// RAII — the guard decrements on every exit including unwinds.
    pub(crate) fn commit_window(&self) -> CommitWindowGuard<'_> {
        self.committing.fetch_add(1, Ordering::AcqRel);
        CommitWindowGuard(self)
    }

    /// Number of open concurrent transactions (diagnostics).
    pub fn active_count(&self) -> usize {
        self.txns.iter().map(|s| s.0.lock().len()).sum()
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
    /// (ORIGIN root, rowid) — the origin fold makes the key independent of
    /// which root view the committer held (see `root_origin`).
    fn stamp_rows(&self, ops: impl Iterator<Item = (PageId, i64)>, epoch: u64) {
        let mut rows = self.row_stamps.write();
        for (root, rowid) in ops {
            let origin = self.origin_of(root);
            rows.insert((origin, rowid), epoch);
        }
    }

    /// Committed row stamp of (ORIGIN root, rowid). 0 = never written
    /// by a concurrent commit. The origin fold applies on BOTH sides
    /// (stamp and lookup), so views that straddle a split agree.
    pub(crate) fn row_stamp(&self, root: PageId, rowid: i64) -> u64 {
        let origin = self.origin_of(root);
        self.row_stamps
            .read()
            .get(&(origin, rowid))
            .copied()
            .unwrap_or(0)
    }

    /// Fold `root` to its ultimate ORIGIN root: the row-stamp key space
    /// and root-view checks are view-independent through this fold.
    pub(crate) fn origin_of(&self, root: PageId) -> PageId {
        self.root_origin.read().get(&root).copied().unwrap_or(root)
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
    /// origin and points it at `new`. Also maintains the REVERSE map
    /// (`root_origin`): every root in the chain folds to the ultimate
    /// origin, so row-stamp keys and root-view checks are view-independent.
    fn install_lineage(&self, lineage: &HashMap<PageId, PageId>) {
        if lineage.is_empty() {
            return;
        }
        let mut live = self.live_roots.write();
        let mut origin = self.root_origin.write();
        for (&new_root, &old_root) in lineage.iter() {
            // Chase the old root's origin through the SAME lineage batch
            // (entries are in split order, so the chain resolves).
            let mut o = old_root;
            for _ in 0..64 {
                match lineage.get(&o) {
                    Some(&older) if older != o => o = older,
                    _ => break,
                }
            }
            // Fold through any previously-committed lineage too.
            let ultimate = origin.get(&o).copied().unwrap_or(o);
            origin.insert(new_root, ultimate);
            origin.insert(old_root, ultimate);
            origin.insert(ultimate, ultimate);
            live.insert(ultimate, new_root);
        }
        // Publish the fold gate for the engine's root resolution (see
        // `roots_moved`).
        self.roots_moved.store(live.len(), Ordering::Release);
    }

    pub(crate) fn note_scope_armed(&self) {
        self.scope_count.fetch_add(1, Ordering::AcqRel);
    }

    /// The concurrent manager's committed origin → current-root map
    /// (the authoritative tree-root view for the concurrent regime —
    /// diagnostics for `RSQL_DBG_CW` root-resolution tracing).
    pub(crate) fn dbg_live_roots(&self) -> Vec<(PageId, PageId)> {
        self.live_roots
            .read()
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect()
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

/// SAVEPOINT undo capture for a shadow-page fetch (the mirror of the
/// plain pager's `capture_savepoint_undo`, operating on the transaction's
/// PRIVATE pages instead of the live cache). Called from
/// `writer_shadow_fetch` while savepoint levels are open.
///
/// Discipline: the bytes at fetch time are the pre-mutation state, and
/// `or_insert` keeps each level's FIRST capture — which is exactly the
/// savepoint-time state, because a page cannot be mutated without being
/// fetched first. The newest-level fast exit is sound because capture
/// fills EVERY eligible level in one pass (eligibility is monotone: a
/// page that existed at a newer level's creation existed at every older
/// level's creation too).
fn capture_shadow_undo(txn: &mut ConcurrentTxn, id: PageId, pr: &PageRef) {
    // Fast exit: the newest level already has this page (the common
    // re-fetch loop on a hot page).
    if txn
        .savepoints
        .last()
        .is_some_and(|s| s.undo.contains_key(&id))
    {
        return;
    }
    // A page needs a pre-image in a level only if it existed at that
    // level's creation: fetched from committed state (below the BEGIN
    // page count) or allocated before the level's mark. Post-savepoint
    // allocations are DROPPED by ROLLBACK TO, never restored — no
    // pre-image needed (a bulk-INSERT window between savepoints then
    // costs ~0 undo bytes, same optimization as the plain pager).
    let elig: Vec<bool> = txn
        .savepoints
        .iter()
        .map(|s| txn.existed_at(s, id))
        .collect();
    if !elig.iter().any(|&e| e) {
        return;
    }
    let bytes = pr.lock().data.clone();
    for (s, e) in txn.savepoints.iter_mut().zip(elig) {
        if e {
            s.undo.entry(id).or_insert_with(|| bytes.clone());
        }
    }
}

/// RAII guard returned by [`Pager::arm_writer_scope`]. Restores the
/// previous TLS value on drop (re-entrancy safe) and keeps the
/// manager's gate count balanced. Must be held for the duration of one
/// statement (or one API call) executing on behalf of the transaction.
///
/// A NO-OP variant (see [`Pager::arm_writer_scope`]) covers the
/// re-arm of the SAME transaction this thread already serves: the TLS
/// already routes correctly, so the guard touches NO shared state —
/// per-statement counter bumps + epoch bumps + gate stores on one
/// cache line measured as the parallel-write path's dominant slowdown
/// (every writer ping-ponged the same lines once per statement).
#[must_use]
pub struct WriterScopeGuard<'a> {
    prev: Option<(u64, u64)>,
    pager: &'a Pager,
    noop: bool,
}

impl Drop for WriterScopeGuard<'_> {
    fn drop(&mut self) {
        if self.noop {
            return;
        }
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
    /// file-backed — OR the store is an in-memory engine, where the
    /// page cache IS the committed state (installs land directly; the
    /// flush is the same no-op a plain memory commit performs). Returns
    /// the transaction id the engine uses for statement routing and
    /// COMMIT/ROLLBACK.
    pub fn begin_concurrent(&self) -> Result<u64> {
        let memory = self.is_memory();
        if !memory {
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
        }
        // The committed page count as of BEGIN. Plain commits keep this
        // in sync at every flush; concurrent installs update it at their
        // own flush. In-memory engines never flush (the cache IS the
        // committed state) so the counter would lag every allocation —
        // but a regime can only open while NO plain transaction is
        // active (checked above) and plain `&self` DML is excluded by
        // the shared-Database tripwire, so at this instant every live
        // page is committed and `n_pages` IS the committed bound: sync
        // the counter once at regime open, and the per-commit publish
        // in `flush_wal_deferred_concurrent` keeps it fresh afterward.
        let begin_n_pages = if memory {
            let n = self.n_pages.load(Ordering::Acquire);
            self.committed_n_pages.store(n, Ordering::Release);
            n
        } else {
            self.committed_n_pages.load(Ordering::Acquire)
        };
        let begin_epoch = self.concurrent.current_epoch();
        self.note_tx_begun();
        let id = self.concurrent.begin(begin_epoch, begin_n_pages);
        self.refresh_concurrent_gate();
        Ok(id)
    }

    /// Arm this thread's writer scope for `txn_id` on THIS pager.
    /// `get_page` then serves the transaction's shadows; `allocate_page`
    /// / `free_page` / `note_dirty` route into the transaction.
    ///
    /// RE-ARM MEMO: when the thread's scope already serves THIS
    /// transaction on THIS pager, return a no-op guard — the routing is
    /// already correct, and skipping the shared-counter bump + epoch
    /// bump + gate store keeps those cache lines quiet under N parallel
    /// writers (the per-statement arm pair was the parallel-write
    /// path's dominant serialization). The scope then stays armed
    /// BETWEEN this thread's statements of the same transaction —
    /// correct (an owner reading between its own statements must see
    /// its own shadows) — and is disarmed at the transaction's END
    /// (see [`Pager::disarm_writer_scope_if`], called by the
    /// commit/rollback paths) or replaced by the next arm of a
    /// different transaction.
    pub fn arm_writer_scope(&self, txn_id: u64) -> WriterScopeGuard<'_> {
        let cur = WRITER_SCOPE.with(|c| c.get());
        if cur == Some((self.instance_id, txn_id)) {
            return WriterScopeGuard {
                prev: None,
                pager: self,
                noop: true,
            };
        }
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
        WriterScopeGuard {
            prev,
            pager: self,
            noop: false,
        }
    }

    /// Disarm this thread's writer scope when it serves `txn_id` — the
    /// transaction's END (COMMIT/ROLLBACK/abort teardown): with the
    /// re-arm memo the scope stays armed between the owner thread's
    /// statements, so the end must clear it explicitly (a fetch after
    /// the registry take would otherwise route into a vanished
    /// transaction). No-op when this thread serves a different
    /// transaction or none.
    pub fn disarm_writer_scope_if(&self, txn_id: u64) {
        let matched = WRITER_SCOPE.with(|c| {
            if c.get() == Some((self.instance_id, txn_id)) {
                c.set(None);
                true
            } else {
                false
            }
        });
        if matched {
            self.concurrent.note_scope_dropped();
            self.refresh_concurrent_gate();
        }
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

    /// Reserve `n` consecutive automatic-rowid ids for a CONCURRENT-regime
    /// statement: one CAS per block against the global high-water (see
    /// [`Self::concurrent_rowid_hw`]). `view_max` is the table's max rowid
    /// in the caller's snapshot; the returned base is strictly greater than
    /// both it and every id ever reserved on this pager, so sibling
    /// transactions' draws are DISJOINT — their commits MERGE on shared
    /// hot leaves instead of dying to the row-stamp first-committer-wins
    /// check. AUTOINCREMENT semantics: reserved-but-unused ids are never
    /// handed out again (rollback leaves gaps, exactly like SQLite's
    /// sqlite_sequence). Fails with SQLITE_FULL semantics when the rowid
    /// space is exhausted (the block is capped at the last usable id; the
    /// next draw past it fails like `next_auto_rowid`'s overflow path).
    pub fn reserve_rowids(&self, view_max: i64, n: i64) -> Result<i64> {
        debug_assert!(n >= 1);
        let hw = &self.concurrent_rowid_hw;
        loop {
            let cur = hw.load(Ordering::Acquire);
            let base = cur
                .max(view_max)
                .checked_add(1)
                .ok_or_else(|| Error::constraint("database or disk is full"))?;
            if base == i64::MAX {
                return Err(Error::constraint("database or disk is full"));
            }
            // Cap the block at the last usable id (a request that would
            // overflow gets a short block; the next draw fails cleanly).
            let last = (base + n - 1).min(i64::MAX - 1);
            match hw.compare_exchange(cur, last, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(base),
                Err(_) => continue, // a sibling drew first — retry above it
            }
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
    ///
    /// AUTHORITATIVE read (`ConcurrentManager::any_active`): this feeds
    /// `finish_concurrent_txn` (the engine's regime-lifetime decision) and
    /// the COUNT(*) memo bypass. The combined `concurrent_gate` fast-path
    /// word is only a HINT — its bit0 can be transiently stale-zero when
    /// two threads' `refresh_concurrent_gate` stores interleave (a plain
    /// read-then-store lost update). A stale-zero here once collapsed the
    /// regime while transactions were still open: the owners' subsequent
    /// INSERTs then routed to the PLAIN autocommit path, wrote the live
    /// cache mid-regime, and clobbered concurrent installs (lost and
    /// phantom rows). One atomic load, off the per-row hot path.
    pub fn concurrent_writers_active(&self) -> bool {
        self.concurrent.any_active()
    }

    /// Reader-side half of the concurrent-install gate (see
    /// [`ConcurrentManager`]'s `install_gate`): a read guard held for
    /// the duration of a PLAIN (no-transaction) reader's page walks
    /// while the concurrent regime is active, so a concurrent COMMIT's
    /// shadow install cannot interleave mid-walk (a torn tree). Returns
    /// `None` — one atomic load, nothing held — when no concurrent
    /// transaction is open. Callers that hold a concurrent transaction
    /// (owners) must NOT take this guard: their fetch-time version-stamp
    /// validation is the isolation mechanism, and an owner's statement
    /// may legitimately run while a sibling commits.
    pub fn plain_reader_gate(&self) -> Option<parking_lot::RwLockReadGuard<'_, ()>> {
        if !self.concurrent.any_active() {
            return None;
        }
        Some(self.concurrent.install_gate.read())
    }

    /// True when the transaction has no observable effect (no dirty
    /// shadows, no row-journal ops, no frees): the IMPLICIT-JOIN tail
    /// routes such statements to ROLLBACK instead of COMMIT — a no-op
    /// commit would still dirty page 0 + append a WAL commit marker.
    pub fn concurrent_txn_is_noop(&self, txn_id: u64) -> bool {
        let guard = match self.concurrent.get(txn_id) {
            Some(g) => g,
            None => return true, // not open: nothing to commit
        };
        let Some(txn) = guard.get(&txn_id) else {
            return true;
        };
        let has_dirty_shadow = txn.shadows.values().any(|pr| pr.lock().dirty);
        !has_dirty_shadow
            && txn.row_journal.is_empty()
            && txn.freed.is_empty()
            && txn.discarded.is_empty()
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
        // Commit window: the body's `take` drops the transaction from
        // `active` BEFORE the install + WAL append run — for the LAST
        // open transaction that would flip the regime gate OFF
        // mid-commit (torn plain readers; plain writers admitted
        // mid-install). Hold any_active true for the whole critical
        // section, then refresh the stored gate AFTER the window drops:
        // a refresh inside the window would store the window's ACTIVE
        // bit and wedge every plain writer on BUSY forever once the
        // window ends.
        let out = {
            let _window = self.concurrent.commit_window();
            self.commit_concurrent_body(txn_id)
        };
        self.refresh_concurrent_gate();
        out
    }

    fn commit_concurrent_body(&self, txn_id: u64) -> Result<(ConcurrentCommitOutcome, u64)> {
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
        // Snapshot isolation anchored at each shadow's BASE: pages
        // materialized before any sibling commit carry the BEGIN-time
        // bytes (base stamp <= begin_epoch), and pages first touched
        // after one serve the newest committed bytes with the moved
        // stamp as their base (see `writer_shadow_fetch` — the parallel
        // regime's hot-page refinement: such a shadow already contains
        // every sibling commit, so installing it cannot lose them).
        // Either way `read_stamps` records the base, and only
        // WRITE-WRITE overlaps need commit-time validation: a dirty
        // shadow page whose committed stamp moved SINCE ITS BASE means
        // another connection committed a write to the same page. That
        // is the END of the story at PAGE granularity — but ROW
        // granularity refines it: if the row journal fully describes
        // this transaction's writes and none of those (tree, rowid)
        // pairs were re-stamped by the concurrent commit, the
        // transaction MERGES (replays its journal onto the current
        // trees) instead of retrying.
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
        // ---- ROOT-VIEW validation: the fetch-time stamp gate protects
        // PAGES, not tree ROOTS. A sibling commit may have installed a new
        // root ABOVE this transaction's view (its descent split the same
        // subtree); this transaction's later statements then descend a
        // subtree that is no longer the whole tree, and installing its
        // shadows would graft a PRIVATE root over the committed one —
        // forking the tree, with rows stranded behind unreachable
        // subtrees and the catalog's rootpage flip-flopping between
        // competing views. The view is current iff the journal's
        // (origin-folded) canonical root equals the tree's CURRENT root;
        // a mismatch is a conflict of the same class as a moved page
        // stamp: route it to the SAME resolution (row validation →
        // MERGE replays the journal onto the CURRENT roots, or abort).
        for op in &txn.row_journal {
            if op.root == 0 {
                continue; // the catalog root is fixed at page 0
            }
            let origin = self.concurrent.origin_of(op.root);
            if self.concurrent.current_root_of(origin) != op.root && !conflicts.contains(&op.root) {
                conflicts.push(op.root);
            }
        }
        // ---- ROW-STAMP validation on the FAST path: a sibling commit may
        // have written the same (tree, rowid) into a DIFFERENT leaf — a
        // leaf split between the two transactions' snapshots moves the
        // row's home page, so both transactions' dirty page sets are
        // DISJOINT and the page/root checks above pass for both. Without
        // this check both install, grafting DUPLICATE rowid cells into the
        // tree (duplicate keys break ordered walks — a mass DELETE skips
        // the whole range). The row stamp is the authority at ROW
        // granularity: first committer wins; later committers route to
        // the conflict resolution, whose row validation aborts them with
        // a retriable 517.
        if conflicts.is_empty() {
            for op in &txn.row_journal {
                if self.concurrent.row_stamp(op.root, op.rowid) > txn.begin_epoch {
                    conflicts.push(if op.root == 0 { 0 } else { op.root });
                    break;
                }
            }
        }
        if !conflicts.is_empty() {
            if std::env::var_os("RSQL_DBG_FLUSH").is_some() {
                eprintln!(
                    "[CONFLICT] pages {:?} journal_ops={} → merge",
                    conflicts,
                    txn.row_journal.len()
                );
            }
            // Any resolution of a page conflict (merge or abort) discards
            // the page shadows, so this transaction's fresh allocations —
            // including savepoint-discarded ones — are garbage either way:
            // recycle them up front.
            let mut recycle = txn.allocated.clone();
            recycle.extend_from_slice(&txn.discarded);
            self.splice_txn_freed(&recycle)?;
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
        // valid). The install gate's WRITE side spans the whole
        // install+splice window: a plain reader's walk (holding the READ
        // side, see `Pager::plain_reader_gate`) observes the tree either
        // fully pre- or fully post-install — never the interleaved
        // middle.
        let mut freed_now = txn.freed.clone();
        freed_now.extend_from_slice(&txn.discarded);
        let installed = {
            let _ig = self.concurrent.install_gate.write();
            // Publish the root moves BEFORE the install lands: the
            // moment the install's pages become visible, plain readers
            // resolve roots — the origin→current fold must already be
            // published (roots_moved) or a reader between the install
            // and a later install_lineage would descend the STALE root
            // into a truncated tree (the reader-atomicity probe's
            // one-leaf torn reads).
            self.concurrent.install_lineage(&txn.root_lineage);
            let installed = self.install_txn_shadows(&txn);
            self.splice_txn_freed(&freed_now)?;
            installed
        };
        // Page-0 header refresh rides the flush (n_pages high-water,
        // freelist, cookie) — the standard flush_wal path does it.

        // ---- WAL commit: frames for every installed page + splice
        // writes + header, one commit marker. The fsync is deferred to
        // group_sync (ticket).
        let ticket = self.flush_wal_deferred_concurrent()?;

        // ---- publish version stamps for the installed pages + rows.
        let epoch = self.concurrent.stamp_installed(installed.into_iter());
        self.concurrent
            .stamp_rows(txn.row_journal.iter().map(|o| (o.root, o.rowid)), epoch);
        // (install_lineage already ran INSIDE the install gate above.)

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
        if std::env::var_os("RSQL_DBG_FLUSH").is_some() && !installed.is_empty() {
            eprintln!("[INSTALL] pages {:?}", installed);
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
                // Phase 3 — catalog convergence: roots moved by THE
                // REPLAY ITSELF (or by earlier commits, visible only
                // through the manager's origin→current map) have NO
                // journal op to patch — the original transaction's
                // statements never moved them, so no catalog op exists.
                // Without this, the durable schema row keeps the stale
                // rootpage forever: the merged tree installs fine, but a
                // REOPEN resolves the old root and strands every row
                // behind the split (found by the 4-parallel-writers
                // probe: in-memory perfect, reopen 262 of 1000).
                .and_then(|()| self.patch_catalog_rootpages())
        };
        let fresh = match self.concurrent.take(fresh_id) {
            Some(t) => t,
            None => {
                // Unreachable (we just created it), but be total.
                return Err(page_conflict());
            }
        };
        if std::env::var_os("RSQL_DBG_FLUSH").is_some() {
            let dirty: Vec<PageId> = fresh
                .shadows
                .iter()
                .filter(|(_, p)| p.lock().dirty)
                .map(|(id, _)| *id)
                .collect();
            let clean: Vec<PageId> = fresh
                .shadows
                .iter()
                .filter(|(_, p)| !p.lock().dirty)
                .map(|(id, _)| *id)
                .collect();
            let detail: Vec<String> = fresh
                .shadows
                .iter()
                .map(|(id, p)| {
                    let b = p.lock();
                    format!("p{}:type={:?}:cells={}", id, b.page_type(), b.n_cells())
                })
                .collect();
            eprintln!(
                "[MERGE-END] dirty={:?} clean={:?} allocated={:?} lineage={:?} shadows=[{}]",
                dirty,
                clean,
                fresh.allocated,
                fresh.root_lineage,
                detail.join(", ")
            );
        }
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

        // ---- install the replayed state through the standard path
        // (under the install gate: same plain-reader exclusion contract
        // as the fast path above).
        let installed = {
            let _ig = self.concurrent.install_gate.write();
            // Publish the replay's root moves BEFORE the install lands
            // (same rationale as the fast path: plain readers resolve
            // roots the moment the pages become visible — the fold must
            // already be published).
            self.concurrent.install_lineage(&fresh.root_lineage);
            let installed = self.install_txn_shadows(&fresh);
            self.splice_txn_freed(&fresh.freed)?;
            installed
        };
        let ticket = self.flush_wal_deferred_concurrent()?;
        let epoch = self.concurrent.stamp_installed(installed.into_iter());
        // The fresh transaction's journal re-recorded the replayed ops
        // (with keys canonicalized against ITS lineage) — stamping those
        // keys publishes exactly the rows the merged commit wrote.
        self.concurrent
            .stamp_rows(fresh.row_journal.iter().map(|o| (o.root, o.rowid)), epoch);
        // (install_lineage already ran INSIDE the install gate above.)

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
                let origin = self.concurrent.origin_of(c);
                let current = self.concurrent.current_root_of(origin);
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
            view_roots.get(&op.root).copied().unwrap_or_else(|| {
                self.concurrent
                    .current_root_of(self.concurrent.origin_of(op.root))
            })
        };
        let post_root = if op.is_index {
            let mut bt = Btree::new(self, root, true);
            match op.kind {
                JournalKind::Insert => {
                    // Duplicate guard: the current tree must NOT already
                    // hold this (key, rowid) — a validation gap would
                    // otherwise graft a duplicate index entry. Divergence
                    // aborts the merge (the caller surfaces a retriable
                    // 517) instead of corrupting the index.
                    let existing = bt.lookup_index(&op.key)?;
                    if existing.contains(&op.rowid) {
                        return Err(Error::Transaction(format!(
                            "index entry ({}, {}) already present during replay",
                            op.rowid, op.root
                        )));
                    }
                    bt.insert_index(&op.key, op.rowid)?
                }
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
                JournalKind::Insert => {
                    // Duplicate guard: the current tree must NOT already
                    // hold this rowid — a validation gap would otherwise
                    // graft a duplicate cell (duplicate keys break ordered
                    // walks: a mass DELETE skips the whole range). Divergence
                    // aborts the merge (retriable 517), never corrupts.
                    use crate::storage::btree::LookupResult;
                    if matches!(bt.lookup_table(op.rowid)?, LookupResult::Found(_)) {
                        return Err(Error::Transaction(format!(
                            "row {} already present during replay",
                            op.rowid
                        )));
                    }
                    bt.insert_table(op.rowid, &op.payload)?
                }
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

    /// Converge EVERY durable catalog (schema) row to the manager's
    /// current committed roots — the merge's phase 3 (see
    /// [`Self::resolve_page_conflict`]).
    ///
    /// Root moves enter the durable schema row through two channels: an
    /// original transaction's own in-shadow schema rewrite (the statement
    /// epilogue's root sync, journaled as a catalog op — phase 2 patches
    /// it), or... nothing at all, when the move happened during a MERGE
    /// REPLAY (the journal never describes the replay's own splits) or
    /// in an earlier commit whose catalog op predates a later merge's
    /// re-rooting. This sweep closes every one of those holes at once:
    /// scan the catalog tree (root 0, inside the armed fresh-transaction
    /// scope so the reads and rewrites land in the merge's shadows) and
    /// rewrite each schema row whose rootpage column no longer names its
    /// tree's current root. Idempotent by construction: a row whose
    /// rootpage already resolves to itself is left untouched, so a merge
    /// with no root drift writes nothing.
    fn patch_catalog_rootpages(&self) -> Result<()> {
        use crate::storage::btree::Btree;
        // Pass 1 (read-only): collect the stale rows — (rowid, patched
        // payload). The scan cannot mutate while iterating.
        let mut stale: Vec<(i64, Vec<u8>)> = Vec::new();
        {
            let mut bt = Btree::new(self, 0, false);
            bt.scan_table(|rowid, payload| {
                let Ok(row) = crate::storage::row_codec::decode_row(payload, 5, rowid, None) else {
                    return true; // not schema-shaped — leave alone
                };
                let Some(crate::Value::Integer(rp)) = row.get(3) else {
                    return true;
                };
                let Some(stale_page) = u32::try_from(*rp).ok() else {
                    return true;
                };
                if stale_page == 0 {
                    return true; // root 0 = the schema tree itself
                }
                let internal = stale_page - 1;
                let origin = self.concurrent.origin_of(internal);
                let current = self.concurrent.current_root_of(origin);
                if current != internal {
                    let mut patched = row.clone();
                    patched[3] = crate::Value::Integer(current as i64 + 1);
                    let enc = crate::storage::row_codec::encode_row_aliased(&patched, None);
                    stale.push((rowid, enc));
                }
                true
            })?;
        }
        if stale.is_empty() {
            return Ok(());
        }
        // Pass 2: apply the rewrites — delete + insert preserving the
        // catalog rowid (the engine-level `rewrite_schema_row_root`
        // contract), all through the armed scope's shadows.
        for (rowid, payload) in &stale {
            let mut bt = Btree::new(self, 0, false);
            bt.delete_table(*rowid)?;
            bt.insert_table(*rowid, payload)?;
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
        let current = view_roots.get(&cur).copied().unwrap_or_else(|| {
            self.concurrent
                .current_root_of(self.concurrent.origin_of(cur))
        });
        if current != stale_root {
            if std::env::var_os("RSQL_DBG_FLUSH").is_some() {
                eprintln!(
                    "[PATCH] catalog row: rootpage {} -> {}",
                    stale_root + 1,
                    current + 1
                );
            }
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
        // Commit window (see commit_concurrent_inner): the freelist splice
        // + WAL flush below mutate shared state after the take dropped
        // `active` — same mid-window protection, with the gate refreshed
        // AFTER the window drops.
        let out = {
            let _window = self.concurrent.commit_window();
            self.rollback_concurrent_body(txn_id)
        };
        self.refresh_concurrent_gate();
        out
    }

    fn rollback_concurrent_body(&self, txn_id: u64) -> Result<()> {
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
        // stay live; do NOT recycle them. Post-savepoint discards
        // (ROLLBACK TO victims) are the same kind of garbage.
        let mut recycle = txn.allocated.clone();
        recycle.extend_from_slice(&txn.discarded);
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

    // -----------------------------------------------------------------
    // concurrent regime: SAVEPOINT (transaction-local nested undo)
    // -----------------------------------------------------------------

    /// `SAVEPOINT <name>` inside a concurrent transaction. Creates a
    /// transaction-local savepoint level: shadow-page pre-images are
    /// captured lazily at fetch time (see `writer_shadow_fetch`), and the
    /// level records marks into the transaction's append-only intents
    /// (allocations / frees / row journal) plus a lineage snapshot.
    pub fn savepoint_concurrent(&self, txn_id: u64, name: &str) -> Result<()> {
        let mut guard = self.concurrent.get(txn_id).ok_or_else(|| {
            Error::Transaction(format!("concurrent transaction {} is not open", txn_id))
        })?;
        let Some(txn) = guard.get_mut(&txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        txn.savepoints.push(ConcurrentSavepoint {
            name: name.to_ascii_lowercase(),
            undo: HashMap::new(),
            allocated_mark: txn.allocated.len(),
            freed_mark: txn.freed.len(),
            journal_mark: txn.row_journal.len(),
            lineage_snap: txn.root_lineage.clone(),
        });
        Ok(())
    }

    /// `ROLLBACK TO SAVEPOINT <name>` inside a concurrent transaction:
    /// restore the transaction's PRIVATE state (shadow bytes, allocation
    /// and free intents, row journal, root lineage) to the savepoint. The
    /// savepoint itself stays active with a reset undo log (SQLite
    /// semantics — it can be rolled back to again); levels above it are
    /// discarded. Returns the new savepoint-stack depth, or `None` when no
    /// savepoint with that name exists. The live page cache and every
    /// other connection's state are untouched by construction.
    pub fn rollback_concurrent_savepoint(&self, txn_id: u64, name: &str) -> Result<Option<usize>> {
        let psz = self.page_size();
        let junk: Vec<PageId>;
        let keep_depth: usize;
        let begin_n_pages: u32;
        // (page, restored bytes) for the pre-images applied below — phase 2
        // decides which of them net to NO change against the committed
        // state.
        let mut restored: Vec<(PageId, Vec<u8>)> = Vec::new();
        {
            let mut guard = self.concurrent.get(txn_id).ok_or_else(|| {
                Error::Transaction(format!("concurrent transaction {} is not open", txn_id))
            })?;
            let Some(txn) = guard.get_mut(&txn_id) else {
                return Err(Error::Transaction(format!(
                    "concurrent transaction {} is not open",
                    txn_id
                )));
            };
            let idx = match txn
                .savepoints
                .iter()
                .rposition(|s| s.name == name.to_ascii_lowercase())
            {
                Some(i) => i,
                None => return Ok(None),
            };
            // Discard the levels ABOVE the target (their undo journals die
            // with the work being undone), then take the target's data.
            txn.savepoints.truncate(idx + 1);
            let level = txn.savepoints.pop().expect("idx checked above");
            // Post-savepoint allocations are garbage in the restored view:
            // the tree surgery that linked them is being undone. Split
            // them off for end-of-transaction recycling.
            junk = txn.allocated.split_off(level.allocated_mark);
            // Post-savepoint tree-surgery frees are undone: those pages are
            // live tree members again in the restored view.
            txn.freed.truncate(level.freed_mark);
            // The row journal shrinks with the page state (parallel arrays).
            txn.row_journal.truncate(level.journal_mark);
            txn.op_ok.truncate(level.journal_mark);
            // Root splits after the savepoint are undone with the pages.
            txn.root_lineage = level.lineage_snap.clone();
            // Drop the junk shadows (nothing in the restored view can
            // reference them) and mark them for recycling.
            for &id in &junk {
                txn.shadows.remove(&id);
                txn.discarded.push(id);
            }
            // Restore pre-images: every page fetched since the savepoint
            // was captured at its savepoint-time bytes. A page whose shadow
            // is MISSING was freed mid-window by tree surgery (that free is
            // now undone) — re-create the shadow with the saved bytes so
            // the restored tree's references resolve.
            for (id, bytes) in &level.undo {
                if junk.contains(id) {
                    continue; // post-savepoint allocation — dropped above
                }
                match txn.shadows.get_mut(id) {
                    Some(pr) => {
                        let mut b = pr.lock();
                        if b.data.len() == bytes.len() {
                            b.data.copy_from_slice(bytes);
                            b.dirty = true;
                        } else {
                            return Err(Error::corruption(format!(
                                "concurrent savepoint restore: page {id} size mismatch"
                            )));
                        }
                    }
                    None => {
                        let mut page = crate::storage::page::Page::new(*id, psz);
                        page.data.copy_from_slice(bytes);
                        page.dirty = true;
                        let pr: PageRef = Arc::new(Mutex::new(page));
                        txn.shadows.insert(*id, pr);
                    }
                }
                txn.dirtied.insert(*id);
                restored.push((*id, bytes.clone()));
            }
            // Dirtied-set hygiene: no entry may outlive its shadow.
            txn.dirtied.retain(|id| txn.shadows.contains_key(id));
            // Re-push the savepoint (stays active, undo reset — SQLite
            // semantics). The restored state matches the recorded marks
            // exactly, so the level is re-armed as if just created.
            txn.savepoints.push(ConcurrentSavepoint {
                name: level.name,
                undo: HashMap::new(),
                allocated_mark: level.allocated_mark,
                freed_mark: level.freed_mark,
                journal_mark: level.journal_mark,
                lineage_snap: level.lineage_snap,
            });
            keep_depth = txn.savepoints.len();
            begin_n_pages = txn.begin_n_pages;
        }
        // ---- Phase 2: NET-ZERO page detection (registry lock released —
        // the committed-bytes fetch takes cache/WAL locks).
        //
        // A restored page whose bytes EQUAL the current committed bytes
        // carries NO net write for this transaction: the ROLLBACK TO undid
        // every post-savepoint mutation and the page had no pre-savepoint
        // change (or one that landed byte-identical). Keeping such a
        // shadow DIRTY would false-conflict at COMMIT against a sibling
        // commit that touched the page — the classic case is rolling back
        // the transaction's ONLY write to a hot page. Removing the shadow
        // reverts the page to "never fetched": it installs nothing,
        // validates nothing, and a later fetch re-materializes identical
        // bytes.
        //
        // The comparison is atomic with respect to sibling commits: the
        // engine write lock serializes this whole statement, and the only
        // later commits that can move the page will meet the standard
        // validation for pages this transaction actually still writes.
        let mut removable: Vec<PageId> = Vec::new();
        for (id, bytes) in &restored {
            if *id >= begin_n_pages && *id != 0 {
                continue; // txn-private allocation — no committed image
            }
            if let Ok(committed) = self.fetch_committed_bytes(*id, begin_n_pages) {
                if &committed == bytes {
                    removable.push(*id);
                }
            }
        }
        if !removable.is_empty() {
            if let Some(mut guard) = self.concurrent.get(txn_id) {
                if let Some(txn) = guard.get_mut(&txn_id) {
                    for id in &removable {
                        txn.shadows.remove(id);
                        txn.dirtied.remove(id);
                    }
                }
            }
        }
        // Restored content differs from the pre-rollback shadows: advisory
        // caches (table-leaf hints pinned to shadow PageRefs, per-thread
        // memos) must re-derive.
        self.write_version.fetch_add(1, Ordering::Relaxed);
        Ok(Some(keep_depth))
    }

    /// `RELEASE [SAVEPOINT] <name>` inside a concurrent transaction:
    /// discard the savepoint and everything above it WITHOUT rolling
    /// back. Returns the remaining stack depth (0 = none left), or `None`
    /// when the name is unknown. Note: releasing the OUTERMOST savepoint
    /// does NOT commit here — the transaction was started by
    /// `BEGIN CONCURRENT`, not by the savepoint (SQLite commits on
    /// outermost release only for savepoint-started transactions).
    pub fn release_concurrent_savepoint(&self, txn_id: u64, name: &str) -> Result<Option<usize>> {
        let mut guard = self.concurrent.get(txn_id).ok_or_else(|| {
            Error::Transaction(format!("concurrent transaction {} is not open", txn_id))
        })?;
        let Some(txn) = guard.get_mut(&txn_id) else {
            return Err(Error::Transaction(format!(
                "concurrent transaction {} is not open",
                txn_id
            )));
        };
        let idx = match txn
            .savepoints
            .iter()
            .rposition(|s| s.name == name.to_ascii_lowercase())
        {
            Some(i) => i,
            None => return Ok(None),
        };
        txn.savepoints.truncate(idx);
        Ok(Some(txn.savepoints.len()))
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
    /// the plain [`Pager::flush`] in WAL mode, minus the dirty-count fast
    /// path (installs mark dirty explicitly), plus the
    /// committed-page-count publish; the lazy-writeback (in-memory) fast
    /// path mirrors the plain flush's own no-op.
    fn flush_wal_concurrent(&self) -> Result<()> {
        // In-memory / lazy-writeback engines: nothing to persist — the
        // cache IS the committed state (see `flush`'s twin fast path);
        // publish the committed bound for the regime's snapshot checks.
        if self.lazy_writeback.load(Ordering::Acquire) {
            self.committed_n_pages
                .store(self.n_pages.load(Ordering::Acquire), Ordering::Release);
            return Ok(());
        }
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
        // In-memory / lazy-writeback engines: the page cache IS the
        // committed state — a plain COMMIT no-ops the flush the same way
        // (see `flush`). The install already landed everything in the
        // cache; publish the committed bound (the install may have grown
        // the tree) and hand back the no-op ticket (`group_sync` returns
        // immediately against it — there is nothing to make durable).
        if self.lazy_writeback.load(Ordering::Acquire) {
            self.committed_n_pages
                .store(self.n_pages.load(Ordering::Acquire), Ordering::Release);
            return Ok(self.wal_frames_total());
        }
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
    /// private copy of the COMMITTED bytes on first touch.
    ///
    /// A page whose committed stamp is still within the transaction's
    /// begin epoch materializes the strict BEGIN-time snapshot bytes.
    /// A page whose stamp moved past begin (a sibling connection
    /// committed it after this transaction began — the hot-page case of
    /// the true-parallel-writer regime) cannot be reconstructed at its
    /// BEGIN-time version (installs content-replace the live cache in
    /// place; the WAL keeps only the newest frame per page), so with NO
    /// savepoint open the fetch materializes the page's NEWEST committed
    /// bytes and records that stamp as the shadow's base — reads of such
    /// pages are read-committed, and commit validation anchors on the
    /// base: the shadow installs directly when the page did not move
    /// again (the base already contains every sibling commit — no lost
    /// update is possible) and routes to the row-level MERGE when it
    /// did. With a savepoint open the strict abort remains: a savepoint
    /// undo image must be byte-stable, and post-sibling bytes could
    /// leak a later commit into a ROLLBACK TO SAVEPOINT view.
    pub(crate) fn writer_shadow_fetch(&self, txn_id: u64, id: PageId) -> Result<PageRef> {
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
        // Fast path: shadow hit (fetched before, or freshly allocated).
        if let Some(pr) = txn.shadows.get(&id).cloned() {
            // SAVEPOINT undo capture (only while a savepoint is open): the
            // bytes at fetch time are the pre-mutation state, and the
            // FIRST fetch after the savepoint's creation carries exactly
            // the savepoint-time bytes — a page cannot be mutated without
            // being fetched first. Zero cost on the savepoint-free path.
            if !txn.savepoints.is_empty() {
                capture_shadow_undo(txn, id, &pr);
            }
            return Ok(pr);
        }
        // ── LATE-TOUCH GATE ──────────────────────────────────────────
        // A stamp newer than this transaction's snapshot means a sibling
        // commit installed the page AFTER our BEGIN (the hot-root case:
        // with N parallel writers on one table, EVERY writer's root leaf
        // is a late touch once the first committer lands — the strict
        // gate used to abort all of them, making the parallel regime a
        // retry storm). Materialization then serves the newest committed
        // bytes (see the method doc); with a savepoint open, keep the
        // strict abort (undo images must be byte-stable).
        //
        // PAGE 0 is exempt (its header region is rewritten by every
        // commit, and its schema rows only move with root moves — a
        // page-0 reader has always served the newest committed bytes;
        // a read-committed page 0).
        let stamp = self.concurrent.stamp_of(id);
        let late = stamp > txn.begin_epoch && id != 0;
        if late && !txn.savepoints.is_empty() {
            drop(guard);
            return Err(Error::SnapshotConflict(format!(
                "page {} was committed by another connection after this \
                 transaction began; the transaction must be retried",
                id
            )));
        }
        // Bounds: pages beyond the BEGIN committed count did not exist
        // in this snapshot — EXCEPT for a late touch, where a sibling
        // commit may have grown the tree (splits): the descent through a
        // newest-bytes internal shadow can reach pages the sibling
        // created, which exist in the CURRENT committed state. Bound a
        // late fetch by the current committed page count instead.
        let begin_n_pages = if late {
            self.committed_n_pages
                .load(Ordering::Acquire)
                .max(txn.begin_n_pages)
        } else {
            txn.begin_n_pages
        };
        if id >= begin_n_pages {
            return Err(Error::corruption(format!(
                "page {} out of range (concurrent snapshot n_pages={})",
                id, begin_n_pages
            )));
        }
        // Materialize the committed bytes WITHOUT the registry guard
        // held (the plain get_page path takes cache/WAL locks).
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
        // SAVEPOINT undo capture for the freshly materialized page: the
        // bytes are the committed state, which for every page except the
        // read-committed page 0 is byte-stable since BEGIN (the stamp gate
        // above aborts otherwise) — i.e. exactly the savepoint-time bytes
        // for this first post-savepoint fetch.
        if !txn.savepoints.is_empty() {
            capture_shadow_undo(txn, id, &pr);
        }
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
