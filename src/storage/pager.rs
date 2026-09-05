//! Pager: the layer between the B+tree and the raw file.
//!
//! Responsibilities:
//! - Read/write fixed-size pages from the database file
//! - Cache pages in memory (FIFO eviction — see `get_page` comment)
//! - Allocate new pages (and maintain a freelist)
//! - Coordinate with the WAL for durability
//! - Snapshot/restore for transaction ROLLBACK
//!
//! ## Concurrency model
//!
//! `Pager` is `Send + Sync` and all public methods take `&self` — the entire
//! pager is wrapped in interior mutability. This means N reader threads can
//! share a single `&Pager` and call `get_page` concurrently.
//!
//! - The page cache is `RwLock<HashMap<PageId, PageRef>>` — cache hits take
//!   a brief read lock and clone the `Arc<Mutex<Page>>`, cache misses take
//!   a brief write lock to insert the new entry.
//! - The LRU list (FIFO actually — see `get_page` comment) is a
//!   `Mutex<VecDeque<PageId>>` because we only push/pop from one end.
//! - The file is `File` + positioned I/O (`read_at` / `write_at`) so no
//!   shared file offset state needs synchronization. The kernel still has
//!   an internal offset for the file description but `pread`/`pwrite`
//!   don't touch it.
//! - The page itself is `Arc<Mutex<Page>>`, so the cache lock is held only
//!   briefly during the lookup/insert; the page lock is held during the
//!   actual decode/encode work.
//!
//! Writes are serialized through the cache write lock (only one writer
//! inserting/evicting pages at a time) but reads can proceed concurrently
//! because they take only a read lock on the cache.

use crate::error::{Error, Result};
use crate::storage::page::{FileHeader, Page, PageId, DB_HEADER_SIZE, DEFAULT_PAGE_SIZE};
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
// Seek/Read/Write are only needed by the non-unix fallback I/O helpers
// below; the unix path uses positioned I/O (read_at/write_all_at).
#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

/// A shared mutable reference to a page.
///
/// `Arc<Mutex<Page>>` makes `PageRef` `Send + Sync`, which in turn makes
/// `Pager` (and therefore `Database`) `Send + Sync`. This unlocks true
/// concurrent reads when `Database` is wrapped in `Arc<RwLock<Database>>`:
/// N readers can hold `&Database` simultaneously, each calling `query_shared()`
/// without serializing against other readers.
///
/// We use `parking_lot::Mutex` rather than `std::sync::Mutex` because:
///  - It's ~2× faster on the uncontended fast path (~10 ns vs ~25 ns on x86_64 Linux).
///  - It never poisons (so a panicking thread doesn't take the DB down with it).
///  - It's futex-based on Linux, which has lower scheduling overhead than std's `Mutex`.
///
/// The lock is held only briefly during each page operation (a few hundred ns
/// for a leaf scan, a few µs for a split). The performance cost relative to
/// the previous `Rc<RefCell<Page>>` (~10 ns per access) is negligible compared
/// to the cost of a single B+tree seek (~1 µs).
pub type PageRef = Arc<Mutex<Page>>;

/// Snapshot of mutable pager state, captured at BEGIN. Used by ROLLBACK to
/// restore the in-memory state to the pre-transaction point.
///
/// We do NOT deep-copy the page cache — that would be O(N * page_size).
/// Instead we capture the metadata (page count, freelist, schema cookie) and
/// on ROLLBACK we drop ALL cached pages so the next read repopulates from
/// disk. This works because during a transaction, no writes go through to
/// disk (the executor's `if !ctx.in_transaction { flush() }` guard ensures
/// that), so the file still holds the pre-BEGIN state.
#[derive(Clone, Debug)]
pub struct PagerSnapshot {
    pub n_pages: u32,
    pub freelist_head: PageId,
    pub freelist_count: u32,
    pub schema_cookie: u32,
}

impl PagerSnapshot {
    /// Snapshot the pager's mutable metadata at the current point in time.
    pub fn capture(pager: &Pager) -> Self {
        Self {
            n_pages: pager.n_pages.load(Ordering::Acquire),
            freelist_head: pager.freelist_head.load(Ordering::Acquire),
            freelist_count: pager.freelist_count.load(Ordering::Acquire),
            schema_cookie: pager.schema_cookie.load(Ordering::Acquire),
        }
    }
}

/// LRU cache of pages. We use a HashMap for O(1) lookup and a doubly-linked
/// list (via `VecDeque` + indices) for LRU ordering.
///
/// For simplicity, we use a `LinkedHashMap`-style structure built on top of
/// `HashMap` + a manual ordering list. This is fast enough for typical
/// workloads (cache sizes of a few thousand pages).
///
/// ## Interior mutability
///
/// All mutable state is wrapped in `RwLock`/`Mutex`/`Atomic*` so that all
/// public methods take `&self`. This is the key enabler for the multi-threaded
/// concurrent server: N reader threads can call `pager.get_page(id)`
/// simultaneously without serializing on a write lock for cache hits.
/// Fast hasher for u32 page-id keys (splitmix64 finalizer).
///
/// The page cache is looked up on EVERY B+tree level of EVERY operation —
/// a descent through a 3-level tree hashes the same page-id class 3+ times.
/// std's default SipHash-1-3 costs ~20-25 ns per u32; this is ~2 ns with
/// full avalanche (splitmix64 finalizer), saving ~60-100 ns per lookup
/// chain. The `write` fallback (never used for u32 keys, but required by
/// the Hasher trait) is FNV-1a.
#[derive(Default)]
pub struct PageIdHasher(u64);

impl std::hash::Hasher for PageIdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        let mut z = (i as u64).wrapping_add(0x9E3779B97F4A7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        self.0 = z ^ (z >> 31);
    }
}

#[derive(Clone, Default)]
pub struct PageIdHashBuild;

impl std::hash::BuildHasher for PageIdHashBuild {
    type Hasher = PageIdHasher;
    #[inline]
    fn build_hasher(&self) -> PageIdHasher {
        PageIdHasher::default()
    }
}

/// Page-id-keyed map types using the fast hasher.
pub type PageCacheMap = std::collections::HashMap<PageId, PageRef, PageIdHashBuild>;
pub type PageIdSet = std::collections::HashSet<PageId, PageIdHashBuild>;

/// Pages with id below this bound live in the direct-indexed Vec (any
/// file up to 4 GB at 4 KB pages); higher ids spill to a HashMap. This
/// bounds the Vec at 8 MB regardless of database size while keeping the
/// no-hash fast path for every page of a normal database.
const PAGE_VEC_DIRECT_LIMIT: usize = 1 << 20;

/// Dense page-id → page cache.
///
/// Page ids are small sequential integers, so low ids are stored in a
/// `Vec<Option<PageRef>>` indexed directly by page id — no hashing, no
/// probing — instead of a HashMap. Every B-tree descent touches 2-4
/// pages, each of which previously paid a hash get (~25 ns) under the
/// cache's read lock; direct indexing costs ~2 ns. Ids beyond
/// `PAGE_VEC_DIRECT_LIMIT` (huge files) spill into a HashMap so the Vec
/// can never grow proportional to a multi-GB file.
pub struct PageCache {
    slots: Vec<Option<PageRef>>,
    overflow: PageCacheMap,
    count: usize,
}

impl PageCache {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            overflow: PageCacheMap::default(),
            count: 0,
        }
    }

    #[inline]
    pub fn get(&self, id: PageId) -> Option<&PageRef> {
        if (id as usize) < PAGE_VEC_DIRECT_LIMIT {
            self.slots.get(id as usize).and_then(|s| s.as_ref())
        } else {
            self.overflow.get(&id)
        }
    }

    #[inline]
    pub fn contains_key(&self, id: PageId) -> bool {
        self.get(id).is_some()
    }

    #[inline]
    pub fn insert(&mut self, id: PageId, page: PageRef) {
        if (id as usize) < PAGE_VEC_DIRECT_LIMIT {
            let idx = id as usize;
            if idx >= self.slots.len() {
                self.slots.resize(idx + 1, None);
            }
            if self.slots[idx].replace(page).is_none() {
                self.count += 1;
            }
        } else {
            if self.overflow.insert(id, page).is_none() {
                self.count += 1;
            }
        }
    }

    #[inline]
    pub fn remove(&mut self, id: PageId) -> Option<PageRef> {
        let old = if (id as usize) < PAGE_VEC_DIRECT_LIMIT {
            self.slots.get_mut(id as usize).and_then(|s| s.take())
        } else {
            self.overflow.remove(&id)
        };
        if old.is_some() {
            self.count -= 1;
        }
        old
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.overflow.clear();
        self.count = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = (PageId, &PageRef)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|p| (i as PageId, p)))
            .chain(self.overflow.iter().map(|(k, v)| (*k, v)))
    }
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

/// WAL-mode pager state (see `Pager::wal`).
pub struct WalState {
    /// The WAL (writer-side handle; also used for frame reads via
    /// `read_frame_at`, which is `&self`).
    pub wal: crate::storage::wal::Wal,
    /// Committed page-id → frame offset ("WAL-served reads" index).
    pub map: std::collections::HashMap<PageId, u64, PageIdHashBuild>,
}

/// Auto-checkpoint threshold: after a commit leaves this many frames in
/// the WAL, copy them back to the main file and reset (SQLite's default
/// `wal_autocheckpoint` is 1000 pages at 4 KiB; ours is the same frame
/// count).
const WAL_AUTOCHECKPOINT_FRAMES: u32 = 1000;

/// Backing store for the pager: either a real file (positioned I/O) or a
/// pure in-memory byte image (`:memory:` databases).
///
/// The memory store eliminates ALL file syscalls from the `:memory:`
/// open/write path. The old tempfile-backed scheme paid
/// open+create+stat+write+unlink per `Database::open_in_memory()` —
/// 50-100 µs on Linux tmpfs, and far worse on macOS APFS (file creation
/// there is markedly slower), which dominated every workload that opens
/// a throwaway database per iteration (bench harnesses, tests, probes).
enum Store {
    File(File),
    Memory(std::sync::Mutex<Vec<u8>>),
}

impl Store {
    /// Positioned read so multiple threads can read without serializing
    /// on a file offset. Returns the number of bytes read (0 at EOF).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Store::File(f) => {
                use std::os::unix::fs::FileExt;
                f.read_at(buf, offset)
            }
            #[cfg(windows)]
            Store::File(f) => {
                use std::os::windows::fs::FileExt;
                f.seek_read(buf, offset)
            }
            Store::Memory(m) => {
                let m = m.lock().unwrap_or_else(|e| e.into_inner());
                let off = offset as usize;
                if off >= m.len() {
                    return Ok(0);
                }
                let n = buf.len().min(m.len() - off);
                buf[..n].copy_from_slice(&m[off..off + n]);
                Ok(n)
            }
        }
    }

    /// Positioned write (pread/pwrite analogue). The memory image grows
    /// on demand, zero-filling any gap — same semantics as writing at an
    /// offset past EOF on a sparse file.
    fn write_all_at(&self, offset: u64, buf: &[u8]) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Store::File(f) => {
                use std::os::unix::fs::FileExt;
                f.write_all_at(buf, offset)
            }
            #[cfg(windows)]
            Store::File(f) => {
                use std::os::windows::fs::FileExt;
                let mut done = 0usize;
                while done < buf.len() {
                    let n = f.seek_write(&buf[done..], offset + done as u64)?;
                    if n == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "failed to write whole buffer",
                        ));
                    }
                    done += n;
                }
                Ok(())
            }
            Store::Memory(m) => {
                let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
                let off = offset as usize;
                let end = off + buf.len();
                if end > m.len() {
                    m.resize(end, 0);
                }
                m[off..end].copy_from_slice(buf);
                Ok(())
            }
        }
    }

    /// Current image length in bytes.
    fn len(&self) -> std::io::Result<u64> {
        match self {
            Store::File(f) => Ok(f.metadata()?.len()),
            Store::Memory(m) => Ok(m.lock().unwrap_or_else(|e| e.into_inner()).len() as u64),
        }
    }

    /// Truncate or zero-extend the image.
    fn set_len(&self, n: u64) -> std::io::Result<()> {
        match self {
            Store::File(f) => f.set_len(n),
            Store::Memory(m) => {
                m.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .resize(n as usize, 0);
                Ok(())
            }
        }
    }

    /// Durability barrier. Pure no-op for the memory store (nothing to
    /// sync — the image IS the durable state for as long as the pager
    /// lives, and it is deleted on drop by design).
    fn sync_all(&self) -> std::io::Result<()> {
        match self {
            Store::File(f) => f.sync_all(),
            Store::Memory(_) => Ok(()),
        }
    }

    /// `std::fs::Metadata` for file-backed stores. Memory stores have no
    /// meaningful fs metadata — callers treat the error as "file-shape
    /// checks don't apply" (e.g. integrity_check's truncation probe).
    fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        match self {
            Store::File(f) => f.metadata(),
            Store::Memory(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "in-memory store has no file metadata",
            )),
        }
    }

    pub fn is_memory(&self) -> bool {
        matches!(self, Store::Memory(_))
    }
}

impl Pager {
    /// True when the backing store is the in-memory image (a `:memory:`
    /// database). In-memory payloads are this process's own encoder
    /// output — the basis for trusted TEXT decoding in the scan family.
    pub fn is_memory(&self) -> bool {
        self.store.is_memory()
    }
}

pub struct Pager {
    store: Store,
    path: PathBuf,
    /// Page size in bytes (immutable after `open`).
    page_size: AtomicU32,
    /// Total number of pages in the file (updated on writes).
    n_pages: AtomicU32,
    /// Head of the freelist (0 if empty).
    freelist_head: AtomicU32,
    /// Number of pages on the freelist.
    freelist_count: AtomicU32,
    /// In-memory cache: page_id → page. RwLock so reads on distinct pages
    /// don't serialize; only cache-miss inserts take the write lock.
    /// Direct-indexed Vec for low page ids (see `PageCache`).
    cache: RwLock<PageCache>,
    /// LRU ordering: most recently used at the back.
    lru: Mutex<VecDeque<PageId>>,
    /// Maximum number of pages to keep in the cache (immutable after open).
    cache_capacity: usize,
    /// Schema cookie, bumped on every schema change.
    schema_cookie: AtomicU32,
    /// True if this is a freshly created database (no header yet).
    is_new: AtomicBool,
    /// When true, `flush()` skips `file.sync_all()`. This is a HUGE perf win
    /// for in-memory databases (which use a tempfile under the hood — the
    /// fsync on a tempfile is a no-op anyway on most tmpfs filesystems, but
    /// the syscall round-trip still costs ~5-50 µs per call, which is the
    /// dominant cost for auto-commit INSERT workloads).
    ///
    /// Set by `Database::open_in_memory` so `:memory:` databases get the
    /// same per-statement overhead as SQLite's `:memory:` mode (which never
    /// fsyncs because there's no file at all).
    skip_fsync: AtomicBool,
    /// Logical WAL mode for PURE in-memory stores. `PRAGMA
    /// journal_mode=WAL` on a `:memory:` database historically reported
    /// "wal" (the old tempfile-backed scheme opened a sidecar file); tests
    /// and ORMs rely on the round-trip. With the pure in-memory store there
    /// is nothing to recover — WAL's only jobs (crash recovery, durable
    /// commit frames) are meaningless — so the flag records the mode and
    /// every WAL file operation is skipped.
    memory_wal: AtomicBool,
    /// Whether FOREIGN KEY constraints are enforced (PRAGMA foreign_keys).
    /// Lives on the pager so both Database (api.rs) and the executor's
    /// static statement dispatcher can reach it through a shared &Pager.
    foreign_keys_enabled: AtomicBool,
    /// Advisory PRAGMA locking_mode ("exclusive" vs "normal"). The
    /// engine's actual cross-connection locking is the transaction slot;
    /// this flag makes the pragma round-trip observable (SQLite: the write
    /// form returns the new mode and later reads repeat it).
    locking_mode_exclusive: AtomicBool,
    /// Whether triggers may fire recursively (PRAGMA recursive_triggers).
    /// SQLite's DEFAULT IS OFF: a trigger does not re-fire for statements
    /// executed from inside another trigger. Our engine previously always
    /// recursed (up to the depth cap), which broke the common
    /// self-inserting AFTER INSERT trigger pattern.
    recursive_triggers_enabled: AtomicBool,
    /// Lazy write-back mode (in-memory databases). When true, `flush()` does
    /// NOT write dirty pages to the backing temp file — it just resets the
    /// dirty bookkeeping (O(1)). Dirty pages are written lazily by cache
    /// eviction instead. Since in-memory DBs are deleted on close, the file
    /// is only a spill area for caches larger than memory, so per-statement
    /// write() syscalls are pure overhead. This is what makes autocommit
    /// INSERTs in `:memory:` mode competitive with SQLite's.
    lazy_writeback: AtomicBool,
    /// Last page id inserted into `dirty_pages` (see note_dirty's fast
    /// path). u32::MAX = none. Reset by flush().
    last_noted_dirty: std::sync::atomic::AtomicU32,
    /// Upper-bound count of dirty pages since the last `flush()`. Incremented
    /// by `note_write()` on every mutating operation (allocate_page, free_page,
    /// Btree insert/delete/etc.). Reset to 0 by `flush()`.
    ///
    /// This is an **upper bound**, not an exact count: a single page dirtied
    /// twice increments the counter twice. The invariant we maintain is:
    ///   `dirty_count_approx == 0  ⟹  no pages are dirty`
    /// which is sufficient to make `flush()`'s fast path O(1) and to make
    /// `dirty_page_count()` O(1) for the threshold check.
    dirty_count_approx: AtomicUsize,
    /// Set of page IDs that are dirty (have `dirty == true`). Inserted by
    /// `note_dirty(id)` (called whenever a page is marked dirty). Removed
    /// by `flush()` after the page is written back.
    ///
    /// This is the EXACT set of dirty pages (modulo deduplication — the
    /// same page inserted twice is fine, `HashSet::insert` is idempotent).
    /// `flush()` iterates this set instead of scanning the entire cache,
    /// making it O(dirty_count) instead of O(cache_size). For a workload
    /// with a 10k-page cache but only 2 dirty pages per statement, this
    /// turns flush() from O(10k) page-lock-acquire-and-check into O(2)
    /// HashSet lookups — a 5000× speedup on the per-statement overhead.
    dirty_pages: Mutex<PageIdSet>,
    /// Truncation target armed by `truncate_tail` (0 = none): the new
    /// n_pages bound after a mass-delete freed the file's tail. The
    /// in-memory state shrinks immediately (cache / LRU / dirty set /
    /// WAL map / freelist / n_pages); the physical `set_len` happens at
    /// the next flush/checkpoint so the file length moves atomically
    /// with the commit that writes the new header.
    pending_truncate: std::sync::atomic::AtomicU32,
    /// Run-once guard for the v3 → v4 freelist migration (the file bytes
    /// keep the v3 magic until the next flush, so re-entry must not
    /// re-interpret an already-trunk freelist as legacy-linked).
    legacy_migrated: AtomicBool,
    /// Write-Ahead Log state. `None` = journal_mode=delete (writes go
    /// straight to the main file). `Some(...)` = journal_mode=wal: commits
    /// APPEND frames here, reads consult the committed-page map before the
    /// main file (WAL-served reads), and checkpoints copy pages back.
    ///
    /// The RwLock is for the page map: readers take the read lock to
    /// resolve a page → frame offset while the (single) writer appends
    /// under the write lock. `Wal` itself is writer-only; the read path
    /// goes through `Wal::read_frame_at` on a shared handle.
    wal: RwLock<Option<WalState>>,
    /// PRAGMA synchronous: 0=OFF, 1=NORMAL, 2=FULL (SQLite default).
    /// In WAL mode NORMAL skips the per-commit fsync (checkpoints carry
    /// durability) — SQLite's recommended high-throughput setting.
    synchronous: std::sync::atomic::AtomicU8,
    /// Monotonic write version, bumped by every `note_write()` (i.e. every
    /// mutating B+tree/pager operation). Readers use it to invalidate
    /// advisory caches (btree leaf hints): a version change means SOME
    /// page content changed somewhere, so cached leaf bounds may be stale
    /// and — critically — a page may have been recycled into another tree,
    /// so a hint must never be trusted across a write.
    write_version: std::sync::atomic::AtomicU64,
    /// Count of get_page slow-path (cache-miss → file read) events.
    /// Debug/diagnostic counter.
    cache_misses: std::sync::atomic::AtomicU64,
    /// Unique id of this Pager instance within the process. Advisory
    /// caches (btree leaf hints) tag entries with (instance, version) so a
    /// new database opened on the same thread can never mistake stale
    /// hints from a previous database for its own — even when both assign
    /// the same page ids (they always do: roots start at low sequential
    /// ids). See `write_epoch`.
    instance_id: u64,
    /// SAVEPOINT undo stack (SQLite-style nested transactions).
    ///
    /// Each level holds the pager metadata at SAVEPOINT time plus page
    /// PRE-IMAGES: every page fetched (get_page) while this level is the
    /// newest gets its bytes copied into the level's log on FIRST fetch —
    /// and every mutation necessarily get_page()s the page before
    /// modifying it, so the copy is always the pre-mutation state.
    /// ROLLBACK TO <name> restores those bytes (in cache, marked dirty),
    /// drops pages allocated after the savepoint, and rewinds metadata.
    savepoints: Mutex<Vec<SavepointLevel>>,
    /// MINIMUM `base.n_pages` across all stacked savepoint levels (u32::MAX
    /// when none). A page id >= this can NEVER need a pre-image for any
    /// level (all levels' bases are <= it), so `capture_savepoint_undo`
    /// skips the lock + hash probe entirely — one relaxed atomic load.
    /// Maintained at push (min never grows: a new level's base is the
    /// CURRENT n_pages, >= every existing base) and pop/clear (recompute).
    savepoint_min_base: std::sync::atomic::AtomicU32,
    /// Mirror of `savepoints.len()` for the get_page fast path (an atomic
    /// load when no savepoint is active — the common case — instead of a
    /// Mutex lock).
    savepoint_depth: std::sync::atomic::AtomicUsize,
    /// Active page codec (`PRAGMA codec = <name>`): every main-file page
    /// write passes through `encode`, every read through `decode`. Page 0
    /// keeps its first 100 bytes (the file header + marker area) plain.
    /// Mutually exclusive with WAL mode (WAL frames would need the same
    /// treatment — enforced in both directions).
    codec: RwLock<crate::plugin::codec::CodecState>,
    /// Codec name recorded in the file header marker (read at open): a
    /// plain `open()` of a coded file fails with a pointer to
    /// `Database::open_with_codec`.
    required_codec: Mutex<Option<String>>,
    /// Cross-statement join-build cache (advisory, epoch-validated —
    /// see `storage/join_cache.rs`). Hosted on the pager because the
    /// executor has no Database handle, and the pager already owns the
    /// write epoch that invalidates every advisory cache.
    join_cache: Mutex<crate::storage::join_cache::JoinBuildCache>,
}

/// One SAVEPOINT level: the metadata snapshot plus page pre-images.
struct SavepointLevel {
    name: String,
    base: PagerSnapshot,
    /// page id -> page bytes as of this savepoint's creation (captured at
    /// first fetch after creation). Pages allocated AFTER the savepoint
    /// (id >= base.n_pages) are dropped rather than restored.
    pages: std::collections::HashMap<PageId, Vec<u8>, PageIdHashBuild>,
}

impl Pager {
    /// Create a savepoint. Must be called while `in_transaction` is true
    /// (the caller ensures a transaction is open, starting one if needed).
    pub fn savepoint(&self, name: &str) {
        let mut sp = self.savepoints.lock();
        sp.push(SavepointLevel {
            name: name.to_ascii_lowercase(),
            base: PagerSnapshot::capture(self),
            pages: std::collections::HashMap::default(),
        });
        // A new level's base is the CURRENT n_pages (>= every existing
        // base — pages only grow), so the min cannot shrink here; the
        // store keeps the invariant explicit.
        let m = sp.iter().map(|s| s.base.n_pages).min().unwrap_or(u32::MAX);
        self.savepoint_min_base.store(m, Ordering::Relaxed);
        self.savepoint_depth.store(sp.len(), Ordering::Release);
    }

    /// ROLLBACK TO SAVEPOINT <name>: restore the pager to the savepoint's
    /// state. The savepoint itself stays active (SQLite semantics);
    /// savepoints created after it are discarded. Returns the savepoint's
    /// new stack depth, or None when no savepoint with that name exists.
    pub fn rollback_savepoint(&self, name: &str) -> Result<Option<usize>> {
        // Phase 1 (under the savepoints lock): locate the level and TAKE
        // its undo data + base snapshot, truncating the levels above it.
        // The lock is released before any page work — get_page's undo
        // capture re-locks this mutex, and std::sync::Mutex is not
        // reentrant (holding it across get_page deadlocked).
        //
        // SQLite semantics: the savepoint STAYS ON THE STACK after
        // ROLLBACK TO — it can be rolled back to again, or RELEASEd
        // later. Its undo log is reset (changes were just undone) but its
        // BASE snapshot is kept, so a second ROLLBACK TO restores to the
        // same point.
        let (undo, base) = {
            let mut sp = self.savepoints.lock();
            let idx = match sp.iter().rposition(|s| s.name == name.to_ascii_lowercase()) {
                Some(i) => i,
                None => return Ok(None),
            };
            let level = sp.split_off(idx);
            let level = level.into_iter().next().unwrap();
            // Re-push the savepoint (kept active) with the same base and
            // an empty undo log.
            sp.push(SavepointLevel {
                name: name.to_ascii_lowercase(),
                base: level.base.clone(),
                pages: std::collections::HashMap::default(),
            });
            let keep_depth = sp.len();
            self.savepoint_depth.store(keep_depth, Ordering::Release);
            (level.pages, level.base)
        };
        // Phase 2 (no savepoints lock): restore pre-images for pages that
        // existed at savepoint time. Pages below this savepoint keep their
        // existing undo entries (any page in OUR log was fetched after the
        // lower savepoints were created, so they logged it first — the
        // capture hook's or_insert is a no-op for them).
        for (id, bytes) in undo {
            if id >= base.n_pages {
                continue; // allocated after the savepoint — dropped below
            }
            let page = self.get_page(id)?;
            {
                let mut b = page.lock();
                if b.data.len() == bytes.len() {
                    b.data.copy_from_slice(&bytes);
                    b.dirty = true;
                } else {
                    // Page-size mismatch can't happen within one file;
                    // be defensive rather than corrupt.
                    return Err(Error::corruption(format!(
                        "savepoint restore: page {id} size mismatch"
                    )));
                }
            }
            self.note_dirty(id);
        }
        // Phase 3: drop pages allocated after the savepoint (evict from
        // cache, remove from the dirty set).
        {
            let mut cache = self.cache.write();
            // Low ids are dense Vec slots; clear everything above the base.
            let base_n = base.n_pages as usize;
            if base_n < PAGE_VEC_DIRECT_LIMIT {
                let slots_len = cache.slots.len();
                for idx in base_n..slots_len.min(PAGE_VEC_DIRECT_LIMIT) {
                    cache.slots[idx] = None;
                }
                cache.count =
                    cache.slots.iter().filter(|s| s.is_some()).count() + cache.overflow.len();
            }
            cache.overflow.retain(|&id, _| id < base.n_pages);
            let mut dp = self.dirty_pages.lock();
            dp.retain(|&id| id < base.n_pages);
            self.last_noted_dirty.store(u32::MAX, Ordering::Release);
        }
        self.lru.lock().clear();
        // Phase 4: rewind mutable metadata + truncate the file if it grew.
        // A tail truncation armed by an in-txn mass DELETE is part of the
        // UNCOMMITTED work being undone — disarm it, or the next flush
        // would shrink the file under the restored page count.
        self.pending_truncate.store(0, Ordering::Release);
        self.n_pages.store(base.n_pages, Ordering::Release);
        self.freelist_head
            .store(base.freelist_head, Ordering::Release);
        self.freelist_count
            .store(base.freelist_count, Ordering::Release);
        self.schema_cookie
            .store(base.schema_cookie, Ordering::Release);
        let target_size = base.n_pages as u64 * self.page_size() as u64;
        if let Ok(len) = self.store.len() {
            if len > target_size {
                self.store.set_len(target_size)?;
            }
        }
        // Phase 5: restored content differs from disk → restored pages are
        // dirty. Invalidate advisory caches (leaf hints).
        self.write_version.fetch_add(1, Ordering::Relaxed);
        let n_dirty = self.dirty_pages.lock().len();
        self.dirty_count_approx.store(n_dirty, Ordering::Release);
        Ok(Some(self.savepoint_depth.load(Ordering::Acquire)))
    }

    /// RELEASE [SAVEPOINT] <name>: discard the savepoint and everything
    /// above it WITHOUT rolling back. Returns the remaining stack depth
    /// (0 = none left), or None when the name is unknown.
    pub fn release_savepoint(&self, name: &str) -> Option<usize> {
        let mut sp = self.savepoints.lock();
        let idx = sp
            .iter()
            .rposition(|s| s.name == name.to_ascii_lowercase())?;
        sp.truncate(idx);
        let remaining = sp.len();
        let m = sp.iter().map(|s| s.base.n_pages).min().unwrap_or(u32::MAX);
        self.savepoint_min_base.store(m, Ordering::Relaxed);
        self.savepoint_depth.store(remaining, Ordering::Release);
        Some(remaining)
    }

    /// Discard all savepoints (COMMIT / plain ROLLBACK).
    pub fn clear_savepoints(&self) {
        let mut sp = self.savepoints.lock();
        sp.clear();
        self.savepoint_min_base.store(u32::MAX, Ordering::Relaxed);
        self.savepoint_depth.store(0, Ordering::Release);
    }

    /// Roll the pager back to the OUTERMOST savepoint — the transaction
    /// base for a plain BEGIN..ROLLBACK. Restores page content from the
    /// undo pre-images (non-destructive: mid-transaction eviction writes
    /// are undone in-cache, pages allocated by the transaction are
    /// dropped, metadata rewinds). Returns false when no savepoint is
    /// stacked (the caller falls back to the destructive snapshot
    /// restore).
    pub fn rollback_bottom_savepoint(&self) -> Result<bool> {
        let bottom_name = {
            let sp = self.savepoints.lock();
            match sp.first() {
                Some(level) => level.name.clone(),
                None => return Ok(false),
            }
        };
        let restored = self.rollback_savepoint(&bottom_name)?;
        self.clear_savepoints();
        Ok(restored.is_some())
    }

    /// True when at least one savepoint is active.
    pub fn has_savepoints(&self) -> bool {
        self.savepoint_depth.load(Ordering::Acquire) > 0
    }

    /// Capture the page's current bytes into every savepoint level that
    /// hasn't seen this page yet. Called from get_page (both hit and
    /// insert paths) — the bytes at fetch time are the pre-mutation state
    /// because every mutation locks the page through get_page first.
    fn capture_savepoint_undo(&self, id: PageId, page: &PageRef) {
        // Range fast path: pages allocated after EVERY level's base need
        // no pre-image (see the loop below) — a bulk-INSERT transaction
        // allocates all its leaves after BEGIN, so this one atomic load
        // replaces a Mutex + HashMap probe + level scan on EVERY get_page
        // of the write storm (~60-100 ns x 6+ pages/row on multi-index
        // loads — the dominant per-row overhead vs SQLite).
        if (id as u64) >= self.savepoint_min_base.load(Ordering::Relaxed) as u64 {
            return;
        }
        let mut sp = self.savepoints.lock();
        if sp.is_empty() {
            return;
        }
        // Fast exit: the newest level already has this page (the common
        // re-fetch loop on a hot page).
        if sp
            .last()
            .map(|s| s.pages.contains_key(&id))
            .unwrap_or(false)
        {
            return;
        }
        // Pages allocated AFTER a level's base are DROPPED on rollback to
        // that level (never restored — see `rollback_savepoint`'s
        // `id >= base.n_pages` skip), so they need no pre-image. A bulk
        // INSERT transaction allocates every leaf page after BEGIN: this
        // check is the difference between a ~n_pages x 4 KB undo journal
        // and ~0 for the entire write storm.
        let mut needs_capture = false;
        for level in sp.iter() {
            if (id as u64) < level.base.n_pages as u64 {
                needs_capture = true;
                break;
            }
        }
        if !needs_capture {
            return;
        }
        let bytes = {
            let b = page.lock();
            b.data.clone()
        };
        for level in sp.iter_mut() {
            if (id as u64) < level.base.n_pages as u64 {
                level.pages.entry(id).or_insert_with(|| bytes.clone());
            }
        }
    }
}

/// Process-wide Pager instance counter (see `Pager::instance_id`).
static PAGER_INSTANCE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for Pager {
    fn drop(&mut self) {
        // Clean shutdown: checkpoint committed WAL frames into the main
        // file and remove the -wal file (SQLite's last-connection-close
        // behavior). Best-effort — an unclean exit (crash, kill) skips
        // this and recovery on next open serves the committed frames.
        if self.wal.read().is_some() {
            let _ = self.checkpoint_wal();
            let _ = std::fs::remove_file(crate::storage::wal::wal_path_for(&self.path));
        }
    }
}

impl Pager {
    /// Open or create a database file at the given path.
    pub fn open<P: AsRef<Path>>(path: P, cache_capacity: usize) -> Result<Self> {
        Self::open_opts(path, cache_capacity, false)
    }

    /// Open with the durable-sync policy decided up front. `skip_sync`
    /// pre-arms `skip_fsync` BEFORE `initialize_new_db` runs, so a fresh
    /// `:memory:` database never pays the header fsync (~0.4 ms Linux CI,
    /// ~1.5 ms macOS, ~10 ms Windows per open).
    pub fn open_opts<P: AsRef<Path>>(
        path: P,
        cache_capacity: usize,
        skip_sync: bool,
    ) -> Result<Self> {
        let path = path.ref_to_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Never truncate: an existing database file's pages must be
            // preserved (we open, read the header, and cache pages on
            // demand); truncation would destroy the database.
            .truncate(false)
            .open(&path)?;
        Self::from_store(Store::File(file), path, cache_capacity, skip_sync)
    }

    /// Open a PURE in-memory pager: no file is ever created, opened,
    /// written, or unlinked. The page image lives in a `Vec<u8>` that
    /// grows on demand (spill target for cache eviction) and is dropped
    /// with the pager. `skip_fsync` and `lazy_writeback` come pre-armed —
    /// the durability flags are meaningless when there is no file.
    ///
    /// This replaces the old tempfile-backed `:memory:` scheme, whose
    /// open cost (create+stat+write+unlink) ranged from ~50 µs on Linux
    /// tmpfs to several hundred µs on macOS APFS and dominated every
    /// per-iteration-open workload.
    pub fn open_memory(cache_capacity: usize) -> Result<Self> {
        let store = Store::Memory(std::sync::Mutex::new(Vec::new()));
        let path = PathBuf::from(":memory:");
        let pager = Self::from_store(store, path, cache_capacity, true)?;
        pager.lazy_writeback.store(true, Ordering::Release);
        Ok(pager)
    }

    /// Shared constructor from an already-built backing store.
    fn from_store(
        store: Store,
        path: PathBuf,
        cache_capacity: usize,
        skip_sync: bool,
    ) -> Result<Self> {
        let pager = Self {
            store,
            path,
            page_size: AtomicU32::new(DEFAULT_PAGE_SIZE),
            n_pages: AtomicU32::new(0),
            freelist_head: AtomicU32::new(0),
            freelist_count: AtomicU32::new(0),
            cache: RwLock::new(PageCache::new()),
            lru: Mutex::new(VecDeque::new()),
            cache_capacity,
            schema_cookie: AtomicU32::new(0),
            is_new: AtomicBool::new(false),
            skip_fsync: AtomicBool::new(skip_sync),
            memory_wal: AtomicBool::new(false),
            foreign_keys_enabled: AtomicBool::new(false),
            locking_mode_exclusive: AtomicBool::new(false),
            recursive_triggers_enabled: AtomicBool::new(false),
            lazy_writeback: AtomicBool::new(false),
            last_noted_dirty: std::sync::atomic::AtomicU32::new(u32::MAX),
            pending_truncate: std::sync::atomic::AtomicU32::new(0),
            legacy_migrated: AtomicBool::new(false),
            dirty_count_approx: AtomicUsize::new(0),
            dirty_pages: Mutex::new(PageIdSet::default()),
            write_version: std::sync::atomic::AtomicU64::new(0),
            wal: RwLock::new(None),
            synchronous: std::sync::atomic::AtomicU8::new(2),
            cache_misses: std::sync::atomic::AtomicU64::new(0),
            instance_id: PAGER_INSTANCE_COUNTER
                .fetch_add(1, Ordering::Relaxed)
                .checked_add(1)
                .unwrap_or(0),
            savepoints: Mutex::new(Vec::new()),
            savepoint_min_base: std::sync::atomic::AtomicU32::new(u32::MAX),
            savepoint_depth: std::sync::atomic::AtomicUsize::new(0),
            codec: RwLock::new(crate::plugin::codec::CodecState::default()),
            required_codec: Mutex::new(None),
            join_cache: Mutex::new(std::collections::HashMap::new()),
        };

        let file_size = pager.store.len()?;
        if file_size == 0 {
            pager.is_new.store(true, Ordering::Release);
            pager.initialize_new_db()?;
        } else {
            pager.read_header()?;
            // v3 files: the legacy linked-list freelist becomes trunk
            // format here (one-time, in-cache; durable with the next
            // flush alongside the v4 magic). Empty-freelist v3 files only
            // need the magic byte rewrite.
            pager.migrate_legacy_freelist()?;
            // Crash recovery: a leftover -wal file holds committed pages
            // newer than the main file. Opening it switches the pager to
            // WAL mode and makes those frames visible through the page map
            // (WAL-served reads) — committed data survives an unclean
            // shutdown, torn transactions are discarded at frame level.
            // Memory stores have no sidecar WAL (enable_wal is a no-op
            // there), so the probe only runs for file-backed pagers.
            if !pager.store.is_memory() {
                let wal_file = crate::storage::wal::wal_path_for(&pager.path);
                if wal_file.exists()
                    && std::fs::metadata(&wal_file)
                        .map(|m| m.len() > 0)
                        .unwrap_or(false)
                {
                    pager.enable_wal()?;
                }
            }
        }
        Ok(pager)
    }

    /// One-time upgrade of a `RSQLDB03` file: convert the legacy
    /// linked-list freelist (each freed page's first 4 bytes = next) into
    /// the v4 trunk format, and rewrite the magic so the next flush
    /// persists v4. Files with an empty freelist (the common case) only
    /// pay the magic rewrite. Idempotent for v4 files (no-op).
    fn migrate_legacy_freelist(&self) -> Result<()> {
        // Read the magic from the RAW file bytes — NEVER via get_page:
        // for codec-coded files the codec is not armed yet during `open`,
        // and caching an undecoded page 0 would poison every later read
        // of the schema root.
        let mut magic8 = [0u8; 8];
        let n = self.read_file_at(0, &mut magic8)?;
        if n < 8 || magic8 != crate::storage::page::DB_MAGIC_V3 {
            return Ok(()); // v4 (or new): nothing to migrate.
        }
        // Codec-coded v3 files: the freelist pages decode only through
        // the active codec — defer the migration to codec activation
        // (see `set_codec`). A plain v3 file has no codec marker.
        if self.required_codec.lock().is_some() {
            return Ok(());
        }
        self.migrate_legacy_freelist_inner()
    }

    /// The actual v3 → v4 conversion (caller verified the magic and the
    /// codec state). Run-once: the raw file keeps the v3 magic until the
    /// next flush, so a second call would misread the already-converted
    /// trunks as legacy chain nodes.
    fn migrate_legacy_freelist_inner(&self) -> Result<()> {
        if self.legacy_migrated.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // Collect the legacy chain's page ids (cycle-guarded).
        let count = self.freelist_count.load(Ordering::Acquire);
        let mut legacy_ids: Vec<PageId> = Vec::new();
        if count > 0 {
            let mut cur = self.freelist_head.load(Ordering::Acquire);
            let mut steps = count as usize + 4;
            while cur != 0 && steps > 0 {
                steps -= 1;
                legacy_ids.push(cur);
                let next = {
                    let p = self.get_page(cur)?;
                    let b = p.lock();
                    u32::from_le_bytes(b.data[..4].try_into().unwrap_or([0; 4]))
                };
                cur = next;
            }
        }
        // Rebuild: every legacy id becomes a trunk-format free page. Use
        // the FIRST id as the new head trunk and batch the rest as its
        // entries (splitting into further trunks when the array fills).
        self.freelist_head.store(0, Ordering::Release);
        self.freelist_count.store(0, Ordering::Release);
        for &id in &legacy_ids {
            // Direct trunk-format insert (mirrors free_page, minus the
            // pre-image capture — the migration runs before any
            // savepoints exist).
            let head = self.freelist_head.load(Ordering::Acquire);
            let psz = self.page_size() as usize;
            let cap = (psz.saturating_sub(8)) / 4;
            let mut became_trunk = true;
            if head != 0 {
                let trunk = self.get_page(head)?;
                let k = {
                    let b = trunk.lock();
                    u32::from_le_bytes(b.data[4..8].try_into().unwrap_or([0; 4])) as usize
                };
                if k < cap {
                    let off = 8 + k * 4;
                    let mut b = trunk.lock();
                    if off + 4 <= b.data.len() {
                        b.data[off..off + 4].copy_from_slice(&id.to_le_bytes());
                        b.data[4..8].copy_from_slice(&((k + 1) as u32).to_le_bytes());
                        b.dirty = true;
                        drop(b);
                        self.note_dirty(head);
                        became_trunk = false;
                    }
                }
            }
            if became_trunk {
                let p = self.get_page(id)?;
                let mut b = p.lock();
                b.data.fill(0);
                b.data[..4].copy_from_slice(&head.to_le_bytes());
                b.data[4..8].copy_from_slice(&0u32.to_le_bytes());
                b.dirty = true;
                drop(b);
                self.note_dirty(id);
                self.freelist_head.store(id, Ordering::Release);
            }
            self.freelist_count.fetch_add(1, Ordering::AcqRel);
        }
        // Rewrite the magic in-cache (durable with the next flush). This
        // is the FIRST get_page(0) of the migration — by now the caller
        // has verified the file is codec-less (or the codec is armed).
        {
            let page0 = self.get_page(0)?;
            let mut b = page0.lock();
            b.data[..8].copy_from_slice(&crate::storage::page::DB_MAGIC);
            b.dirty = true;
        }
        self.note_dirty(0);
        Ok(())
    }

    /// Create a fresh database: write page 0 with the file header and an
    /// empty leaf page (the schema table root).
    fn initialize_new_db(&self) -> Result<()> {
        let page_size = self.page_size.load(Ordering::Acquire);
        let mut page0 = Page::new(0, page_size);
        FileHeader::write(
            &mut page0.data,
            page_size,
            1, // 1 page total
            0, // schema cookie
        );
        // Page 0 is also the schema table's root (a leaf table page).
        // The header is 100 bytes; the B+tree header begins at offset 100.
        page0.data[DB_HEADER_SIZE as usize] = crate::storage::page::PageType::LeafTable as u8;
        page0.data[DB_HEADER_SIZE as usize + 4..DB_HEADER_SIZE as usize + 6]
            .copy_from_slice(&0u16.to_be_bytes()); // n_cells = 0
        page0.data[DB_HEADER_SIZE as usize + 6..DB_HEADER_SIZE as usize + 8]
            .copy_from_slice(&0u16.to_be_bytes()); // cell_content_start = 0 (= page_size)
        page0.data[DB_HEADER_SIZE as usize + 8..DB_HEADER_SIZE as usize + 12]
            .copy_from_slice(&0u32.to_be_bytes()); // right_pointer = 0
        page0.dirty = true;

        self.write_file_at(0, &page0.data)?;
        // Sync only for durable opens. `:memory:` databases (skip_fsync)
        // are backed by a throwaway temp file that is deleted on drop —
        // an fsync here costs ~0.4 ms on CI Linux, ~1.5 ms on macOS and
        // ~10 ms on Windows per open, and buys nothing.
        if !self.skip_fsync.load(Ordering::Acquire) {
            self.store.sync_all()?;
        }
        self.n_pages.store(1, Ordering::Release);
        self.page_size.store(page_size, Ordering::Release);
        self.schema_cookie.store(0, Ordering::Release);
        self.is_new.store(false, Ordering::Release);
        // page0 was written directly to disk (not through the cache), so
        // no in-memory page is dirty. Keep the counter accurate.
        self.dirty_count_approx.store(0, Ordering::Release);
        Ok(())
    }

    fn read_header(&self) -> Result<()> {
        let mut header = [0u8; 100];
        let n = self.read_file_at(0, &mut header)?;
        if n < 100 {
            return Err(Error::corruption(format!(
                "file too small for header: {} bytes",
                n
            )));
        }
        let magic = FileHeader::magic(&header).copied();
        // v3 (legacy linked-list freelist): accepted and migrated — the
        // freelist rebuild happens in `migrate_legacy_freelist` (called
        // from `open`); the magic itself upgrades on the next flush.
        let is_v3 = magic == Some(crate::storage::page::DB_MAGIC_V3);
        if magic != Some(crate::storage::page::DB_MAGIC) && !is_v3 {
            // Distinguish "not a rustqlite file" from "old format version"
            // so users get an actionable message.
            let m = FileHeader::magic(&header)
                .map(|m| String::from_utf8_lossy(m).into_owned())
                .unwrap_or_default();
            if m.starts_with("RSQLDB") {
                return Err(Error::corruption(format!(
                    "unsupported database format version: {} (this build reads {};                      re-create the database or use the version that wrote it)",
                    m,
                    String::from_utf8_lossy(&crate::storage::page::DB_MAGIC)
                )));
            }
            return Err(Error::corruption("invalid magic header"));
        }
        let page_size = FileHeader::page_size(&header)?;
        // Page-codec marker (see set_codec): "RQLCODEC:<name>\0" at
        // bytes 72..100. Present → the file was written with a codec;
        // Database::open refuses, open_with_codec activates it.
        if let Some(marker) = codec_marker_name(&header) {
            *self.required_codec.lock() = Some(marker);
        }
        // Validate before trusting it: a corrupted page-size field (bit
        // flip, torn write) otherwise poisons every later Page allocation
        // (a 0-byte page panics on first page_type(), a 4864-byte page
        // misaligns every b-tree read). SQLite applies the same constraint
        // on open: power of two, 512..=65536.
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(Error::corruption(format!(
                "invalid page size {} (must be a power of two in 512..=65536)",
                page_size
            )));
        }
        self.page_size.store(page_size, Ordering::Release);
        let n_pages = FileHeader::db_size_pages(&header);
        let freelist_head = u32::from_le_bytes(header[20..24].try_into().unwrap());
        let freelist_count = u32::from_le_bytes(header[24..28].try_into().unwrap());
        let schema_cookie = FileHeader::schema_cookie(&header);
        self.n_pages.store(n_pages, Ordering::Release);
        self.freelist_head.store(freelist_head, Ordering::Release);
        self.freelist_count.store(freelist_count, Ordering::Release);
        self.schema_cookie.store(schema_cookie, Ordering::Release);

        // Verify file size matches the claimed page count.
        let actual_size = self.store.len()?;
        let expected_size = n_pages as u64 * page_size as u64;
        if actual_size < expected_size {
            return Err(Error::corruption(format!(
                "file size {} < expected {} (n_pages={}, page_size={})",
                actual_size, expected_size, n_pages, page_size
            )));
        }
        Ok(())
    }

    pub fn page_size(&self) -> u32 {
        self.page_size.load(Ordering::Acquire)
    }

    /// Set the page size for a database that has not been written yet
    /// (SQLite's `PRAGMA page_size = N` semantics: the value is only
    /// effective before the first content page is allocated). Returns
    /// true when applied.
    ///
    /// On a brand-new database the header page (page 0) already exists on
    /// disk at the OLD size — `Pager::open` initializes it eagerly. A
    /// size swap must therefore REWRITE page 0 at the new size (it holds
    /// no user data yet: n_cells = 0). Once any content page exists
    /// (n_pages > 1 or the dirty set is non-empty), the pragma is
    /// ignored — exactly like SQLite ignoring it mid-life without VACUUM.
    ///
    /// Accepted sizes: 4096, 8192, 16384, 32768, 65536.
    pub fn try_set_page_size(&self, size: u32) -> bool {
        use std::sync::atomic::Ordering;
        if !matches!(size, 4096 | 8192 | 16384 | 32768 | 65536) {
            return false;
        }
        let n = self.n_pages.load(Ordering::Acquire);
        if n > 1 {
            return false;
        }
        // Any dirty page implies content beyond the header — too late.
        if self.dirty_count_approx.load(Ordering::Acquire) > 0 {
            return false;
        }
        // Drop any cached page-0 (it was materialized at the old size) so
        // subsequent get_page(0) reads the rewritten bytes.
        {
            let mut cache = self.cache.write();
            cache.remove(0);
        }
        // Rewrite page 0 at the new size. The schema table is still empty
        // (n_cells = 0), so nothing else on the page needs preservation.
        let mut page0 = Page::new(0, size);
        FileHeader::write(&mut page0.data, size, 1, 0);
        page0.data[DB_HEADER_SIZE as usize] = crate::storage::page::PageType::LeafTable as u8;
        page0.data[DB_HEADER_SIZE as usize + 4..DB_HEADER_SIZE as usize + 6]
            .copy_from_slice(&0u16.to_be_bytes());
        page0.data[DB_HEADER_SIZE as usize + 6..DB_HEADER_SIZE as usize + 8]
            .copy_from_slice(&0u16.to_be_bytes());
        page0.data[DB_HEADER_SIZE as usize + 8..DB_HEADER_SIZE as usize + 12]
            .copy_from_slice(&0u32.to_be_bytes());
        // Truncate the file to exactly one page at the NEW size: the old
        // header page may have been larger (or the file smaller).
        let _ = self.store.set_len(size as u64);
        if self.write_file_at(0, &page0.data).is_err() {
            return false;
        }
        let _ = self.store.sync_all();
        self.page_size.store(size, Ordering::Release);
        true
    }

    pub fn n_pages(&self) -> u32 {
        self.n_pages.load(Ordering::Acquire)
    }

    /// Number of pages currently on the freelist (available for reuse by
    /// `allocate_page` without growing the file).
    pub fn freelist_count(&self) -> u32 {
        self.freelist_count.load(Ordering::Acquire)
    }

    /// Head page of the freelist (0 = empty). Read-only accessor for
    /// integrity checking and diagnostics.
    pub fn freelist_head(&self) -> PageId {
        self.freelist_head.load(Ordering::Acquire)
    }

    /// Metadata of the underlying database file (size, mtime). Used by
    /// `PRAGMA integrity_check` to validate the file's shape.
    pub fn file_metadata(&self) -> Result<std::fs::Metadata> {
        Ok(self.store.metadata()?)
    }

    pub fn schema_cookie(&self) -> u32 {
        self.schema_cookie.load(Ordering::Acquire)
    }

    /// Notify the pager that a mutating operation just happened (or is
    /// about to happen). Idempotent — calling it N times just bumps the
    /// counter N times. The counter is an upper bound on the number of
    /// dirty pages; the invariant `dirty_count_approx == 0 ⟹ no dirty
    /// pages` is what we rely on for `flush()`'s fast path.
    ///
    /// Cost: O(1). This replaced an O(cache_size) scan on every
    /// `Database::query()` call (the 9.2× point-lookup gap vs SQLite).
    pub fn note_write(&self) {
        self.dirty_count_approx.fetch_add(1, Ordering::Relaxed);
        self.write_version.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of cache-miss file reads so far (diagnostics).
    pub fn cache_misses(&self) -> u64 {
        self.cache_misses.load(Ordering::Relaxed)
    }

    /// Variant of `note_write` for mutations that CANNOT change any B+tree
    /// layout: same-size in-place payload patches. The dirty counter still
    /// moves (flush must write the page) but the write epoch does NOT —
    /// leaf first/last keys are untouched, so advisory leaf hints stay
    /// valid across bulk in-place UPDATEs. Any op that can move keys,
    /// split pages, or recycle pages must use the full `note_write`.
    pub fn note_write_in_place(&self) {
        self.dirty_count_approx.fetch_add(1, Ordering::Relaxed);
    }

    /// Access the cross-statement join-build cache (advisory,
    /// epoch-validated — see `storage/join_cache.rs`).
    pub(crate) fn join_build_cache(&self) -> &Mutex<crate::storage::join_cache::JoinBuildCache> {
        &self.join_cache
    }

    /// Current write version (see `write_version`). Readers compare this
    /// against the version their advisory caches were built at.
    #[inline]
    pub fn write_version(&self) -> u64 {
        self.write_version.load(Ordering::Relaxed)
    }

    /// Cache-invalidation epoch: packs (instance_id, write_version) so
    /// advisory caches can detect BOTH "content changed" and "this is a
    /// different database object than the one the cache was built for"
    /// with a single comparison. instance fits 16 bits (65k databases per
    /// process), version 48 bits (281 trillion writes) — wraparound is
    /// beyond any realistic workload.
    #[inline]
    pub fn write_epoch(&self) -> u64 {
        let v = self.write_version.load(Ordering::Relaxed);
        (self.instance_id << 48) | (v & 0xFFFF_FFFF_FFFF)
    }

    /// Notify the pager that a specific page is dirty. Adds the page ID
    /// to the dirty_pages set so `flush()` can iterate only dirty pages
    /// instead of scanning the entire cache.
    ///
    /// Idempotent — calling it twice with the same page ID is fine.
    /// Cost: O(1) HashSet insert.
    pub fn note_dirty(&self, id: PageId) {
        // Fast path: the same page is the last one we inserted — it's
        // already in the set (nothing but flush() removes entries, and
        // flush resets this hint). Bulk inserts dirty the SAME leaf
        // hundreds of times in a row; the lock + hash + insert was
        // ~40-60 ns per row.
        if self.last_noted_dirty.load(Ordering::Relaxed) == id {
            return;
        }
        let mut dp = self.dirty_pages.lock();
        dp.insert(id);
        self.last_noted_dirty.store(id, Ordering::Relaxed);
    }

    /// True if there might be dirty pages in the cache. O(1).
    pub fn has_dirty_pages(&self) -> bool {
        self.dirty_count_approx.load(Ordering::Acquire) > 0
    }

    /// True if `flush()` skips fsync (in-memory mode).
    pub fn is_in_memory(&self) -> bool {
        self.skip_fsync.load(Ordering::Acquire)
    }

    /// Upper-bound count of dirty pages since the last `flush()`. O(1).
    pub fn dirty_page_count(&self) -> usize {
        self.dirty_count_approx.load(Ordering::Acquire)
    }

    pub fn bump_schema_cookie(&self) -> Result<()> {
        let new_cookie = self
            .schema_cookie
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |x| {
                Some(x.wrapping_add(1))
            })
            .unwrap_or_else(|x| x);
        let mut header = [0u8; 100];
        self.read_file_at(0, &mut header)?;
        FileHeader::set_schema_cookie(&mut header, new_cookie.wrapping_add(1));
        let n_pages_val = self.n_pages.load(Ordering::Acquire);
        header[16..20].copy_from_slice(&n_pages_val.to_le_bytes());
        self.write_file_at(0, &header)?;
        Ok(())
    }

    /// Get a page by ID, reading from disk if not cached.
    ///
    /// Concurrency:
    ///  - Cache hit: brief read lock on the cache; clone the Arc; release.
    ///    Multiple readers can do this concurrently on different pages.
    ///  - Cache miss: brief read lock to check (double-checked), then brief
    ///    write lock to insert. Only one thread does the disk read; the other
    ///    waits on the write lock and then sees the page in cache.
    pub fn get_page(&self, id: PageId) -> Result<PageRef> {
        let n_pages_val = self.n_pages.load(Ordering::Acquire);
        if id >= n_pages_val && id != 0 {
            return Err(Error::corruption(format!(
                "page {} out of range (n_pages={})",
                id, n_pages_val
            )));
        }

        // Fast path: read lock, check cache.
        {
            let cache = self.cache.read();
            if let Some(page_ref) = cache.get(id).cloned() {
                drop(cache);
                // SAVEPOINT undo capture: the bytes at fetch time are the
                // pre-mutation state (every mutation get_page()s before it
                // modifies). One atomic load when no savepoint is active.
                if self.savepoint_depth.load(Ordering::Relaxed) > 0 {
                    self.capture_savepoint_undo(id, &page_ref);
                }
                return Ok(page_ref);
            }
        }

        // Slow path: cache miss — take write lock, double-check, then read from disk.
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        let page_ref = {
            let mut cache = self.cache.write();
            // Double-check: another thread may have inserted while we waited.
            if let Some(page_ref) = cache.get(id).cloned() {
                return Ok(page_ref);
            }
            let psz = self.page_size();
            let mut page = Page::new(id, psz);
            // WAL-served read: pages committed to the WAL since the last
            // checkpoint are the newest version — read the frame, not the
            // (stale) main-file page. One read-lock + map probe; falls
            // through to the main file when absent.
            let served_from_wal = {
                let wal_guard = self.wal.read();
                match wal_guard.as_ref() {
                    Some(state) => {
                        if let Some(&offset) = state.map.get(&id) {
                            state.wal.read_frame_at(offset, &mut page.data)?;
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                }
            };
            if !served_from_wal {
                let codec_active = self.codec.read().is_active();
                if codec_active {
                    let offset = id as u64 * psz as u64;
                    let mut raw = vec![0u8; psz as usize];
                    let n = self.read_file_at(offset, &mut raw)?;
                    if n != psz as usize {
                        return Err(Error::corruption(format!(
                            "short read on page {}: {} of {} bytes",
                            id, n, psz
                        )));
                    }
                    let decoded = {
                        let cs = self.codec.read();
                        cs.decode_page(id == 0, &raw, psz as usize)?
                    };
                    page.data.copy_from_slice(&decoded);
                } else {
                    let offset = id as u64 * psz as u64;
                    let n = self.read_file_at(offset, &mut page.data)?;
                    if n != psz as usize {
                        return Err(Error::corruption(format!(
                            "short read on page {}: {} of {} bytes",
                            id, n, psz
                        )));
                    }
                }
            }
            let page_ref = Arc::new(Mutex::new(page));
            self.maybe_evict_locked(&mut cache);
            cache.insert(id, page_ref.clone());
            self.lru.lock().push_back(id);
            page_ref
        };
        if self.savepoint_depth.load(Ordering::Relaxed) > 0 {
            self.capture_savepoint_undo(id, &page_ref);
        }
        Ok(page_ref)
    }

    /// Allocate a new page. Uses the freelist first, then extends the file.
    ///
    /// TRUNK FREELIST (v4 format, SQLite's design): the head trunk page
    /// holds `[next_trunk (4B), K (4B), K leaf page numbers (4B each)]`.
    /// Popping a leaf is ONE small write to the trunk (K -= 1) — the
    /// freed page's 4 KB body is never written. When a trunk empties
    /// (K == 0), the trunk page itself is handed out and the head moves
    /// to `next_trunk` — O(1), no re-linking writes.
    pub fn allocate_page(&self) -> Result<PageId> {
        let current_free_count = self.freelist_count.load(Ordering::Acquire);
        if current_free_count > 0 {
            let head = self.freelist_head.load(Ordering::Acquire);
            if head == 0 {
                return Err(Error::corruption(
                    "freelist count > 0 but freelist head is 0 (corrupt freelist)",
                ));
            }
            let page = self.get_page(head)?;
            // Read the trunk header: next trunk + entry count.
            let (next_trunk, k_entries) = {
                let borrowed = page.lock();
                if borrowed.data.len() < 8 {
                    return Err(Error::corruption(format!(
                        "freelist trunk {} too small",
                        head
                    )));
                }
                (
                    u32::from_le_bytes(borrowed.data[..4].try_into().unwrap()),
                    u32::from_le_bytes(borrowed.data[4..8].try_into().unwrap()),
                )
            };
            let freed: PageId;
            if k_entries > 0 {
                // Pop the LAST leaf entry: one 4-byte write to the trunk.
                let last = 8 + (k_entries as usize - 1) * 4;
                let page = self.get_page(head)?;
                let mut borrowed = page.lock();
                if last + 4 > borrowed.data.len() {
                    return Err(Error::corruption(format!(
                        "freelist trunk {} entry {} out of range",
                        head,
                        k_entries - 1
                    )));
                }
                freed = u32::from_le_bytes(borrowed.data[last..last + 4].try_into().unwrap());
                borrowed.data[4..8].copy_from_slice(&(k_entries - 1).to_le_bytes());
                borrowed.dirty = true;
                drop(borrowed);
                self.note_dirty(head);
            } else {
                // Empty trunk: the trunk page itself is the free page.
                // Hand it out; the chain continues at next_trunk. (A lone
                // empty trunk with next == 0 empties the freelist.)
                freed = head;
                self.freelist_head.store(next_trunk, Ordering::Release);
                let page = self.get_page(head)?;
                let mut borrowed = page.lock();
                borrowed.data.fill(0);
                borrowed.dirty = true;
                drop(borrowed);
                self.note_dirty(head);
            }
            // Use fetch_sub to safely decrement without overflow.
            self.freelist_count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |x| {
                    if x > 0 {
                        Some(x - 1)
                    } else {
                        None
                    }
                })
                .map_err(|_| Error::corruption("freelist underflow"))?;

            // Clear the handed-out page before reuse (the recycled body
            // may still hold the old tree's bytes in the cache or file).
            {
                let page = self.get_page(freed)?;
                let mut borrowed = page.lock();
                borrowed.data.fill(0);
                borrowed.dirty = true;
            }
            self.note_write();
            self.note_dirty(freed);
            Ok(freed)
        } else {
            // Extend the file.
            let id = self.n_pages.fetch_add(1, Ordering::AcqRel);
            let psz = self.page_size();
            let mut page = Page::new(id, psz);
            page.dirty = true;
            let page_ref = Arc::new(Mutex::new(page));
            {
                let mut cache = self.cache.write();
                self.maybe_evict_locked(&mut cache);
                cache.insert(id, page_ref);
            }
            self.lru.lock().push_back(id);
            self.note_write();
            self.note_dirty(id);
            Ok(id)
        }
    }

    /// Bulk `allocate_page`: hands out `n` pages, returning the live
    /// `PageRef` for each (no per-page `get_page` round trip). One
    /// cache-write critical section + one LRU lock for the whole batch
    /// instead of one of each per page — the overflow-chain writer (a
    /// 64 KB blob = ~17 pages) previously paid ~17 lock round trips per
    /// row. Freelist-reused pages are zeroed here exactly as in the
    /// single-page path (recycled bodies may still hold old bytes).
    pub fn allocate_pages(&self, n: usize) -> Result<Vec<(PageId, PageRef)>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut out: Vec<(PageId, PageRef)> = Vec::with_capacity(n);
        // Phase 1: freelist pops (exact single-page semantics, batched).
        for _ in 0..n {
            let current_free_count = self.freelist_count.load(Ordering::Acquire);
            if current_free_count == 0 {
                break;
            }
            let head = self.freelist_head.load(Ordering::Acquire);
            if head == 0 {
                return Err(Error::corruption(
                    "freelist count > 0 but freelist head is 0 (corrupt freelist)",
                ));
            }
            let page = self.get_page(head)?;
            let (next_trunk, k_entries) = {
                let borrowed = page.lock();
                if borrowed.data.len() < 8 {
                    return Err(Error::corruption(format!(
                        "freelist trunk {head} too small"
                    )));
                }
                (
                    u32::from_le_bytes(borrowed.data[..4].try_into().unwrap()),
                    u32::from_le_bytes(borrowed.data[4..8].try_into().unwrap()),
                )
            };
            let freed: PageId;
            if k_entries > 0 {
                let last = 8 + (k_entries as usize - 1) * 4;
                let page = self.get_page(head)?;
                let mut borrowed = page.lock();
                if last + 4 > borrowed.data.len() {
                    return Err(Error::corruption(format!(
                        "freelist trunk {head} entry {} out of range",
                        k_entries - 1
                    )));
                }
                freed = u32::from_le_bytes(borrowed.data[last..last + 4].try_into().unwrap());
                borrowed.data[4..8].copy_from_slice(&(k_entries - 1).to_le_bytes());
                borrowed.dirty = true;
                drop(borrowed);
                self.note_dirty(head);
            } else {
                freed = head;
                self.freelist_head.store(next_trunk, Ordering::Release);
                let page = self.get_page(head)?;
                let mut borrowed = page.lock();
                borrowed.data.fill(0);
                borrowed.dirty = true;
                drop(borrowed);
                self.note_dirty(head);
            }
            self.freelist_count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |x| {
                    if x > 0 {
                        Some(x - 1)
                    } else {
                        None
                    }
                })
                .map_err(|_| Error::corruption("freelist underflow"))?;
            let page_ref = self.get_page(freed)?;
            // Clear the handed-out page before reuse (the recycled body
            // may still hold the old tree's bytes in the cache or file) —
            // same contract as the single-page path.
            {
                let mut borrowed = page_ref.lock();
                borrowed.data.fill(0);
                borrowed.dirty = true;
            }
            self.note_dirty(freed);
            out.push((freed, page_ref));
        }
        // Phase 2: the remainder comes from file growth, created and
        // inserted under ONE cache-write lock.
        let remaining = n - out.len();
        if remaining > 0 {
            let first_id = self.n_pages.fetch_add(remaining as u32, Ordering::AcqRel);
            let psz = self.page_size();
            let mut new_pages: Vec<(PageId, PageRef)> = Vec::with_capacity(remaining);
            for i in 0..remaining {
                let id = first_id + i as u32;
                let mut page = Page::new(id, psz);
                page.dirty = true;
                new_pages.push((id, Arc::new(Mutex::new(page))));
            }
            {
                let mut cache = self.cache.write();
                self.maybe_evict_locked(&mut cache);
                for (id, page_ref) in &new_pages {
                    cache.insert(*id, page_ref.clone());
                }
            }
            {
                let mut lru = self.lru.lock();
                for (id, _) in &new_pages {
                    lru.push_back(*id);
                }
            }
            out.extend(new_pages);
        }
        if !out.is_empty() {
            self.note_write();
            let mut dp = self.dirty_pages.lock();
            for (id, _) in &out {
                dp.insert(*id);
            }
            self.last_noted_dirty.store(
                out.last().map(|(id, _)| *id).unwrap_or(u32::MAX),
                Ordering::Release,
            );
            self.dirty_count_approx.store(dp.len(), Ordering::Release);
        }
        Ok(out)
    }

    /// Mark a page as freed (record it on the trunk freelist).
    ///
    /// The freed page's BODY IS NEVER WRITTEN: it becomes a 4-byte entry
    /// in the head trunk's array (SQLite's freelist design). Only when
    /// the head trunk is full does the freed page itself become a new
    /// trunk page (one write per ~1022 frees at 4 KiB pages). This is
    /// what makes mass deletes cheap: freeing N pages costs O(N/1022)
    /// page writes instead of N.
    pub fn free_page(&self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(Error::InvalidArgument("cannot free page 0".into()));
        }
        // Savepoint undo: capture the freed page's pre-image BEFORE any
        // in-cache mutation below (rollback restores its old bytes even
        // though the durable copy is never touched).
        let page_ref = self.get_page(id)?;
        let head = self.freelist_head.load(Ordering::Acquire);
        let psz = self.page_size() as usize;
        let trunk_cap = (psz.saturating_sub(8)) / 4;
        let mut became_trunk = false;
        if head != 0 {
            let trunk_ref = self.get_page(head)?;
            let k_entries = {
                let borrowed = trunk_ref.lock();
                u32::from_le_bytes(borrowed.data[4..8].try_into().unwrap_or([0; 4]))
            };
            if (k_entries as usize) < trunk_cap {
                // Append the freed page id as a new leaf entry.
                let off = 8 + k_entries as usize * 4;
                let mut borrowed = trunk_ref.lock();
                if off + 4 <= borrowed.data.len() {
                    borrowed.data[off..off + 4].copy_from_slice(&id.to_le_bytes());
                    borrowed.data[4..8].copy_from_slice(&(k_entries + 1).to_le_bytes());
                    borrowed.dirty = true;
                    drop(borrowed);
                    self.note_dirty(head);
                } else {
                    became_trunk = true;
                }
            } else {
                became_trunk = true;
            }
        } else {
            became_trunk = true;
        }
        if became_trunk {
            // The freed page becomes the new head trunk: [next = old
            // head, K = 0]. This is the only body write, amortized over
            // ~1022 subsequent frees.
            {
                let mut borrowed = page_ref.lock();
                borrowed.data.fill(0);
                borrowed.data[..4].copy_from_slice(&head.to_le_bytes());
                borrowed.data[4..8].copy_from_slice(&0u32.to_le_bytes());
                borrowed.dirty = true;
            }
            self.note_dirty(id);
            self.freelist_head.store(id, Ordering::Release);
        } else {
            // Cache hygiene only: zero the in-cache copy so no code can
            // read the freed page's stale bytes as live data. NOT dirty —
            // the durable copy never changes for leaf entries.
            let mut borrowed = page_ref.lock();
            borrowed.data.fill(0);
            borrowed.dirty = false;
        }
        self.freelist_count.fetch_add(1, Ordering::AcqRel);
        self.note_write();
        Ok(())
    }

    /// Shrink the file by dropping a contiguous freed SUFFIX of pages.
    ///
    /// `freed` holds the page ids freed by the calling operation (bulk
    /// mass-delete). The largest `k` with every page in `[k, n_pages)`
    /// present in `freed` (and `k >= 1` — page 0 is the header/schema
    /// root) is truncated away entirely: the pages leave the cache, the
    /// LRU, the dirty set, the WAL committed map, and the freelist; no
    /// WAL frames are ever written for them; `n_pages` drops. This is
    /// SQLite's truncate-on-mass-delete optimization: `DELETE FROM t
    /// WHERE id > K` over an append-inserted table reclaims the file's
    /// tail outright instead of paying a freelist write + a 4 KB frame
    /// per freed page.
    ///
    /// The physical `set_len` is deferred to the next flush/checkpoint
    /// (`pending_truncate`) so the on-disk length moves with the commit
    /// that records the new page count. ROLLBACK is covered twice: the
    /// truncation floor never drops below the lowest active savepoint's
    /// base (pages below it hold undo pre-images ROLLBACK must be able to
    /// restore via `get_page`), and both rollback paths disarm the armed
    /// truncate so a post-rollback flush cannot shrink the file back.
    ///
    /// Returns the new page bound (0 when no truncation applied).
    pub fn truncate_tail(&self, freed: &[PageId]) -> Result<u32> {
        if freed.is_empty() {
            return Ok(0);
        }
        let set: PageIdSet = freed.iter().copied().collect();
        let n = self.n_pages.load(Ordering::Acquire);
        if n <= 1 {
            return Ok(0);
        }
        let mut k = n;
        while k > 1 && set.contains(&(k - 1)) {
            k -= 1;
        }
        // SAVEPOINT FLOOR: never truncate below the lowest active
        // savepoint's base n_pages. Pages below that bound were alive at
        // BEGIN time and are logged in at least one undo map — ROLLBACK
        // restores them through `get_page(id)`, which fails once n_pages
        // has rewound past the id. Pages at or above the floor were
        // allocated during the current transaction (no undo entries in
        // ANY level), so dropping them is always rollback-safe. The
        // un-truncated freed tail below the floor simply stays on the
        // freelist — the standard, correct shape (same as SQLite without
        // its truncate optimization).
        let floor = self.savepoint_min_base.load(Ordering::Acquire);
        if floor != u32::MAX && k < floor {
            k = floor;
        }
        if k == n {
            return Ok(0);
        }

        // --- scrub the TRUNK freelist: remove every entry >= k ---
        // A freelist page can never be referenced by a live btree, so
        // dropping all entries >= k is always sound. Trunk pages >= k are
        // dropped whole (page + entries); trunk pages < k keep their
        // entries < k (compacted in place); the chain is re-linked.
        {
            let mut cur = self.freelist_head.load(Ordering::Acquire);
            let mut removed: u32 = 0;
            let mut new_head: PageId = 0;
            let mut prev_kept: PageId = 0;
            // Cycle guard: the trunk chain can't be longer than the old
            // page count; a corrupt file stops the walk.
            let mut steps = (self.freelist_count.load(Ordering::Acquire) + n).max(1);
            while cur != 0 && steps > 0 {
                steps -= 1;
                // (next_trunk, K) + the K leaf entries of this trunk.
                let (next, k_entries, entries) = {
                    let page = self.get_page(cur)?;
                    let borrowed = page.lock();
                    let next = u32::from_le_bytes(borrowed.data[..4].try_into().unwrap_or([0; 4]));
                    let kn = u32::from_le_bytes(borrowed.data[4..8].try_into().unwrap_or([0; 4]))
                        as usize;
                    let kn = kn.min((borrowed.data.len().saturating_sub(8)) / 4);
                    let entries: Vec<u32> = (0..kn)
                        .map(|i| {
                            u32::from_le_bytes(
                                borrowed.data[8 + i * 4..12 + i * 4]
                                    .try_into()
                                    .unwrap_or([0; 4]),
                            )
                        })
                        .collect();
                    (next, kn, entries)
                };
                if cur >= k {
                    // The whole trunk page is gone: it + its entries all
                    // count as removed freelist pages.
                    removed += 1 + k_entries as u32;
                } else {
                    let kept: Vec<u32> = entries.into_iter().filter(|&e| e < k).collect();
                    removed += (k_entries - kept.len()) as u32;
                    if kept.len() != k_entries {
                        // Rewrite the compacted entry array + K.
                        let page = self.get_page(cur)?;
                        let mut borrowed = page.lock();
                        borrowed.data[4..8].copy_from_slice(&(kept.len() as u32).to_le_bytes());
                        for (i, e) in kept.iter().enumerate() {
                            borrowed.data[8 + i * 4..12 + i * 4].copy_from_slice(&e.to_le_bytes());
                        }
                        borrowed.dirty = true;
                        drop(borrowed);
                        self.note_dirty(cur);
                    }
                    if new_head == 0 {
                        new_head = cur;
                    } else {
                        // Re-link the previous kept trunk to this one.
                        let page = self.get_page(prev_kept)?;
                        let mut borrowed = page.lock();
                        borrowed.data[..4].copy_from_slice(&cur.to_le_bytes());
                        borrowed.dirty = true;
                        drop(borrowed);
                        self.note_dirty(prev_kept);
                    }
                    prev_kept = cur;
                }
                cur = next;
            }
            self.freelist_head.store(new_head, Ordering::Release);
            if removed > 0 {
                self.freelist_count.fetch_sub(removed, Ordering::AcqRel);
            }
        }

        // --- drop [k, n) from the cache, LRU, dirty set, WAL map ---
        {
            let mut cache = self.cache.write();
            let slots_len = cache.slots.len();
            let dense_end = (n as usize).min(slots_len);
            for idx in (k as usize)..dense_end {
                cache.slots[idx] = None;
            }
            cache.count = cache.slots.iter().filter(|s| s.is_some()).count() + cache.overflow.len();
            cache.overflow.retain(|&id, _| id < k);
            self.lru.lock().retain(|&id| id < k);
            let mut dp = self.dirty_pages.lock();
            dp.retain(|&id| id < k);
            self.last_noted_dirty.store(u32::MAX, Ordering::Release);
            let mut wal = self.wal.write();
            if let Some(state) = wal.as_mut() {
                state.map.retain(|&id, _| id < k);
            }
        }

        // --- rewind the page count + arm the physical truncate ---
        self.n_pages.store(k, Ordering::Release);
        self.pending_truncate.store(k, Ordering::Release);
        // Invalidate advisory caches (leaf hints may point at truncated
        // pages; the epoch check re-fetches).
        self.note_write();
        // Memory stores: shrink the backing Vec now (it is only snapshot
        // scratch — the cache is the store — so there is nothing to
        // defer).
        if self.store.is_memory() {
            self.store.set_len(k as u64 * self.page_size() as u64)?;
            self.pending_truncate.store(0, Ordering::Release);
        }
        Ok(k)
    }

    /// Apply the armed physical file truncation (called from the flush
    /// paths, after the new header/page images are durable).
    fn apply_pending_truncate(&self) -> Result<()> {
        let k = self.pending_truncate.load(Ordering::Acquire);
        if k == 0 {
            return Ok(());
        }
        let want_len = k as u64 * self.page_size() as u64;
        if let Ok(len) = self.store.len() {
            if len > want_len {
                self.store.set_len(want_len)?;
            }
        }
        self.pending_truncate.store(0, Ordering::Release);
        Ok(())
    }

    /// Flush all dirty pages to disk and sync.
    /// Switch to WAL mode (`PRAGMA journal_mode = WAL`).
    ///
    /// First flushes any dirty pages to the main file (the switch point),
    /// then opens (or recovers) the `-wal` file alongside the database.
    /// Committed frames left over from a previous session become visible
    /// through the page map — this IS crash recovery: un-checkpointed
    /// committed data is served from the WAL.
    /// Activate a page codec (`PRAGMA codec = <name>` /
    /// `Database::open_with_codec`). Errors when WAL mode is active, or
    /// when the file carries a DIFFERENT codec's marker. When activating
    /// on a codec-less file, the marker is written into the in-cache page
    /// 0 (flushed with the next commit) so reopen knows what to require.
    pub fn set_codec(
        &self,
        codec: Option<std::sync::Arc<dyn crate::plugin::PageCodec>>,
    ) -> Result<()> {
        if codec.is_some() && self.wal_enabled() {
            return Err(crate::error::Error::semantic(
                "page codecs require journal_mode=delete (WAL frames are not encoded)",
            ));
        }
        if let Some(c) = &codec {
            if let Some(required) = self.required_codec.lock().clone() {
                if !required.eq_ignore_ascii_case(c.name()) {
                    return Err(crate::error::Error::semantic(format!(
                        "database was written with codec '{}', refusing codec '{}'",
                        required,
                        c.name()
                    )));
                }
            }
        }
        {
            let mut cs = self.codec.write();
            cs.active = codec;
        }
        // Codec-coded v3 file: the deferred freelist migration now runs
        // with the codec armed (the open-time pass skipped it — freelist
        // pages decode only through the active codec). Raw-magic check
        // first: a v4 file (or an already-migrated one) must not re-enter.
        {
            let mut magic8 = [0u8; 8];
            let is_v3 = self
                .read_file_at(0, &mut magic8)
                .map(|n| n == 8 && magic8 == crate::storage::page::DB_MAGIC_V3)
                .unwrap_or(false);
            if is_v3 {
                self.migrate_legacy_freelist_inner()?;
            }
        }
        // Write / clear the marker on the in-cache page 0.
        let page0 = self.get_page(0)?;
        {
            let mut b = page0.lock();
            let name = self.codec.read().active_name().map(|n| n.to_string());
            match name {
                Some(n) => write_codec_marker(&mut b.data, &n),
                None => clear_codec_marker(&mut b.data),
            }
            b.dirty = true;
        }
        self.note_dirty(0);
        if self.lazy_writeback.load(Ordering::Acquire) {
            // In-memory mode: force the marker out so reopen-by-path sees it.
            let _ = self.flush();
        } else {
            let _ = self.flush();
        }
        Ok(())
    }

    /// Active codec name, if any.
    pub fn codec_name(&self) -> Option<String> {
        self.codec.read().active_name().map(|s| s.to_string())
    }

    /// Codec required by the file's marker (read at open).
    pub fn required_codec(&self) -> Option<String> {
        self.required_codec.lock().clone()
    }

    /// Write one page through the active codec (or raw when none).
    fn codec_write_page(&self, id: PageId, data: &[u8]) -> Result<()> {
        let psz = self.page_size();
        let offset = id as u64 * psz as u64;
        let out = {
            let cs = self.codec.read();
            cs.encode_page(id == 0, data)?
        };
        self.write_file_at(offset, &out)
    }

    pub fn enable_wal(&self) -> Result<()> {
        {
            let guard = self.wal.read();
            if guard.is_some() {
                return Ok(()); // already in WAL mode
            }
        }
        if self.codec.read().is_active() {
            return Err(crate::error::Error::semantic(
                "journal_mode=WAL is unavailable while a page codec is active",
            ));
        }
        // Pure in-memory store: WAL becomes a LOGICAL mode. There is no
        // main file, so there is nothing for a sidecar -wal file to
        // protect — crash recovery and durable frames are meaningless for
        // a database that vanishes on drop. Record the mode (the pragma
        // round-trip must report "wal") and skip every file operation.
        if self.store.is_memory() {
            self.memory_wal.store(true, Ordering::Release);
            return Ok(());
        }
        // Flush pending dirty pages in DELETE mode first.
        self.flush()?;
        let mut wal = crate::storage::wal::Wal::open(&self.path, self.page_size())?;
        let map: std::collections::HashMap<PageId, u64, PageIdHashBuild> =
            wal.committed_page_map()?.into_iter().collect();
        let n = wal.n_frames();
        let mut guard = self.wal.write();
        *guard = Some(WalState { wal, map });
        drop(guard);
        // Reload the header through the WAL (page 0 may be newer there):
        // n_pages / freelist / schema_cookie must reflect committed state.
        if n > 0 {
            self.reload_header_from_committed()?;
            // A pre-truncation crash can leave committed frames for pages
            // beyond the committed n_pages (a tail truncation armed but
            // never checkpointed). Drop them: get_page's bounds check
            // makes them unreachable anyway, and a later checkpoint would
            // transiently re-extend the file while writing them back.
            let bound = self.n_pages.load(Ordering::Acquire);
            let mut guard = self.wal.write();
            if let Some(state) = guard.as_mut() {
                state.map.retain(|&id, _| id < bound);
            }
        }
        Ok(())
    }

    /// Switch back to DELETE mode (`PRAGMA journal_mode = DELETE`):
    /// checkpoint the WAL into the main file, then remove it.
    pub fn disable_wal(&self) -> Result<()> {
        // Logical WAL on a memory store: just clear the flag.
        if self.store.is_memory() {
            self.memory_wal.store(false, Ordering::Release);
            return Ok(());
        }
        if self.wal.read().is_none() {
            return Ok(());
        }
        self.checkpoint_wal()?;
        {
            let mut guard = self.wal.write();
            *guard = None;
        }
        let _ = std::fs::remove_file(crate::storage::wal::wal_path_for(&self.path));
        Ok(())
    }

    /// Is the pager in WAL mode?
    pub fn wal_enabled(&self) -> bool {
        self.wal.read().is_some() || self.memory_wal.load(Ordering::Acquire)
    }

    /// Copy every committed WAL page back into the main database file,
    /// sync it, and reset the WAL. Readers stay correct throughout: the
    /// map is dropped only after the main file holds every page.
    pub fn checkpoint_wal(&self) -> Result<()> {
        let mut guard = self.wal.write();
        let Some(state) = guard.as_mut() else {
            return Ok(());
        };
        let psz = self.page_size();
        // Sort page ids for sequential main-file writes.
        let mut ids: Vec<PageId> = state.map.keys().copied().collect();
        ids.sort_unstable();
        let mut buf = vec![0u8; psz as usize];
        // Header (page 0) goes LAST (crash safety — see
        // flush_inner_delete's ordering contract): every other page is
        // copied back before the n_pages-bearing header commit.
        for id in ids.iter().copied().filter(|&id| id != 0) {
            let offset = state.map[&id];
            state.wal.read_frame_at(offset, &mut buf)?;
            self.write_file_at(id as u64 * psz as u64, &buf)?;
        }
        if let Some(&offset) = state.map.get(&0) {
            state.wal.read_frame_at(offset, &mut buf)?;
            self.write_file_at(0, &buf)?;
        }
        // Ensure the main file covers every page we just wrote (the file
        // may be shorter than the committed page count — new pages live
        // only in the WAL until now), and apply any armed mass-delete
        // truncation (the file tail reclaims with the commit).
        let want_len = self.n_pages.load(Ordering::Acquire) as u64 * psz as u64;
        let cur_len = self.store.len()?;
        if want_len != cur_len {
            self.store.set_len(want_len)?;
        }
        self.pending_truncate.store(0, Ordering::Release);
        if !self.skip_fsync.load(Ordering::Acquire) {
            self.store.sync_all()?;
        }
        state.wal.reset()?;
        state.map.clear();
        Ok(())
    }

    /// WAL-mode commit: append every dirty page as a frame, mark the last
    /// frame as the commit frame, sync per `PRAGMA synchronous`, and
    /// auto-checkpoint when the WAL grows past the threshold.
    fn flush_wal(&self) -> Result<()> {
        let psz = self.page_size();

        // --- header page: refresh in-cache page 0 and mark it dirty ---
        let n_pages_val = self.n_pages.load(Ordering::Acquire);
        let freelist_head_val = self.freelist_head.load(Ordering::Acquire);
        let freelist_count_val = self.freelist_count.load(Ordering::Acquire);
        let schema_cookie_val = self.schema_cookie.load(Ordering::Acquire);
        // Page 0 carries the header (n_pages, freelist, cookie). Refresh
        // it through the normal page path (cache → WAL → file) so the
        // newest committed version is the base.
        let page0 = self.get_page(0)?;
        {
            let mut borrowed = page0.lock();
            FileHeader::write(&mut borrowed.data, psz, n_pages_val, schema_cookie_val);
            borrowed.data[20..24].copy_from_slice(&freelist_head_val.to_le_bytes());
            borrowed.data[24..28].copy_from_slice(&freelist_count_val.to_le_bytes());
            borrowed.dirty = true;
        }
        self.dirty_pages.lock().insert(0);

        // --- collect dirty page ids ---
        let dirty_ids: Vec<PageId> = {
            let mut set = self.dirty_pages.lock();
            let ids = set.drain().collect::<Vec<_>>();
            // The set was drained — reset the last-noted hint so a page
            // re-dirtied after this flush is not silently skipped by
            // note_dirty's fast path (its content must reach the next WAL
            // commit).
            self.last_noted_dirty.store(u32::MAX, Ordering::Release);
            ids
        };
        if dirty_ids.is_empty() {
            // Only the header changed but nothing was dirty — nothing to
            // commit. (Cannot normally happen: dirty_count > 0 implies
            // dirty pages.)
            self.dirty_count_approx.store(0, Ordering::Release);
            return Ok(());
        }

        // --- append frames under the WAL write lock ---
        let mut frame_offsets: Vec<(PageId, u64)> = Vec::with_capacity(dirty_ids.len());
        let sync_needed;
        {
            let mut guard = self.wal.write();
            let Some(state) = guard.as_mut() else {
                // Mode flipped to DELETE between the dispatch check and
                // here (single writer, but be safe): fall back.
                drop(guard);
                return self.flush_inner_delete();
            };
            let mut scratch = vec![0u8; psz as usize];
            for (i, id) in dirty_ids.iter().enumerate() {
                let page_ref = self.cache.read().get(*id).cloned();
                let Some(page_ref) = page_ref else { continue };
                {
                    let mut borrowed = page_ref.lock();
                    if !borrowed.dirty {
                        continue;
                    }
                    scratch.copy_from_slice(&borrowed.data);
                    borrowed.dirty = false;
                }
                let is_last = i + 1 == dirty_ids.len();
                let offset = state.wal.append(*id, &scratch, is_last)?;
                frame_offsets.push((*id, offset));
            }
            // Durability point. synchronous=NORMAL (the recommended WAL
            // setting) skips this fsync: commits survive process crashes
            // (the OS page cache holds them) but not power loss; the next
            // checkpoint makes them fully durable — SQLite's documented
            // trade-off.
            let sync_mode = self.synchronous.load(Ordering::Acquire);
            sync_needed = sync_mode >= 2 && !self.skip_fsync.load(Ordering::Acquire);
            if sync_needed {
                state.wal.sync()?;
            }
            for (id, off) in &frame_offsets {
                state.map.insert(*id, *off);
            }
        }

        self.dirty_count_approx.store(0, Ordering::Release);

        // --- auto-checkpoint ---
        let frames = {
            self.wal
                .read()
                .as_ref()
                .map(|s| s.wal.n_frames())
                .unwrap_or(0)
        };
        // A tail truncation does NOT force a checkpoint: the in-memory
        // state (n_pages, WAL map, cache) is already consistent, the WAL
        // holds no frames for truncated pages (scrubbed), and the file
        // length reclaims at the next checkpoint/close. Forcing one here
        // would copy the WHOLE accumulated WAL back to the main file
        // inside the DELETE's measured time — SQLite doesn't checkpoint
        // on mass delete either (its file keeps the high-water length
        // until a later checkpoint; the committed page-0 frame carries
        // the smaller n_pages).
        if frames >= WAL_AUTOCHECKPOINT_FRAMES {
            self.checkpoint_wal()?;
        }
        Ok(())
    }

    /// Re-read the file header through the committed page map (WAL) and
    /// refresh n_pages / freelist / schema_cookie in memory. Used after
    /// WAL recovery on open.
    fn reload_header_from_committed(&self) -> Result<()> {
        let psz = self.page_size();
        let mut header = vec![0u8; psz as usize];
        let got = {
            let guard = self.wal.read();
            match guard.as_ref() {
                Some(state) => match state.map.get(&0) {
                    Some(&offset) => {
                        state.wal.read_frame_at(offset, &mut header)?;
                        true
                    }
                    None => false,
                },
                None => false,
            }
        };
        if !got {
            self.read_file_at(0, &mut header)?;
        }
        let n_pages = u32::from_le_bytes(header[16..20].try_into().unwrap());
        let freelist_head = u32::from_le_bytes(header[20..24].try_into().unwrap());
        let freelist_count = u32::from_le_bytes(header[24..28].try_into().unwrap());
        let schema_cookie = u32::from_le_bytes(header[28..32].try_into().unwrap());
        self.n_pages.store(n_pages, Ordering::Release);
        self.freelist_head.store(freelist_head, Ordering::Release);
        self.freelist_count.store(freelist_count, Ordering::Release);
        self.schema_cookie.store(schema_cookie, Ordering::Release);
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        // LAZY WRITE-BACK MODE (in-memory databases): pure no-op. Do NOT
        // clear the dirty bookkeeping — the dirty_pages set and count are
        // the record of what a future REAL flush (flush_before_snapshot at
        // BEGIN, or eviction) must write. Clearing them here while pages
        // keep their in-cache `.dirty` flag would orphan those pages: the
        // next real flush would skip them, and ROLLBACK (which restores by
        // clearing the cache and re-reading from the file) would hit short
        // reads. All reads go through the cache, so skipping the file
        // writes is safe; the temp file is deleted on close anyway.
        if self.lazy_writeback.load(Ordering::Acquire) {
            return Ok(());
        }
        // O(1) fast path: if no writes happened since the last flush, skip
        // the entire flush (including sync_all).
        if self.dirty_count_approx.load(Ordering::Acquire) == 0 {
            return Ok(());
        }

        // WAL mode: commits append dirty pages as frames to the -wal file
        // instead of writing the main database file. Readers see the
        // newest page versions through the committed-page map; a
        // checkpoint later copies them back to the main file.
        if self.wal.read().is_some() {
            return self.flush_wal();
        }
        self.flush_inner_delete()
    }

    /// DELETE-mode flush body: scattered page writes + header refresh +
    /// fsync.
    ///
    /// ORDERING CONTRACT (crash safety): the header — the only place the
    /// committed `n_pages` lives — is written LAST, after every other
    /// page and before the truncating `set_len`. A crash at ANY point
    /// then leaves the file at least as long as the header claims
    /// (extra tail bytes are ignored at open), never shorter; the
    /// "file size < n_pages * page_size" corruption is unreachable. The
    /// previous header-first order left a window (header written, new
    /// pages not yet) that the OOM fault-injection suite hits.
    fn flush_inner_delete(&self) -> Result<()> {
        let n_pages_val = self.n_pages.load(Ordering::Acquire);
        let freelist_head_val = self.freelist_head.load(Ordering::Acquire);
        let freelist_count_val = self.freelist_count.load(Ordering::Acquire);
        let schema_cookie_val = self.schema_cookie.load(Ordering::Acquire);

        // Refresh the in-cache page 0 header bytes now (they ride the
        // dirty-page writes below); the physical write happens LAST.
        let psz = self.page_size();
        let mut header_direct: Option<Vec<u8>> = None;
        let page0_in_cache = self.cache.read().contains_key(0);
        if page0_in_cache {
            let page0 = self.cache.read().get(0).cloned();
            if let Some(page0) = page0 {
                let mut borrowed = page0.lock();
                FileHeader::write(&mut borrowed.data, psz, n_pages_val, schema_cookie_val);
                borrowed.data[20..24].copy_from_slice(&freelist_head_val.to_le_bytes());
                borrowed.data[24..28].copy_from_slice(&freelist_count_val.to_le_bytes());
                borrowed.dirty = true;
                // Mark page 0 as dirty in the dirty_pages set so the
                // header write below (OUT OF ORDER — last) finds it.
                drop(borrowed);
                self.dirty_pages.lock().insert(0);
            }
        } else {
            // Page 0 not in cache — read + modify now, WRITE last.
            let mut header = vec![0u8; psz as usize];
            if self.codec.read().is_active() {
                let mut raw = vec![0u8; psz as usize];
                self.read_file_at(0, &mut raw)?;
                let decoded = {
                    let cs = self.codec.read();
                    cs.decode_page(true, &raw, psz as usize)?
                };
                header.copy_from_slice(&decoded);
            } else {
                self.read_file_at(0, &mut header)?;
            }
            FileHeader::write(&mut header, psz, n_pages_val, schema_cookie_val);
            header[20..24].copy_from_slice(&freelist_head_val.to_le_bytes());
            header[24..28].copy_from_slice(&freelist_count_val.to_le_bytes());
            header_direct = Some(header);
        }

        // Flush dirty pages — use the dirty_pages set so this is O(dirty_count),
        // not O(cache_size). This is the key optimization: a 10k-page cache with
        // only 1-2 dirty pages per statement used to scan 10k page-locks per
        // flush; now we iterate only the dirty set (~1-2 entries).
        // Page 0 is deliberately EXCLUDED here (it goes last).
        let dirty_ids: Vec<PageId> = {
            let mut set = self.dirty_pages.lock();
            let ids = set.drain().filter(|&id| id != 0).collect::<Vec<_>>();
            // Reset the last-noted hint: the set was drained, so a page
            // re-dirtied after this flush MUST re-enter the set or its new
            // content would never reach the file (the hint fast-path
            // assumed "flush resets this" but nothing did — a page updated
            // twice across two autocommit statements silently kept its
            // first version on disk).
            self.last_noted_dirty.store(u32::MAX, Ordering::Release);
            ids
        };

        for id in dirty_ids {
            // Use a single cache read lock to look up the page; clone the Arc
            // and release the lock before doing I/O.
            let page_ref = self.cache.read().get(id).cloned();
            if let Some(page_ref) = page_ref {
                let mut borrowed = page_ref.lock();
                if borrowed.dirty {
                    self.codec_write_page(id, &borrowed.data)?;
                    borrowed.dirty = false;
                }
            }
        }

        // ---- header LAST: every other page is durable; NOW commit the
        // new page count. A crash before this point leaves the old
        // (smaller or equal) n_pages with a file that covers it. ----
        if let Some(header) = &header_direct {
            self.codec_write_page(0, header)?;
        } else {
            let page_ref = self.cache.read().get(0).cloned();
            if let Some(page_ref) = page_ref {
                let mut borrowed = page_ref.lock();
                if borrowed.dirty {
                    self.codec_write_page(0, &borrowed.data)?;
                    borrowed.dirty = false;
                }
            }
        }
        // If skip_fsync is set (in-memory mode), sync_all is a no-op on
        // tmpfs anyway, but the syscall round-trip still costs ~5-50 µs.
        // Skip the entire call to make in-memory mode match SQLite's `:memory:`
        // performance.
        if !self.skip_fsync.load(Ordering::Acquire) {
            self.store.sync_all()?;
        }
        // Mass-delete tail truncation: shrink the file with the commit.
        self.apply_pending_truncate()?;
        self.dirty_count_approx.store(0, Ordering::Release);
        Ok(())
    }

    /// Set whether `flush()` should skip `file.sync_all()`. Used by
    /// `Database::open_in_memory` to make `:memory:` databases skip fsyncs
    /// (since the file is on tmpfs and will be deleted on close, durability
    /// is irrelevant).
    pub fn set_skip_fsync(&self, skip: bool) {
        self.skip_fsync.store(skip, Ordering::Release);
    }

    /// Enable lazy write-back mode (in-memory databases): `flush()` becomes
    /// O(1) (no file writes); dirty pages spill to the backing temp file
    /// only on cache eviction. See the field docs for rationale.
    pub fn set_lazy_writeback(&self, enabled: bool) {
        self.lazy_writeback.store(enabled, Ordering::Release);
    }

    /// Called at BEGIN (before taking the rollback snapshot).
    ///
    /// In lazy write-back mode, dirty pages normally never reach the file —
    /// but ROLLBACK restores by clearing the page cache and re-reading
    /// pages from the file, so the file MUST hold the pre-BEGIN state.
    /// This forces a real write-back of all dirty pages. It runs once per
    /// BEGIN, not per statement, so the amortized cost is negligible
    /// compared to the per-autocommit-statement writes it eliminates.
    ///
    /// In normal (non-lazy) mode this is a no-op: flush() already keeps the
    /// file current.
    pub fn flush_before_snapshot(&self) -> Result<()> {
        if self.lazy_writeback.load(Ordering::Acquire) {
            // CACHE-IS-THE-STORE (in-memory databases): the backing image
            // is never consulted — reads always hit the unbounded cache and
            // ROLLBACK restores through the `__begin__` savepoint's pre-
            // images (captured at first fetch, non-destructive). Writing
            // the image here would only grow RSS by a second full copy of
            // the DB. File-backed lazy pagers keep the write-back.
            if self.store.is_memory() {
                return Ok(());
            }
            // Temporarily disable lazy mode and run the real flush with the
            // dirty-count fast path bypassed: in lazy mode the count and the
            // dirty_pages set can diverge from the pages' actual .dirty
            // flags, so the only reliable "nothing to write" check is the
            // set itself (which the flush body drains).
            self.lazy_writeback.store(false, Ordering::Release);
            let count_checkpoint = self.dirty_count_approx.swap(1, Ordering::Release);
            let result = self.flush();
            // Restore a sane count (flush() reset it to 0; if the real
            // flush wrote nothing because the set was empty, keep whatever
            // the pre-call state implied — 0 is fine either way since the
            // set is now empty too).
            let _ = count_checkpoint;
            self.lazy_writeback.store(true, Ordering::Release);
            result
        } else {
            Ok(())
        }
    }

    /// Rollback to the state captured by `PagerSnapshot::capture` at BEGIN.
    ///
    /// This discards all in-memory dirty pages (their contents were never
    /// written to disk during the transaction — see `ExecContext::in_transaction`
    /// guard), restores the pager's mutable metadata to the pre-BEGIN values,
    /// and truncates the file back to `n_pages` if the transaction allocated
    /// new pages.
    pub fn rollback_to(&self, snap: &PagerSnapshot) -> Result<()> {
        // 1. Drop the entire cache.
        {
            let mut cache = self.cache.write();
            cache.clear();
        }
        self.lru.lock().clear();
        self.dirty_pages.lock().clear();
        // Rollback RESTORES older page content — visible state changes, so
        // advisory caches (btree leaf hints) must be invalidated even
        // though no note_write ran for it.
        self.write_version.fetch_add(1, Ordering::Relaxed);
        // The set was drained — the last-noted hint is stale (that page is
        // no longer in the set). Reset so future note_dirty calls don't
        // skip a needed insert.
        self.last_noted_dirty.store(u32::MAX, Ordering::Release);

        // 2. Restore mutable metadata. Any armed tail truncation belonged
        // to the rolled-back work — disarm it.
        self.pending_truncate.store(0, Ordering::Release);
        self.n_pages.store(snap.n_pages, Ordering::Release);
        self.freelist_head
            .store(snap.freelist_head, Ordering::Release);
        self.freelist_count
            .store(snap.freelist_count, Ordering::Release);
        self.schema_cookie
            .store(snap.schema_cookie, Ordering::Release);

        // 3. Truncate the file back if pages were allocated during the txn.
        let target_size = snap.n_pages as u64 * self.page_size() as u64;
        let current_size = self.store.len()?;
        if current_size > target_size {
            self.store.set_len(target_size)?;
        }

        // 4. Reset the dirty counter.
        self.dirty_count_approx.store(0, Ordering::Release);
        Ok(())
    }

    /// Take a snapshot of the pager's mutable state, for use with ROLLBACK.
    pub fn snapshot(&self) -> PagerSnapshot {
        PagerSnapshot::capture(self)
    }

    /// Evict pages from the cache until we're under capacity.
    /// Caller must hold the cache write lock.
    fn maybe_evict_locked(&self, cache: &mut PageCache) {
        // CACHE-IS-THE-STORE (in-memory databases): never evict. The page
        // cache IS the database image — the backing Vec is never read back
        // (every page enters the cache at first touch and stays), so
        // eviction buys nothing and costs a 4 KB write-back + a 4 KB
        // re-read on the next visit. This is SQLite's own `:memory:`
        // design: the pager array is the whole DB. It also caps RSS at
        // DB size (no double image + cache representation).
        if self.store.is_memory() {
            return;
        }
        // Safety bound: if every cached page is dirty and unwritable (or
        // lazy_writeback is off), the loop below would otherwise spin
        // forever moving dirty pages to the back. `attempts` caps it.
        let mut attempts = cache.len();
        while cache.len() >= self.cache_capacity && attempts > 0 {
            attempts -= 1;
            let evict_id = {
                let mut lru = self.lru.lock();
                match lru.front().copied() {
                    Some(id) => {
                        lru.pop_front();
                        id
                    }
                    None => break,
                }
            };
            let should_evict = match cache.get(evict_id) {
                Some(p) => {
                    let mut pg = p.lock();
                    if pg.dirty {
                        if self.lazy_writeback.load(Ordering::Acquire) {
                            // Lazy write-back: the page was never written by
                            // flush() — write it NOW, then it's safe to evict.
                            // Errors: keep the page (retry later) rather than
                            // losing data.
                            let offset = evict_id as u64 * self.page_size() as u64;
                            if self.write_file_at(offset, &pg.data).is_ok() {
                                pg.dirty = false;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                }
                None => true,
            };
            if should_evict {
                cache.remove(evict_id);
            } else {
                // Move dirty page to the back and try the next one.
                self.lru.lock().push_back(evict_id);
            }
        }
    }

    #[allow(dead_code)]
    fn touch_lru(&self, id: PageId) {
        let mut lru = self.lru.lock();
        if let Some(pos) = lru.iter().position(|x| *x == id) {
            lru.remove(pos);
            lru.push_back(id);
        }
    }

    /// Total bytes used by the cache (for instrumentation).
    /// FOREIGN KEY enforcement toggle (PRAGMA foreign_keys = ON/OFF).
    pub fn set_foreign_keys_enabled(&self, enabled: bool) {
        self.foreign_keys_enabled
            .store(enabled, std::sync::atomic::Ordering::Release);
    }

    pub fn foreign_keys_enabled(&self) -> bool {
        self.foreign_keys_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Advisory PRAGMA locking_mode toggle — see the field docs.
    pub fn set_locking_mode_exclusive(&self, exclusive: bool) {
        self.locking_mode_exclusive
            .store(exclusive, std::sync::atomic::Ordering::Release);
    }

    pub fn locking_mode_exclusive(&self) -> bool {
        self.locking_mode_exclusive
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// PRAGMA synchronous level: 0=OFF, 1=NORMAL, 2=FULL (default).
    pub fn set_synchronous(&self, level: u8) {
        self.synchronous.store(level.min(3), Ordering::Release);
    }

    pub fn synchronous(&self) -> u8 {
        self.synchronous.load(Ordering::Acquire)
    }

    /// Trigger recursion toggle (PRAGMA recursive_triggers). Default OFF —
    /// SQLite's default: triggers do not re-fire from inside another
    /// trigger's body.
    pub fn set_recursive_triggers_enabled(&self, enabled: bool) {
        self.recursive_triggers_enabled
            .store(enabled, std::sync::atomic::Ordering::Release);
    }

    pub fn recursive_triggers_enabled(&self) -> bool {
        self.recursive_triggers_enabled
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache.read().len() * self.page_size() as usize
    }

    pub fn cache_size(&self) -> usize {
        self.cache.read().len()
    }

    pub fn cache_capacity(&self) -> usize {
        self.cache_capacity
    }

    // ----- File I/O helpers: positioned I/O so multiple threads can
    //       read/write without serializing on the file offset. Both route
    //       through the `Store` enum — the OS-specific positioned-I/O APIs
    //       (pread/pwrite vs seek_read/seek_write) live in `Store`, and the
    //       memory store serves reads/writes from its byte image. -----

    fn read_file_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        Ok(self.store.read_at(offset, buf)?)
    }

    fn write_file_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.store.write_all_at(offset, buf)?;
        Ok(())
    }

    /// Read one OVERFLOW page without the page cache: returns (next page
    /// id, valid data byte count) and appends the page's data region to
    /// `out`. Overflow chains are walked sequentially and almost never
    /// revisited, so routing them through the LRU cache only thrashes it —
    /// a 1000 x 64KB blob scan pushes ~16k pages through a 2048-slot
    /// cache, evicting the actual tree pages. WAL-committed pages and
    /// codec-encoded pages take the same paths as `get_page`.
    ///
    /// `cap` is the page's overflow capacity (page_size - 16); the caller
    /// passes it so this stays a pure reader.
    pub(crate) fn read_overflow_page_append(
        &self,
        id: PageId,
        out: &mut Vec<u8>,
    ) -> Result<(PageId, usize)> {
        // 1. Cache hit: the cached page is authoritative (lazy write-back
        // keeps committed-but-unflushed pages only in the cache).
        {
            let cache = self.cache.read();
            if let Some(page_ref) = cache.get(id) {
                let p = page_ref.lock();
                if p.page_type()? != crate::storage::page::PageType::Overflow {
                    return Err(Error::corruption(format!(
                        "overflow chain hit non-overflow page {id}"
                    )));
                }
                let next = p.overflow_next();
                let data = p.overflow_data();
                let take = data.len();
                out.extend_from_slice(data);
                return Ok((next, take));
            }
        }
        // 2. Cache miss: raw sequential read — no Page allocation, no
        // cache insert, no LRU/eviction churn for read-once pages. The
        // store holds the current committed version here (evictions write
        // dirty pages out; everything else flushed at commit). The page is
        // read DIRECTLY into `out`'s tail (one copy instead of two).
        let psz = self.page_size() as usize;
        let served_from_wal = {
            let wal_guard = self.wal.read();
            match wal_guard.as_ref() {
                Some(state) => state.map.contains_key(&id),
                None => false,
            }
        };
        if served_from_wal {
            // WAL frame: the header + data layout differs from the main
            // file page — take the buffered path.
            let mut raw = vec![0u8; psz];
            let offset = {
                let wal_guard = self.wal.read();
                wal_guard
                    .as_ref()
                    .and_then(|s| s.map.get(&id).copied())
                    .unwrap_or(0)
            };
            let st = self.wal.read();
            st.as_ref().unwrap().wal.read_frame_at(offset, &mut raw)?;
            drop(st);
            if raw[0] != crate::storage::page::PageType::Overflow as u8 {
                return Err(Error::corruption(format!(
                    "overflow chain hit non-overflow page {id}"
                )));
            }
            let next = u32::from_be_bytes(raw[12..16].try_into().unwrap());
            let take = psz - 16;
            out.extend_from_slice(&raw[16..]);
            return Ok((next, take));
        }
        let offset = id as u64 * psz as u64;
        let codec_active = self.codec.read().is_active();
        if !codec_active {
            // Direct read into out's tail: [type..16) header region in a
            // small stack buffer, data region straight into out.
            let mut head = [0u8; 16];
            let n = self.read_file_at(offset, &mut head)?;
            if n != 16 {
                return Err(Error::corruption(format!(
                    "short read on overflow page {id}: {n} of 16 header bytes"
                )));
            }
            if head[0] != crate::storage::page::PageType::Overflow as u8 {
                return Err(Error::corruption(format!(
                    "overflow chain hit non-overflow page {id}"
                )));
            }
            let next = u32::from_be_bytes(head[12..16].try_into().unwrap());
            let data_len = psz - 16;
            let base = out.len();
            out.resize(base + data_len, 0);
            let n = self.read_file_at(offset + 16, &mut out[base..])?;
            if n != data_len {
                return Err(Error::corruption(format!(
                    "short read on overflow page {id}: {n} of {data_len} bytes"
                )));
            }
            Ok((next, data_len))
        } else {
            let mut raw = vec![0u8; psz];
            let n = self.read_file_at(offset, &mut raw)?;
            if n != psz {
                return Err(Error::corruption(format!(
                    "short read on overflow page {id}: {n} of {psz} bytes"
                )));
            }
            let decoded = {
                let cs = self.codec.read();
                cs.decode_page(false, &raw, psz)?
            };
            if decoded[0] != crate::storage::page::PageType::Overflow as u8 {
                return Err(Error::corruption(format!(
                    "overflow chain hit non-overflow page {id}"
                )));
            }
            let next = u32::from_be_bytes(decoded[12..16].try_into().unwrap());
            let take = decoded.len() - 16;
            out.extend_from_slice(&decoded[16..]);
            Ok((next, take))
        }
    }

    /// Gather `payload[start..end]` from an overflow cell's chain into
    /// `out` (appended), walking the chain under ONE cache read-lock for
    /// the whole resident run: a 64 KB blob is 16 pages, and the generic
    /// `get_page` per-page cost (RwLock + Arc clone + savepoint-depth
    /// atomic) dominated the blob-scan gather. Chain pages hold
    /// `payload[local_len..total]` sequentially; `page_pos` tracks the
    /// current page's start offset within the payload.
    ///
    /// Cold (non-resident) pages — file-backed DBs with evicted chains,
    /// WAL-committed frames, codec pages — fall back to
    /// `read_overflow_page_append` one page at a time, re-acquiring the
    /// cache guard for the next resident run.
    pub(crate) fn gather_overflow_chain_range(
        &self,
        first: PageId,
        local_len: usize,
        start: usize,
        end: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if start >= end || first == 0 {
            return Ok(());
        }
        let cap = self.page_size() as usize - 16;
        let max_pages = self.n_pages() as usize + 4;
        let mut steps = 0usize;
        let mut cur = first;
        let mut page_pos = local_len;
        'outer: loop {
            // ONE read-guard for a maximal run of resident pages (lock
            // order cache -> page matches get_page/evict, so holding it
            // across page locks is safe).
            let cache = self.cache.read();
            loop {
                steps += 1;
                if steps > max_pages {
                    return Err(Error::corruption(format!(
                        "overflow chain cycle starting at page {first}"
                    )));
                }
                if page_pos >= end {
                    return Ok(());
                }
                let page_ref = match cache.get(cur) {
                    Some(r) => r.clone(),
                    None => {
                        // Cold page: release the guard, use the generic
                        // reader, then re-acquire for the next run.
                        drop(cache);
                        let mut sink = Vec::new();
                        let (next, take) = self.read_overflow_page_append(cur, &mut sink)?;
                        let p_end = page_pos + take;
                        if p_end > start && page_pos < end {
                            let from = (start.max(page_pos) - page_pos).min(sink.len());
                            let to = (end.min(p_end) - page_pos).min(sink.len());
                            if to > from {
                                out.extend_from_slice(&sink[from..to]);
                            }
                        }
                        if next == 0 {
                            return Ok(());
                        }
                        page_pos += take;
                        cur = next;
                        continue 'outer;
                    }
                };
                let (next, take) = {
                    let p = page_ref.lock();
                    if p.page_type()? != crate::storage::page::PageType::Overflow {
                        return Err(Error::corruption(format!(
                            "overflow chain hit non-overflow page {cur}"
                        )));
                    }
                    let next = p.overflow_next();
                    let data = p.overflow_data();
                    let take = data.len().min(cap);
                    let p_end = page_pos + take;
                    if p_end > start && page_pos < end {
                        let from = start.max(page_pos) - page_pos;
                        let to = end.min(p_end) - page_pos;
                        if to > from {
                            out.extend_from_slice(&data[from..to]);
                        }
                    }
                    (next, take)
                };
                if next == 0 {
                    return Ok(());
                }
                page_pos += take;
                cur = next;
            }
        }
    }
}

/// Codec marker area: bytes 72..100 of the file header, laid out as
/// `b"RQLCODEC:"` + name + NUL padding. Kept plain by the codec layer
/// (see `CodecState`).
const CODEC_MARKER_OFFSET: usize = 72;
const CODEC_MARKER_LEN: usize = 28;

fn write_codec_marker(header: &mut [u8], name: &str) {
    let area = &mut header[CODEC_MARKER_OFFSET..CODEC_MARKER_OFFSET + CODEC_MARKER_LEN];
    area.fill(0);
    let prefix = b"RQLCODEC:";
    area[..prefix.len()].copy_from_slice(prefix);
    let max_name = CODEC_MARKER_LEN - prefix.len() - 1;
    let n = name.len().min(max_name);
    area[prefix.len()..prefix.len() + n].copy_from_slice(&name.as_bytes()[..n]);
}

fn clear_codec_marker(header: &mut [u8]) {
    header[CODEC_MARKER_OFFSET..CODEC_MARKER_OFFSET + CODEC_MARKER_LEN].fill(0);
}

/// Read the codec name from a 100-byte header image (None when absent).
fn codec_marker_name(header: &[u8; 100]) -> Option<String> {
    let area = &header[CODEC_MARKER_OFFSET..CODEC_MARKER_OFFSET + CODEC_MARKER_LEN];
    let prefix = b"RQLCODEC:";
    if area.len() < prefix.len() || &area[..prefix.len()] != prefix {
        return None;
    }
    let name_bytes = &area[prefix.len()..];
    let end = name_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_bytes.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&name_bytes[..end]).into_owned())
}

/// Trait helper to convert `AsRef<Path>` to `PathBuf` without naming the
/// `path` parameter `path` (which would shadow the field `path`).
trait PathExt {
    fn ref_to_path(&self) -> PathBuf;
}
impl<P: AsRef<Path>> PathExt for P {
    fn ref_to_path(&self) -> PathBuf {
        self.as_ref().to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn open_creates_new_db() {
        let tmp = NamedTempFile::new().unwrap();
        let pager = Pager::open(tmp.path(), 64).unwrap();
        assert_eq!(pager.n_pages(), 1);
        assert_eq!(pager.page_size(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn allocate_and_flush() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let pager = Pager::open(tmp.path(), 64).unwrap();
            let id = pager.allocate_page().unwrap();
            assert_eq!(id, 1);
            let page = pager.get_page(id).unwrap();
            {
                let mut p = page.lock();
                p.data[0] = 42;
                p.mark_dirty();
            }
            pager.note_write();
            pager.flush().unwrap();
        }
        // Reopen and verify
        let pager = Pager::open(tmp.path(), 64).unwrap();
        assert_eq!(pager.n_pages(), 2);
        let page = pager.get_page(1).unwrap();
        assert_eq!(page.lock().data[0], 42);
    }

    #[test]
    fn freelist_recycles_pages() {
        let tmp = NamedTempFile::new().unwrap();
        let pager = Pager::open(tmp.path(), 64).unwrap();
        let p1 = pager.allocate_page().unwrap();
        let p2 = pager.allocate_page().unwrap();
        let p3 = pager.allocate_page().unwrap();
        assert_eq!((p1, p2, p3), (1, 2, 3));
        pager.free_page(p2).unwrap();
        let reused = pager.allocate_page().unwrap();
        assert_eq!(reused, p2);
    }

    /// Concurrent reads should not deadlock and should see consistent data.
    #[test]
    fn concurrent_get_page_is_safe() {
        let tmp = NamedTempFile::new().unwrap();
        let pager = Arc::new(Pager::open(tmp.path(), 128).unwrap());
        // Allocate some pages
        let ids: Vec<u32> = (0..8).map(|_| pager.allocate_page().unwrap()).collect();
        for &id in &ids {
            let page = pager.get_page(id).unwrap();
            let mut p = page.lock();
            p.data[0] = (id % 256) as u8;
            p.dirty = true;
        }
        pager.note_write();
        pager.flush().unwrap();

        let pager = Arc::new(Pager::open(tmp.path(), 128).unwrap());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pager = Arc::clone(&pager);
            let ids = ids.clone();
            handles.push(std::thread::spawn(move || {
                for &id in &ids {
                    let page = pager.get_page(id).unwrap();
                    let p = page.lock();
                    assert_eq!(p.data[0], (id % 256) as u8);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
