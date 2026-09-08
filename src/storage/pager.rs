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

/// Epoch-space separator for committed-view hint stamps (see
/// `Pager::write_epoch`): committed-scope epochs carry this bit, live
/// epochs never do, so a hint built by a live-mode read can never
/// validate inside a committed scope and vice versa.
pub(crate) const COMMITTED_EPOCH_MARK: u64 = 1u64 << 47;

/// Pages with id below this bound live in the direct-indexed Vec (any
/// file up to 4 GB at 4 KB pages); higher ids spill to a HashMap. This
/// bounds the Vec at 8 MB regardless of database size while keeping the
/// no-hash fast path for every page of a normal database.
const PAGE_VEC_DIRECT_LIMIT: usize = 1 << 20;

/// PRAGMA parallel_scan default: tables at or above this estimated row
/// count split their aggregate scans across worker threads
/// (`executor::parallel`). 128k rows sits above every existing
/// test/bench table (so those paths stay serial, bit-identical) and
/// below the 1M-row analytical range where the split pays.
pub(crate) const DEFAULT_PARALLEL_SCAN_MIN_ROWS: i64 = 131_072;

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
    /// MID-TRANSACTION SPILL index: page-id → frame offset of a dirty
    /// page's newest version, written as an UNCOMMITTED frame so the
    /// cache can drop it under pressure (SQLite's pager does exactly
    /// this: dirty pages spill to the WAL, not the cache, which is what
    /// keeps big write transactions' RSS bounded). The frames become
    /// committed by the next COMMIT's trailing commit frame — no rewrite
    /// needed; `map` absorbs these offsets at commit time. ROLLBACK
    /// simply discards the map (recovery ignores trailing uncommitted
    /// frames). `get_page` misses consult this BEFORE the committed map
    /// (a spilled page's newest version is the spill frame, not the
    /// committed one).
    pub spilled: std::collections::HashMap<PageId, u64, PageIdHashBuild>,
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

/// Path of the DELETE-mode mid-transaction page-spill sidecar.
fn spill_path_for<P: AsRef<Path>>(db_path: P) -> PathBuf {
    let mut p = db_path.as_ref().as_os_str().to_os_string();
    p.push("-spill");
    PathBuf::from(p)
}

/// DELETE-mode mid-transaction page spill: BOUNDED-RSS eviction for big
/// write transactions (SQLite's own journal spill, shaped as a sidecar
/// instead of a rollback journal). In DELETE mode a dirty page could
/// never leave the cache mid-txn — the main file must keep the
/// pre-BEGIN bytes until COMMIT — so a 1M-row insert txn held the whole
/// database in memory (~47 MiB where SQLite peaks at ~10). Under cache
/// pressure, dirty pages now append as `[u32 id][page bytes]` records
/// to a `-spill` sidecar:
/// - `get_page` misses re-read the newest (uncommitted) version from
///   the spill map and re-cache it DIRTY (back under normal tracking),
/// - COMMIT drains the records into the main file (with the other
///   dirty pages, before the header — the crash ordering contract),
/// - ROLLBACK just drops the file (the main file was never touched),
/// - a crash mid-txn leaves a stale sidecar that the next open
///   deletes (uncommitted spill is garbage).
///
/// The sidecar is disk, never RSS: the cache stays at its capacity
/// bound and peak memory stops tracking database size.
struct DeleteSpill {
    file: std::fs::File,
    /// page id -> byte offset of its NEWEST `[id][bytes]` record.
    pages: std::collections::HashMap<PageId, u64, PageIdHashBuild>,
}

impl DeleteSpill {
    /// Append a record for `id`; returns the record offset.
    fn append(&mut self, id: PageId, data: &[u8]) -> Result<u64> {
        use std::io::{Seek, SeekFrom, Write};
        let off = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&id.to_le_bytes())?;
        self.file.write_all(data)?;
        Ok(off)
    }

    /// Read the page body of the record at `off` (the `[id][bytes]`
    /// header is at `off`, the body at `off + 4`).
    fn read_page_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        read_exact_at(&self.file, off + 4, buf)
    }
}

#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, off: u64, buf: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(Error::corruption("short read on the spill sidecar"));
        }
        done += n;
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_exact_at(file: &std::fs::File, off: u64, buf: &mut [u8]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file;
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)?;
    Ok(())
}

impl Pager {
    /// True when the backing store is the in-memory image (a `:memory:`
    /// database). In-memory payloads are this process's own encoder
    /// output — the basis for trusted TEXT decoding in the scan family.
    /// In-memory stores never evict (the cache IS the store), so the
    /// DELETE-mode spill never engages for them.
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
    /// Cache capacity in PAGES. Atomic: `PRAGMA cache_size` updates it
    /// at runtime (positive = pages, negative = KiB per SQLite), while
    /// the eviction pass reads it on every cache insert.
    cache_capacity: AtomicUsize,
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
    /// APPEND frames here, reads consult the committed-page map before the
    /// main file (WAL-served reads), and checkpoints copy pages back.
    ///
    /// The RwLock is for the page map: readers take the read lock to
    /// resolve a page → frame offset while the (single) writer appends
    /// under the write lock. `Wal` itself is writer-only; the read path
    /// goes through `Wal::read_frame_at` on a shared handle.
    wal: RwLock<Option<WalState>>,
    /// DELETE-mode mid-transaction page spill (see `DeleteSpill`):
    /// `None` until the first cache-pressure eviction of a dirty page
    /// during a write transaction; dropped (file deleted) at COMMIT
    /// drain / ROLLBACK / close.
    delete_spill: Mutex<Option<DeleteSpill>>,
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
    /// TRUNCATE FLOOR: the MINIMUM `base.n_pages` across all stacked
    /// savepoint levels (u32::MAX when none). Pages below it were alive
    /// before the transaction began and are logged in at least one undo
    /// map — `truncate_tail`'s rewind must never cross it (a ROLLBACK
    /// `get_page(id)` would fail past a rewound n_pages). Pages at or
    /// above the floor were allocated during the current transaction,
    /// so dropping them is rollback-safe. Maintained at push / pop /
    /// rollback / clear. (The CAPTURE fast path is the separate
    /// MAXIMUM-based `savepoint_capture_gate` — see its docs.)
    savepoint_min_base: std::sync::atomic::AtomicU32,
    /// Capture GATE for `capture_savepoint_undo`'s range fast path: the
    /// MAXIMUM base n_pages across all savepoint levels. A page at or
    /// above it was allocated after EVERY level's base, so no level can
    /// hold (or need) its pre-image — the capture loop's per-level
    /// `id < level.base.n_pages` check would insert into none of them.
    /// Distinct from `savepoint_min_base` (the truncate FLOOR — pages
    /// below the LOWEST base exist in at least one undo map): pages
    /// between the floor and the gate were allocated after BEGIN but
    /// BEFORE an upper savepoint, and they DO need pre-images in the
    /// upper levels (their state at the savepoint is rollback state).
    /// The old single min-based gate skipped those captures — a nested
    /// savepoint's ROLLBACK TO then left mid-tree pages mutated (root
    /// splits during cache-pressure storms exposed it as out-of-range
    /// page walks).
    savepoint_capture_gate: std::sync::atomic::AtomicU32,
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
    /// COMMITTED-VIEW READ CACHE (see `committed_view` below): pages
    /// materialized for readers that run while a write transaction is
    /// open on another thread. Never touched by the writer's live view;
    /// entries are invalidated at PRE-IMAGE CAPTURE (the fetch that
    /// falsifies the alias precondition — see capture_savepoint_undo)
    /// and at transaction boundaries (`clear_savepoints` removes the
    /// transaction's touched pages; DDL / savepoint ops / VACUUM clear
    /// everything).
    committed_pages: RwLock<std::collections::HashMap<PageId, CommittedEntry, PageIdHashBuild>>,
    /// Set when a committed-view read served BEGIN-time bytes that differ
    /// from the live cache (pre-images / their memoized copies). Retired
    /// by `CommittedViewGuard::drop` with a single write-epoch bump so
    /// the reader's advisory-cache state can never outlive its scope.
    committed_poison: std::sync::atomic::AtomicBool,
    /// Latched true the first time a page is materialized into
    /// `committed_pages`. The capture path (`capture_savepoint_undo`)
    /// and the transaction boundaries (`clear_savepoints`) use it to
    /// skip the committed-pages write lock on pagers that never served
    /// a committed-view read. Never reset: a stale-true only costs one
    /// guarded probe per captured page.
    committed_view_used: std::sync::atomic::AtomicBool,
    /// Cache of the BOTTOM savepoint's base.n_pages (the committed-view
    /// bound) — 0 = no savepoint stack (committed == live). Maintained
    /// inside the savepoints-lock critical sections so
    /// `committed_bound()` is a single Acquire load: every armed
    /// committed-view reader query used to take the savepoints Mutex,
    /// contending with the writer's per-page undo capture (a lock convoy
    /// with 7 reader threads on multicore boxes).
    committed_bound_cache: std::sync::atomic::AtomicU32,
    /// Fast-path gate for the committed-view scope check in `get_page`:
    /// the scope COUNT below, loaded as a single Acquire load — zero (the
    /// overwhelming common case — no reader scope armed) skips the
    /// thread-local scope read entirely; one atomic load in L1 replaces a
    /// TLS access per page fetch. Windows TLS access is a TEB/FLS round
    /// trip — the per-`get_page` TLS probe measurably regressed join-heavy
    /// queries on CI hardware, which motivated the gate. The count is the
    /// SOLE authority (no separate bool): a bool raised in `arm` and
    /// lowered in `drop` has an interleaving where a concurrent last
    /// drop's store(false) lands AFTER a new arm's store(true), leaving a
    /// live scope gated off. RMW operations on the counter serialize, so
    /// count > 0 holds whenever any scope exists — no ordering hazard.
    committed_scope_count: std::sync::atomic::AtomicUsize,
    /// Snapshot of `write_epoch()` at BEGIN: while a committed-view scope
    /// is armed on this pager, `write_epoch` returns THIS value so hints
    /// built against BEGIN-time pages stay valid for the whole
    /// transaction (the committed tree is immutable until COMMIT — the
    /// writer's live `note_write` bumps must not rebuild every reader
    /// descent). 0 = no transaction open (never consulted then).
    committed_view_epoch: std::sync::atomic::AtomicU64,
    /// PRAGMA user_version / application_id — persisted at the SQLite
    /// header offsets (60..64 / 68..72) inside page 0.
    user_version: std::sync::atomic::AtomicU32,
    application_id: std::sync::atomic::AtomicU32,
    /// Retired flag (VACUUM's rewrite path): the database file is being
    /// replaced out from under this pager — `Drop` must skip ALL close-time
    /// bookkeeping (checkpoint / set_len / sidecar removal) so it cannot
    /// clobber the freshly written compact image. See `retire()`.
    retired: AtomicBool,
    /// PRAGMA parallel_scan — the minimum estimated row count at which
    /// single-table aggregate scans split across worker threads (see
    /// `executor::parallel`). 0 disables intra-statement parallelism
    /// (the serial paths are then ALWAYS taken, bit-identical). Lives on
    /// the pager — like foreign_keys/recursive_triggers — because the
    /// executor's ExecContext carries a `&Pager`, the one shared handle
    /// both the engine and the statement dispatcher reach.
    parallel_scan_min_rows: std::sync::atomic::AtomicI64,
    /// `PRAGMA temp_store` (SQLite semantics: 0/DEFAULT and 1/FILE let
    /// ephemeral structures spill to disk; 2/MEMORY keeps them in RAM).
    /// The aggregate grouper's temp-store freeze honors it — see
    /// `executor::HashGrouper::armed_grouper`.
    temp_store: std::sync::atomic::AtomicI64,
    /// Sequential-access tracker for read-ahead: the last page id
    /// touched (any path) and the length of the current +1 run.
    prefetch_last: std::sync::atomic::AtomicU32,
    prefetch_run: std::sync::atomic::AtomicU8,
}

// ---------------------------------------------------------------------------
// COMMITTED-VIEW READS (WAL-grade read concurrency)
// ---------------------------------------------------------------------------
//
// While a write transaction is open, the writer's uncommitted state lives
// ONLY in the live page cache (the executor never flushes mid-transaction;
// WAL spill frames are recorded in a separate `spilled` map, never in the
// committed `map`). The last COMMITTED state of every page is therefore
// always reconstructable from:
//   1. the `__begin__` savepoint's undo pre-images (pages the writer
//      fetched — and possibly mutated — since BEGIN), and
//   2. the WAL committed-page map + main file (pages the writer never
//      fetched — their live-cache copies are byte-identical to the
//      committed state).
// A reader on a thread that does NOT own the transaction arms a
// COMMITTED-VIEW SCOPE (thread-local) around its statement; `get_page`
// then serves the BEGIN-time state instead of the live one. Readers never
// block on an open write transaction — SQLite-WAL reader semantics, but
// with the version store in memory (pre-images) instead of WAL frames.
//
// Because the engine serializes statements behind a RwLock (readers hold
// the read guard while the writer mutates only under the write guard),
// no page bytes can change WHILE a committed-view reader executes — the
// scope is quiescent by construction. The scope is thread-local, so the
// WRITER thread (and the txn owner reading its own writes) is unaffected.

thread_local! {
    /// Armed committed-view scope: `(pager instance id, committed n_pages
    /// bound)`. `None` on the fast path — one TLS load per `get_page`.
    static COMMITTED_VIEW: std::cell::Cell<Option<(u64, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// A committed-view materialization-cache entry: the shared `PageRef`
/// plus whether it ALIASES the live cache (path 3: the writer never
/// fetched this page this transaction, so the live bytes ARE the
/// committed bytes) or is a pre-image COPY (path 1: BEGIN-time bytes,
/// always different-or-divergent from live).
///
/// The alias bit is decided at INSERT time and never changes: an alias
/// dies at the writer's first fetch (capture-side invalidation removes
/// the entry and bumps the epoch) — it can never quietly become a
/// copy. (The old code re-derived alias-ness on every hit by
/// cross-probing the LIVE cache with a second read lock per page
/// fetch — under N concurrent readers that lock word ping-ponged and
/// dominated the committed-view cost. Statically latched at insert,
/// the bytes-equivalence invariant carries the same correctness: a
/// live-page eviction + re-fetch never mutates the entry's PageRef.)
#[derive(Clone)]
struct CommittedEntry {
    pr: PageRef,
    alias: bool,
}

/// Per-thread memo of the shared committed-view map. The 1W+7R reader
/// shape re-fetches the same ~40-60 pages every query (root, interior,
/// and the working-set leaves): the shared map's read lock is an RMW
/// on one cache line, so N readers ping-pong it per page fetch. The
/// memo serves repeat fetches from thread-local storage (no lock, no
/// shared-line traffic) and validates with ONE read-only epoch load:
/// the committed-view epoch moves on every invalidating event, so a
/// matching token proves every memoized entry is still the shared
/// map's current content.
///
/// Capped at [`COMMITTED_MEMO_CAP`] pages: scan-shaped readers beyond
/// the cap fall back to the shared map (same as pre-memo behavior);
/// OLTP working sets (the contended shape) fit comfortably.
struct CommittedMemo {
    instance: u64,
    token: u64,
    pages: std::collections::HashMap<PageId, CommittedEntry, PageIdHashBuild>,
}

thread_local! {
    static COMMITTED_MEMO: std::cell::RefCell<Option<CommittedMemo>> =
        const { std::cell::RefCell::new(None) };
}

/// Hard cap on the per-thread committed-view memo: 256 pages x 4 KiB
/// = 1 MiB per reader thread — the interior-node working set of even
/// large tables is tens of pages; hot leaf ranges fit under a few
/// hundred.
const COMMITTED_MEMO_CAP: usize = 256;

/// Hard cap on the committed-view materialization cache (pages). See
/// `cache_committed_page`: bounds the extra footprint of concurrent
/// committed-view readers to 4 MiB at the default 4 KiB page size.
const COMMITTED_PAGES_CAP: usize = 1024;

/// RAII guard returned by `Pager::arm_committed_view`: arms the
/// thread-local scope for THIS pager and restores the previous value on
/// drop (re-entrancy-safe — a nested arm restores the outer scope).
///
/// Drop also retires the epoch-poison signal: while the scope was armed,
/// `get_page` served BEGIN-time bytes that can differ from the live
/// cache. Every advisory read cache in the engine (TLS leaf hints,
/// fast-path handle pins, join builds) is validated against the pager's
/// write epoch — state a reader recorded from BEGIN-time bytes at epoch
/// E would validate as "live at E" after the scope ends and serve stale
/// first/last keys or stale pinned pages to LIVE readers. When the scope
/// actually served writer-touched pages (pre-images), drop bumps the
/// write epoch ONCE: every cache the reader populated is re-derived on
/// next use. Readers that only touched never-fetched pages
/// (byte-identical to live) leave the epoch — and every warm cache in
/// the process — untouched.
#[must_use]
pub struct CommittedViewGuard<'a> {
    prev: Option<(u64, u32)>,
    /// Per-thread advisory-cache generation at ARM time: the drop only
    /// bumps the write epoch when this scope actually created durable
    /// thread-local advisory state (hints). A read that merely probed
    /// pages retires nothing — and skipping the bump keeps concurrent
    /// committed-view readers from invalidating the process's hint state
    /// on every query.
    advisory_gen_at_arm: u64,
    pager: &'a Pager,
}

impl Drop for CommittedViewGuard<'_> {
    fn drop(&mut self) {
        COMMITTED_VIEW.with(|c| c.set(self.prev.take()));
        let poisoned = self.pager.committed_poison.swap(false, Ordering::AcqRel);
        if poisoned && crate::storage::btree::advisory_state_gen() != self.advisory_gen_at_arm {
            // This scope served BEGIN-time pages AND built thread-local
            // advisory state under those bytes — retire it (the state
            // would otherwise validate against a LIVE epoch after the
            // scope ends).
            self.pager.write_version.fetch_add(1, Ordering::Release);
        }
        // Lower the gate: the count IS the gate, and the decrement is a
        // serialized RMW — once it lands, every thread's gate load sees
        // the new count atomically. Checking only this thread's TLS
        // ("did my drop leave this pager armed on THIS thread?") is
        // wrong with concurrent readers: reader B's drop would clear the
        // gate while reader C's scope is still live, and C's get_page
        // would serve live pages inside its committed scope (isolation
        // violation).
        self.pager.scope_count_dec();
    }
}

/// Read the armed scope without touching TLS state.
#[inline]
fn committed_scope() -> Option<(u64, u32)> {
    COMMITTED_VIEW.with(|c| c.get())
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
        // base — pages only grow), so the min cannot shrink and the max
        // IS the new level's base; the stores keep both invariants
        // explicit.
        let m = sp.iter().map(|s| s.base.n_pages).min().unwrap_or(u32::MAX);
        self.savepoint_min_base.store(m, Ordering::Relaxed);
        let g = sp.iter().map(|s| s.base.n_pages).max().unwrap_or(u32::MAX);
        self.savepoint_capture_gate.store(g, Ordering::Relaxed);
        self.savepoint_depth.store(sp.len(), Ordering::Release);
        // Committed-view bound cache: the FIRST level is the txn base.
        if let Some(base) = sp.first().map(|s| s.base.n_pages) {
            self.committed_bound_cache.store(base, Ordering::Release);
        }
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
            // The bottom level never changes here (idx >= 0 re-pushed at
            // idx), so the bound cache stays valid — except the idx == 0
            // re-push, which keeps the same base. No update needed.
            (level.pages, level.base)
        };
        // The stack was re-shaped above (levels above this savepoint
        // truncated away, the level itself re-pushed with its base):
        // recompute both derived bounds — a stale GATE (from a dropped,
        // higher-base level) would skip needed captures; a stale FLOOR
        // would mis-gate tail truncation.
        {
            let sp = self.savepoints.lock();
            let m = sp.iter().map(|s| s.base.n_pages).min().unwrap_or(u32::MAX);
            self.savepoint_min_base.store(m, Ordering::Relaxed);
            let g = sp.iter().map(|s| s.base.n_pages).max().unwrap_or(u32::MAX);
            self.savepoint_capture_gate.store(g, Ordering::Relaxed);
        }
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
            // SPILL discipline: pages allocated after the savepoint's base
            // are dropped — their uncommitted spill frames are dead work.
            // Pages BELOW the base keep their entries: a page spilled
            // before this savepoint (never re-fetched since) has its
            // current state IN that frame; pages re-fetched during Phase
            // 2's restore already left the spilled map at get_page time.
            if let Some(state) = self.wal.write().as_mut() {
                state.spilled.retain(|&id, _| id < base.n_pages);
            }
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
        let g = sp.iter().map(|s| s.base.n_pages).max().unwrap_or(u32::MAX);
        self.savepoint_capture_gate.store(g, Ordering::Relaxed);
        self.savepoint_depth.store(remaining, Ordering::Release);
        // Releasing the bottom level re-bases the committed view (the
        // savepoint-txn COMMIT path — releasing the outermost savepoint
        // commits, and clear_savepoints resets the cache; an inner
        // release that pops to a new bottom needs the refresh).
        match sp.first().map(|s| s.base.n_pages) {
            Some(base) => self.committed_bound_cache.store(base, Ordering::Release),
            None => self.committed_bound_cache.store(0, Ordering::Release),
        }
        Some(remaining)
    }

    /// Discard all savepoints (COMMIT / plain ROLLBACK).
    pub fn clear_savepoints(&self) {
        let mut sp = self.savepoints.lock();
        // Committed-view lifecycle: instead of clearing the WHOLE
        // materialization map at every transaction boundary (the old
        // behavior — readers re-copied every scanned page after every
        // BEGIN/COMMIT: ~16 x 8 KiB copies per query under 1W+7R
        // contention), invalidate ONLY the pages this transaction touched.
        // The __begin__ level's pre-image key set is a superset of every
        // page the transaction fetched (all mutations go through
        // get_page -> capture), so the remaining entries — pages the
        // writer never touched — still hold the correct BEGIN-time bytes
        // for the NEXT transaction's committed view too.
        if self.committed_view_used.load(Ordering::Acquire) {
            if let Some(begin_level) = sp.first() {
                let mut cp = self.committed_pages.write();
                for id in begin_level.pages.keys() {
                    cp.remove(id);
                }
            }
        }
        sp.clear();
        self.savepoint_min_base.store(u32::MAX, Ordering::Relaxed);
        self.savepoint_capture_gate
            .store(u32::MAX, Ordering::Relaxed);
        self.savepoint_depth.store(0, Ordering::Release);
        self.committed_bound_cache.store(0, Ordering::Release);
        // The transaction ended: unfreeze the committed-view hint epoch.
        self.committed_view_epoch.store(0, Ordering::Release);
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
        // The savepoint stack is going away: any DELETE-mode spill records
        // are this transaction's uncommitted mutations being discarded.
        self.drop_delete_spill();
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
        // loads — the dominant per-row overhead vs SQLite). The gate is
        // the MAXIMUM base (pages below it existed within at least the
        // newest level's lifetime and may need pre-images there); the
        // pre-fix MIN-based gate wrongly skipped pages allocated between
        // BEGIN and an upper savepoint — ROLLBACK TO that savepoint then
        // left their mutations in place (mid-tree pages pointing at
        // rewound ids).
        if (id as u64) >= self.savepoint_capture_gate.load(Ordering::Relaxed) as u64 {
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
        // COMMITTED-VIEW ENTRY INVALIDATION — at the exact moment the
        // page's committed-bytes source flips from LIVE to PRE-IMAGE.
        // Until now a reader's alias entry (path 3: live bytes ==
        // committed bytes for a page the writer had never fetched) was
        // valid; from this fetch on, the live bytes may diverge (in-place
        // mutation, payload patch without an epoch bump, freelist
        // rewrite) while the committed bytes are the pre-image recorded
        // below. Kill the entry HERE — not at mutation time: note_dirty's
        // same-page early-return (last_noted_dirty == id) skips
        // mutation-side invalidation whenever a reader memoized an alias
        // between two consecutive notes of the same page, and
        // note_write_in_place deliberately bumps no epoch an entry could
        // validate against. Fetch-side invalidation has neither hole:
        // the alias's precondition ("writer never fetched this page this
        // transaction") is exactly what this capture falsifies. Runs
        // once per page per transaction (the re-fetch fast exit above
        // returns first), under the engine write lock that excludes
        // every reader — and latched off entirely for pagers that never
        // served a committed-view read.
        if self.committed_view_used.load(Ordering::Acquire) {
            let mut cp = self.committed_pages.write();
            if cp.remove(&id).is_some() {
                // BEGIN-time divergence: this page's committed alias (or
                // copy) just died — the committed STATE changed. Bump the
                // committed-state epoch so hints built earlier in the
                // transaction (which may hold this page's LIVE PageRef —
                // the writer is about to mutate it in place) are retired:
                // the next descent rebuilds through get_page_committed
                // and serves the pre-image instead.
                self.committed_view_epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
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
        //
        // A RETIRED pager (its database file was replaced out from under
        // it — VACUUM's rewrite path) skips all of it: the close-time
        // bookkeeping would write its own pre-replacement view back over
        // the new image (an empty-map checkpoint still re-asserts the
        // old page count via set_len) and delete sidecars now owned by
        // the fresh pager.
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        if self.wal.read().is_some() {
            let _ = self.checkpoint_wal();
            let _ = std::fs::remove_file(crate::storage::wal::wal_path_for(&self.path));
        }
        // DELETE-mode spill sidecar: uncommitted cache-pressure records.
        // A clean close either already drained them (COMMIT ran flush) or
        // the transaction was abandoned — both leave garbage. Best-effort.
        if self.delete_spill.lock().is_some() {
            let _ = std::fs::remove_file(spill_path_for(&self.path));
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

    /// The memory store's backing image (post-flush): the full byte
    /// sequence a file store would hold. VACUUM's page-level compact copy
    /// materializes its result this way.
    pub(crate) fn memory_store_image(&self) -> Result<Vec<u8>> {
        match &self.store {
            Store::Memory(m) => Ok(m.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            Store::File(_) => Err(Error::InvalidArgument(
                "memory_store_image on a file-backed pager".into(),
            )),
        }
    }

    /// The CURRENT image of a `:memory:` database — cache included. In
    /// lazy write-back mode the cache IS the store (`flush` is a no-op,
    /// so the backing `Vec` can be arbitrarily stale); this snapshot
    /// merges every cached page on top of the store bytes, resizes to
    /// the live page count, and rewrites the header (n_pages, freelist,
    /// schema cookie — exactly what a real flush would write last).
    ///
    /// This is `Database::image`'s `:memory:` path and the C ABI's
    /// `sqlite3_serialize` contract: the buffer a fresh
    /// `open_memory_from_image` (or `open_in_memory_with_image`)
    /// reopens faithfully.
    pub(crate) fn memory_image_current(&self) -> Result<Vec<u8>> {
        if !self.store.is_memory() {
            return Err(Error::InvalidArgument(
                "memory_image_current on a file-backed pager".into(),
            ));
        }
        if self.wal.read().is_some() {
            return Err(Error::InvalidArgument(
                "serialize of a WAL-armed memory pager is not supported".into(),
            ));
        }
        if self.codec.read().is_active() {
            return Err(Error::InvalidArgument(
                "serialize of a page-codec database is not supported".into(),
            ));
        }
        let psz = self.page_size() as usize;
        let n_pages_val = self.n_pages.load(Ordering::Acquire);
        let freelist_head_val = self.freelist_head.load(Ordering::Acquire);
        let freelist_count_val = self.freelist_count.load(Ordering::Acquire);
        let schema_cookie_val = self.schema_cookie.load(Ordering::Acquire);

        let mut out = vec![0u8; n_pages_val as usize * psz];
        // Base: whatever the store already holds (evicted pages).
        {
            let base_len = {
                let m = match &self.store {
                    Store::Memory(m) => m,
                    Store::File(_) => unreachable!("checked above"),
                };
                let guard = m.lock().unwrap_or_else(|e| e.into_inner());
                let take = guard.len().min(out.len());
                out[..take].copy_from_slice(&guard[..take]);
                guard.len()
            };
            let _ = base_len;
        }
        // Overlay: every cached page (the newest state; the cache is
        // authoritative in lazy mode).
        {
            let cache = self.cache.read();
            for (id, page) in cache.iter() {
                let idx = id as usize;
                if idx >= n_pages_val as usize {
                    continue;
                }
                let borrowed = page.lock();
                let start = idx * psz;
                out[start..start + psz].copy_from_slice(&borrowed.data);
            }
        }
        // Header: the same fields a real flush writes last.
        if out.len() >= psz {
            FileHeader::write(&mut out[..psz], psz as u32, n_pages_val, schema_cookie_val);
            out[20..24].copy_from_slice(&freelist_head_val.to_le_bytes());
            out[24..28].copy_from_slice(&freelist_count_val.to_le_bytes());
        }
        Ok(out)
    }

    /// Open a pure in-memory pager seeded from a COMPLETE database image
    /// (page 0 header + every live page — exactly the byte sequence a
    /// file store would hold on disk). `from_store` reads the header and
    /// re-arms page size / page count / freelist / schema cookie from
    /// page 0, so the seeded pager is a faithful reopening of the image.
    ///
    /// VACUUM uses this to re-seed a `:memory:` database with its
    /// compacted image (the compact build runs in a temp FILE database
    /// so every durability path applies, then the bytes come home).
    pub fn open_memory_from_image(image: Vec<u8>, cache_capacity: usize) -> Result<Self> {
        if image.is_empty() {
            return Err(Error::corruption(
                "cannot seed a memory pager from an empty image",
            ));
        }
        let store = Store::Memory(std::sync::Mutex::new(image));
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
            cache_capacity: AtomicUsize::new(cache_capacity),
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
            delete_spill: Mutex::new(None),
            synchronous: std::sync::atomic::AtomicU8::new(2),
            cache_misses: std::sync::atomic::AtomicU64::new(0),
            instance_id: PAGER_INSTANCE_COUNTER
                .fetch_add(1, Ordering::Relaxed)
                .checked_add(1)
                .unwrap_or(0),
            savepoints: Mutex::new(Vec::new()),
            savepoint_min_base: std::sync::atomic::AtomicU32::new(u32::MAX),
            savepoint_capture_gate: std::sync::atomic::AtomicU32::new(u32::MAX),
            savepoint_depth: std::sync::atomic::AtomicUsize::new(0),
            codec: RwLock::new(crate::plugin::codec::CodecState::default()),
            required_codec: Mutex::new(None),
            join_cache: Mutex::new(std::collections::HashMap::new()),
            committed_pages: RwLock::new(std::collections::HashMap::default()),
            committed_poison: std::sync::atomic::AtomicBool::new(false),
            committed_view_used: std::sync::atomic::AtomicBool::new(false),
            committed_bound_cache: std::sync::atomic::AtomicU32::new(0),
            committed_scope_count: std::sync::atomic::AtomicUsize::new(0),
            committed_view_epoch: std::sync::atomic::AtomicU64::new(0),
            user_version: std::sync::atomic::AtomicU32::new(0),
            application_id: std::sync::atomic::AtomicU32::new(0),
            retired: AtomicBool::new(false),
            parallel_scan_min_rows: std::sync::atomic::AtomicI64::new(
                crate::storage::pager::DEFAULT_PARALLEL_SCAN_MIN_ROWS,
            ),
            temp_store: std::sync::atomic::AtomicI64::new(0),
            prefetch_last: std::sync::atomic::AtomicU32::new(u32::MAX),
            prefetch_run: std::sync::atomic::AtomicU8::new(0),
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
                // Crash recovery: a leftover -spill sidecar is UNCOMMITTED
                // work from a transaction that never committed (the main
                // file was never touched mid-txn). Garbage — delete it.
                let spill_file = spill_path_for(&pager.path);
                if spill_file.exists() {
                    let _ = std::fs::remove_file(&spill_file);
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
        self.user_version
            .store(FileHeader::user_version(&header), Ordering::Release);
        self.application_id
            .store(FileHeader::application_id(&header), Ordering::Release);

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

    /// PRAGMA user_version — the 32-bit value at header bytes 60..64.
    /// Persisted by patching page 0 IN PLACE (the cached page owns the
    /// header; the next flush writes it out with every other dirty page).
    pub fn set_user_version(&self, v: u32) -> Result<()> {
        self.user_version.store(v, Ordering::Release);
        self.patch_header_u32(60, v)
    }

    pub fn user_version(&self) -> u32 {
        self.user_version.load(Ordering::Acquire)
    }

    /// PRAGMA application_id — header bytes 68..72.
    pub fn set_application_id(&self, v: u32) -> Result<()> {
        self.application_id.store(v, Ordering::Release);
        self.patch_header_u32(68, v)
    }

    pub fn application_id(&self) -> u32 {
        self.application_id.load(Ordering::Acquire)
    }

    /// Patch a 4-byte little-endian field inside page 0's file header,
    /// marking the page dirty so the next flush persists it. The backing
    /// image is patched with a DIRECT 4-byte write as well (a full flush
    /// could drag other mid-transaction dirty pages into the committed
    /// image — the byte patch cannot).
    fn patch_header_u32(&self, offset: usize, v: u32) -> Result<()> {
        let page = self.get_page(0)?;
        {
            let mut p = page.lock();
            let bytes = v.to_le_bytes();
            p.data[offset..offset + 4].copy_from_slice(&bytes);
            p.dirty = true;
        }
        self.note_dirty(0);
        let bytes = v.to_le_bytes();
        self.write_file_at(offset as u64, &bytes)?;
        Ok(())
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

    /// This pager's stable instance id (process-wide counter).
    pub(crate) fn instance_id(&self) -> u64 {
        self.instance_id
    }

    /// The active page codec, if any (for VACUUM's compact-target parity).
    pub(crate) fn active_page_codec(&self) -> Option<std::sync::Arc<dyn crate::plugin::PageCodec>> {
        self.codec.read().active.clone()
    }

    /// Cache-invalidation epoch: packs (instance_id, write_version) so
    /// advisory caches can detect BOTH "content changed" and "this is a
    /// different database object than the one the cache was built for"
    /// with a single comparison. instance fits 16 bits (65k databases per
    /// process), version 48 bits (281 trillion writes) — wraparound is
    /// beyond any realistic workload.
    ///
    /// While a committed-view scope is armed on this thread, the BEGIN-time
    /// snapshot is returned instead (see `committed_view_epoch`).
    #[inline]
    pub fn write_epoch(&self) -> u64 {
        // Committed-view scope armed on THIS thread: return the BEGIN-time
        // epoch. Hints built against BEGIN-time pages validate against it
        // for the whole transaction, so a concurrent writer's live bumps
        // do not clear and rebuild every reader descent (the 1W+7R shape
        // paid a full hint rebuild + root re-descent per query). Live
        // readers (no scope) and the writer thread keep the live epoch.
        //
        // The COMMITTED_EPOCH_MARK bit keeps committed-scope stamps in a
        // DISJOINT space from live stamps: a hint built by a LIVE-mode
        // read before BEGIN can never validate inside a committed scope —
        // even when the live version has not moved since (the seed-write
        // -> BEGIN -> first-mutation window), because such a hint holds
        // the LIVE PageRef of a page the writer is about to mutate in
        // place. Bit 47 is the version field's top bit: writes never
        // reach 2^47, and the instance id (bits 48..64) stays intact.
        if self.committed_scope_count.load(Ordering::Acquire) > 0 {
            if let Some((pid, _)) = committed_scope() {
                if pid == self.instance_id {
                    let e = self.committed_view_epoch.load(Ordering::Acquire);
                    if e != 0 {
                        return e | COMMITTED_EPOCH_MARK;
                    }
                }
            }
        }
        let v = self.write_version.load(Ordering::Relaxed);
        (self.instance_id << 48) | (v & 0xFFFF_FFFF_FFFF)
    }

    /// Freeze the committed-view hint epoch (see `committed_view_epoch`).
    /// Called at BEGIN, BEFORE the transaction's first mutation can bump
    /// the live version.
    pub fn set_committed_view_epoch(&self, epoch: u64) {
        self.committed_view_epoch.store(epoch, Ordering::Release);
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
        // NOTE: no committed-view entry invalidation here. The alias
        // lifecycle is maintained at PRE-IMAGE CAPTURE time
        // (capture_savepoint_undo) — mutation-side invalidation cannot
        // be made complete: this same-page early-return legitimately
        // skips work, and in-place payload patches bump no epoch. See
        // the capture-side comment for the full invariant.
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
        // Committed-view scope (armed by a reader on a foreign thread while
        // a write transaction is open): serve the BEGIN-time state. The
        // per-pager atomic gate keeps the common (scope-less) path to a
        // single relaxed load — TLS is consulted only while some scope is
        // armed somewhere in the process.
        if self.committed_scope_count.load(Ordering::Acquire) > 0 {
            if let Some((pid, bound)) = committed_scope() {
                if pid == self.instance_id {
                    return self.get_page_committed(id, bound);
                }
            }
        }
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
                self.note_seq_access(id);
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
            // MID-TXN SPILL hit: this page's newest (uncommitted) version
            // lives in a spill frame written when the cache dropped it
            // under pressure. Re-cache it as DIRTY (it is mid-transaction
            // state: a later COMMIT must re-emit it, a later ROLLBACK
            // discards it) and un-spill it — the page is back under normal
            // cache/dirty tracking; its old spill frame becomes dead (a
            // newer frame for the same page always follows in the log).
            let mut spilled_hit = false;
            {
                let mut wal_guard = self.wal.write();
                if let Some(state) = wal_guard.as_mut() {
                    if let Some(offset) = state.spilled.remove(&id) {
                        state.wal.read_frame_at(offset, &mut page.data)?;
                        spilled_hit = true;
                    }
                }
            }
            // DELETE-mode spill hit: same un-spill semantics as the WAL
            // path above — the newest (uncommitted) version re-enters the
            // cache as DIRTY, back under normal dirty tracking.
            if !spilled_hit {
                let mut spill_guard = self.delete_spill.lock();
                if let Some(spill) = spill_guard.as_mut() {
                    if let Some(off) = spill.pages.remove(&id) {
                        spill.read_page_at(off, &mut page.data)?;
                        spilled_hit = true;
                    }
                }
            }
            let dirty_on_insert = spilled_hit;
            // WAL-served read: pages committed to the WAL since the last
            // checkpoint are the newest version — read the frame, not the
            // (stale) main-file page. One read-lock + map probe; falls
            // through to the main file when absent.
            if !spilled_hit {
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
            }
            if dirty_on_insert {
                page.dirty = true;
            }
            let page_ref = Arc::new(Mutex::new(page));
            self.maybe_evict_locked(&mut cache);
            cache.insert(id, page_ref.clone());
            self.lru.lock().push_back(id);
            if dirty_on_insert {
                // Back under dirty tracking: the commit path must see it.
                self.note_dirty(id);
            }
            page_ref
        };
        // Sequential read-ahead: after a +1 run of page touches, batch the
        // next few pages into the cache in one go — cold sequential scans
        // (bulk loads, full-table aggregates, VACUUM) cut their syscall
        // count ~5x. Heavily gated: only the plain file-backed, non-WAL,
        // non-codec, no-committed-scope, no-spill state (every other
        // layout serves pages from a VERSIONED source whose bytes must
        // not be short-circuited from the main file). Prefetched pages
        // enter CLEAN — later fetchers take the normal path (fast-path
        // hit, savepoint capture, WAL check all still run).
        if self.prefetch_run.load(Ordering::Relaxed) >= 2
            && !self.store.is_memory()
            && self.wal.read().is_none()
            && !self.codec.read().is_active()
            && self.committed_scope_count.load(Ordering::Acquire) == 0
            && self.delete_spill.lock().is_none()
            && self.savepoint_depth.load(Ordering::Relaxed) == 0
        {
            const PREFETCH_PAGES: u32 = 4;
            let psz = self.page_size();
            let mut cache = self.cache.write();
            for pid in (id + 1)..(id + 1 + PREFETCH_PAGES) {
                if pid >= n_pages_val {
                    break;
                }
                if cache.get(pid).is_some() {
                    continue;
                }
                let mut p = Page::new(pid, psz);
                let offset = pid as u64 * psz as u64;
                match self.read_file_at(offset, &mut p.data) {
                    Ok(n) if n == psz as usize => {}
                    _ => break, // short read / I/O error: leave it to the real miss path
                }
                self.maybe_evict_locked(&mut cache);
                let prefetched = Arc::new(Mutex::new(p));
                cache.insert(pid, prefetched);
                self.lru.lock().push_back(pid);
            }
        }
        self.note_seq_access(id);
        if self.savepoint_depth.load(Ordering::Relaxed) > 0 {
            self.capture_savepoint_undo(id, &page_ref);
        }
        Ok(page_ref)
    }

    /// Track the +1 access pattern for sequential read-ahead.
    #[inline]
    fn note_seq_access(&self, id: PageId) {
        let last = self.prefetch_last.load(Ordering::Relaxed);
        if id == last.wrapping_add(1) {
            let r = self.prefetch_run.load(Ordering::Relaxed);
            if r < 8 {
                self.prefetch_run.store(r + 1, Ordering::Relaxed);
            }
        } else {
            self.prefetch_run.store(0, Ordering::Relaxed);
        }
        self.prefetch_last.store(id, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------
    // committed-view read path
    // -----------------------------------------------------------------

    /// Arm the thread-local committed-view scope for THIS pager.
    /// `bound` = committed n_pages (the transaction's BEGIN-time page
    /// count). Callers must hold the engine read lock (or otherwise
    /// exclude concurrent writer statements) for the scope's duration.
    pub fn arm_committed_view(&self, bound: u32) -> CommittedViewGuard<'_> {
        let prev = COMMITTED_VIEW.with(|c| c.replace(Some((self.instance_id, bound))));
        // Raise the gate: the RMW makes the scope visible to every
        // thread's gate load atomically — foreign threads that see the
        // count > 0 fall through to their own (empty) TLS scope and take
        // the live path, while same-thread consumers see it in program
        // order.
        self.committed_scope_count.fetch_add(1, Ordering::AcqRel);
        CommittedViewGuard {
            prev,
            advisory_gen_at_arm: crate::storage::btree::advisory_state_gen(),
            pager: self,
        }
    }

    /// Decrement the scope count; true when it reached zero — the gate
    /// is the count itself, so there is nothing else to lower. Saturating
    /// (never wraps below zero): a defensive `disarm_reader_scope`
    /// racing a still-live guard's own drop must not poison the count for
    /// the process lifetime.
    fn scope_count_dec(&self) -> bool {
        let mut cur = self.committed_scope_count.load(Ordering::Acquire);
        loop {
            if cur == 0 {
                return false;
            }
            match self.committed_scope_count.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return cur == 1,
                Err(actual) => cur = actual,
            }
        }
    }

    /// True while this thread has a committed-view scope armed for THIS
    /// pager (read paths use this to skip read-side caches that are
    /// keyed on the LIVE state, e.g. the table count cache). Gated by the
    /// per-pager atomic so the TLS is touched only while a scope is (or
    /// was) armed.
    #[inline]
    pub fn committed_reads_armed(&self) -> bool {
        if self.committed_scope_count.load(Ordering::Acquire) == 0 {
            return false;
        }
        match committed_scope() {
            Some((pid, _)) => pid == self.instance_id,
            None => false,
        }
    }

    /// The committed n_pages bound of the open transaction: the base of
    /// the bottom (`__begin__`) savepoint, or the live page count when
    /// no transaction is open (committed == live then). Single Acquire
    /// load — maintained by the savepoint lifecycle methods (see
    /// `committed_bound_cache`); the savepoints Mutex is never taken on
    /// the reader path.
    pub fn committed_bound(&self) -> u32 {
        let b = self.committed_bound_cache.load(Ordering::Acquire);
        if b != 0 {
            return b;
        }
        self.n_pages.load(Ordering::Acquire)
    }

    /// Drop all committed-view materialized pages. Called at every
    /// transaction boundary (BEGIN / COMMIT / ROLLBACK): after a boundary
    /// the BEGIN-time state is either restored (rollback), superseded
    /// (commit — the live cache is now the committed state), or
    /// re-baselined (a new BEGIN).
    ///
    /// The epoch bump is the INVALIDATION EVENT for the per-thread
    /// committed-view memos (see `COMMITTED_MEMO`): every entry the
    /// clear removes must also die in every thread's memo, and the
    /// moved epoch is what proves that on the next probe. (BEGIN does
    /// NOT clear — see `set_committed_view_epoch` — but the boundary
    /// that clears here always moves the token, and a BEGIN without
    /// intervening writes stores a token whose content provably did
    /// not change.)
    pub fn clear_committed_view(&self) {
        self.committed_view_epoch.fetch_add(1, Ordering::AcqRel);
        self.committed_pages.write().clear();
    }

    /// Disarm any committed-view scope this thread holds for THIS pager
    /// (writers clear it at `execute` entry — a leaked reader scope must
    /// never serve BEGIN-time pages to a write path). Scoped guards
    /// restore their own previous value on drop; this is the defensive
    /// belt for scope-free callers.
    pub fn disarm_reader_scope(&self) {
        if let Some((pid, _)) = committed_scope() {
            if pid == self.instance_id {
                COMMITTED_VIEW.with(|c| c.set(None));
                // Deliberately NO count decrement: this is the defensive
                // belt, and the pairing arm's guard may still drop later
                // and decrement itself. A spurious extra decrement could
                // zero the count while another live scope exists — gating
                // it off and serving LIVE pages inside it. A stuck-high
                // count (the true-leak case) only costs one TLS probe per
                // get_page on scope-less threads: fail-safe, never
                // isolation-breaking.
            }
        }
    }

    /// Serve page `id` from the BEGIN-time (committed) view while a
    /// foreign write transaction is open. Never inserts into the live
    /// cache and never consults the writer's dirty or spilled state.
    fn get_page_committed(&self, id: PageId, bound: u32) -> Result<PageRef> {
        // Pages allocated after BEGIN do not exist in the committed view.
        if id >= bound && id != 0 {
            return Err(Error::corruption(format!(
                "page {} out of range in committed view (n_pages={})",
                id, bound
            )));
        }
        // Reader-side materialization cache: a hot leaf is read once and
        // the PageRef reused (this is what keeps committed-view scans at
        // live-cache speed — the 8 KiB pre-image copy is amortized to one
        // per page per transaction). Entries derived from pre-images set
        // the epoch-poison signal; ALIAS entries (path 3: the page was
        // never fetched by the writer, so the live bytes ARE the committed
        // bytes) do not — poisoning them would retire the advisory hint
        // state on every committed-scope drop, and since every query
        // rebuilds hints, every drop would bump write_version: a
        // reader-induced invalidation feedback loop that kept 1W+7R
        // readers rebuilding their descent hints per query.
        //
        // Fastest tier — the per-thread MEMO: no shared lock, no shared
        // cache-line RMW. Validated by the committed-view epoch (moved by
        // EVERY invalidating event: capture-kill bump, clear bump,
        // clear_savepoints reset-to-zero) and the pager instance; a
        // matching token proves the entry is still the shared map's
        // current content. Under N concurrent committed-view readers the
        // shared map's read lock is a ping-ponging cache line — this
        // tier is what keeps reader throughput at uncontended speed.
        let token = self.committed_view_epoch.load(Ordering::Acquire);
        let memo_hit: Option<CommittedEntry> = COMMITTED_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            match m.as_mut() {
                Some(memo) if memo.instance == self.instance_id && memo.token == token => {
                    memo.pages.get(&id).cloned()
                }
                _ => None,
            }
        });
        if let Some(entry) = memo_hit {
            if !entry.alias {
                self.committed_poison.store(true, Ordering::Release);
            }
            return Ok(entry.pr);
        }
        // Shared tier: one read-lock probe. On hit, also populate the
        // memo (resetting it first when its token/instance went stale).
        {
            let cp = self.committed_pages.read();
            if let Some(entry) = cp.get(&id) {
                let entry = entry.clone();
                drop(cp);
                if !entry.alias {
                    self.committed_poison.store(true, Ordering::Release);
                }
                self.memo_committed_page(id, &entry, token);
                return Ok(entry.pr);
            }
        }
        let psz = self.page_size();
        // 1. The __begin__ savepoint's pre-image: the writer fetched this
        //    page during the transaction (and may have mutated the live
        //    copy) — the pre-image is exactly its BEGIN-time bytes.
        {
            let sp = self.savepoints.lock();
            if let Some(bytes) = sp.first().and_then(|lvl| lvl.pages.get(&id)) {
                let mut page = Page::new(id, psz);
                page.data.copy_from_slice(bytes);
                let pr: PageRef = Arc::new(Mutex::new(page));
                let entry = CommittedEntry { pr, alias: false };
                self.cache_committed_page(id, &entry);
                self.memo_committed_page(id, &entry, token);
                // BEGIN-time bytes ≠ live cache for writer-fetched pages:
                // the scope's drop must retire advisory-cache state built
                // from these bytes (single write-epoch bump).
                self.committed_poison.store(true, Ordering::Release);
                return Ok(entry.pr);
            }
        }
        // 2. Never touched by the writer this transaction: the committed
        //    bytes are, in order: the live cache (in-memory stores never
        //    write the backing image; evicted file-store pages live in
        //    the WAL / main file).
        // 3. Not fetched by the writer during THIS transaction (no
        //    pre-image): every pre-existing-page mutation goes through
        //    `get_page` (capture) — so the live cache's bytes ARE the
        //    committed bytes. This covers in-memory stores (flush is a
        //    no-op, the dirty set accumulates forever, the backing image
        //    is never written) and file stores alike. The live PageRef
        //    is MEMOIZED into committed_pages: an alias is safe because
        //    (a) a page eligible for this path was never fetched by the
        //    writer, and a writer cannot mutate a page without fetching
        //    it first, and (b) the FIRST fetch of the page in any later
        //    statement invalidates the entry (capture_savepoint_undo)
        //    while the engine-level write lock still excludes every
        //    reader from the fetch->capture window. The old no-memoize
        //    rule ("an alias would serve uncommitted bytes") predates
        //    capture-side invalidation; with it, aliasing turns repeated
        //    committed-view scans of untouched ranges from per-query
        //    lock cascades into a single Arc clone per page.
        if let Some(page_ref) = self.cache.read().get(id).cloned() {
            let entry = CommittedEntry {
                pr: page_ref,
                alias: true,
            };
            self.cache_committed_page(id, &entry);
            self.memo_committed_page(id, &entry, token);
            return Ok(entry.pr);
        }
        let mut page = Page::new(id, psz);
        let mut served_from_wal = false;
        {
            let wal_guard = self.wal.read();
            if let Some(state) = wal_guard.as_ref() {
                if let Some(&offset) = state.map.get(&id) {
                    state.wal.read_frame_at(offset, &mut page.data)?;
                    served_from_wal = true;
                }
            }
        }
        if !served_from_wal {
            let codec_active = self.codec.read().is_active();
            if codec_active && id != 0 {
                let offset = id as u64 * psz as u64;
                let mut raw = vec![0u8; psz as usize];
                let n = self.read_file_at(offset, &mut raw)?;
                if n != psz as usize {
                    return Err(Error::corruption(format!(
                        "short read on page {}: {} of {} bytes",
                        id, n, psz as usize
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
                        id, n, psz as usize
                    )));
                }
            }
        }
        let pr: PageRef = Arc::new(Mutex::new(page));
        let entry = CommittedEntry { pr, alias: false };
        self.cache_committed_page(id, &entry);
        self.memo_committed_page(id, &entry, token);
        Ok(entry.pr)
    }

    /// Materialization-cache insert with a hard page cap: the committed
    /// view must never grow the process's footprint beyond the readers'
    /// working set. Below the cap, a hot page is copied ONCE per
    /// transaction (scans then run at live-cache speed); at the cap, the
    /// page is re-materialized per miss — slower, but the memory budget
    /// holds (1k pages = 4 MiB at the default page size).
    fn cache_committed_page(&self, id: PageId, entry: &CommittedEntry) {
        let mut cp = self.committed_pages.write();
        if cp.len() < COMMITTED_PAGES_CAP {
            cp.insert(id, entry.clone());
            // Latch: once any entry exists, the mutation path must keep
            // invalidating (see note_dirty / clear_savepoints). Never
            // unlatched — a stale `false` would skip needed removals.
            self.committed_view_used.store(true, Ordering::Release);
        }
    }

    /// Insert into the per-thread committed-view memo (the no-lock
    /// acceleration tier). A stale token or foreign instance resets the
    /// whole memo first — the epoch moved, so every old entry died with
    /// the shared map's content. The cap keeps scan-shaped readers from
    /// pinning unbounded pages per thread.
    fn memo_committed_page(&self, id: PageId, entry: &CommittedEntry, token: u64) {
        COMMITTED_MEMO.with(|m| {
            let mut m = m.borrow_mut();
            let reset = match m.as_ref() {
                Some(memo) => memo.instance != self.instance_id || memo.token != token,
                None => true,
            };
            if reset {
                *m = Some(CommittedMemo {
                    instance: self.instance_id,
                    token,
                    pages: std::collections::HashMap::default(),
                });
            }
            if let Some(memo) = m.as_mut() {
                if memo.pages.len() < COMMITTED_MEMO_CAP || memo.pages.contains_key(&id) {
                    memo.pages.insert(id, entry.clone());
                }
            }
        });
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

    /// Bulk `allocate_page` (zeroed fresh pages — see
    /// `allocate_pages_opts` for the no-zero overflow-chain variant).
    pub fn allocate_pages(&self, n: usize) -> Result<Vec<(PageId, PageRef)>> {
        self.allocate_pages_opts(n, true)
    }

    /// Bulk `allocate_page`: hands out `n` pages, returning the live
    /// `PageRef` for each (no per-page `get_page` round trip). One
    /// cache-write critical section + one LRU lock for the whole batch
    /// instead of one of each per page — the overflow-chain writer (a
    /// 64 KB blob = ~17 pages) previously paid ~17 lock round trips per
    /// row. Freelist-reused pages are zeroed here exactly as in the
    /// single-page path (recycled bodies may still hold old bytes);
    /// `zeroed: false` skips the fresh-page calloc for callers that
    /// overwrite every byte by construction (the overflow-chain writer).
    pub fn allocate_pages_opts(&self, n: usize, zeroed: bool) -> Result<Vec<(PageId, PageRef)>> {
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
                let mut page = if zeroed {
                    Page::new(id, psz)
                } else {
                    // SAFETY (contract): the caller (overflow-chain
                    // writer) overwrites every byte before the page is
                    // read back or flushed. Single-writer model: no flush
                    // can observe the page between here and the fill.
                    Page::new_uninit(id, psz)
                };
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
        *guard = Some(WalState {
            wal,
            map,
            spilled: Default::default(),
        });
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
        // Mid-transaction spill frames must NOT reach the main file
        // (they are uncommitted), and a WAL reset would invalidate their
        // offsets. Checkpoints run at COMMIT end / close (spilled is
        // empty by then); an explicit mid-txn PRAGMA wal_checkpoint
        // defers to the next commit-time auto-checkpoint.
        if !state.spilled.is_empty() {
            return Ok(());
        }
        let psz = self.page_size();
        // Frames for pages at or beyond an ARMED truncation bound are
        // about to be discarded by this checkpoint's own set_len —
        // copying them into the main file first is pure waste (VACUUM's
        // tail retirement leaves hundreds of dead frames in the WAL).
        let trunc = self.pending_truncate.load(Ordering::Acquire);
        // Sort page ids for sequential main-file writes.
        let mut ids: Vec<PageId> = state
            .map
            .keys()
            .copied()
            .filter(|&id| trunc == 0 || (id as u64) < trunc as u64)
            .collect();
        ids.sort_unstable();
        // CACHE-FIRST SERVE: every page-cached id is served by a ~100 ns
        // cache hit + memcpy instead of a ~1 us WAL pread; only cache
        // misses pay the frame read under the lock below. The pages are
        // probed LAZILY, one at a time inside the run loop (a ~4 KB copy
        // per probe, bounded by the 1 MiB run buffer's lifetime): the
        // previous eager gather cloned EVERY cached page up front — up to
        // a full 512-page cache = 2 MiB of transient copies on every
        // checkpoint — which stacked on the run buffer's own peak.
        let cache_read_page = |id: PageId| -> Option<Vec<u8>> {
            let cache = self.cache.read();
            cache.get(id).map(|pr| {
                let b = pr.lock();
                b.data.clone()
            })
        };
        // COALESCED RUNS: consecutive page ids are contiguous in the
        // main file; when their frame offsets are ALSO consecutive
        // (flush_wal appends sorted, so they usually are), one read+write
        // pair moves the whole run. A 500-page commit checkpoints as ONE
        // 2 MiB pwrite instead of 500 4 KiB syscalls.
        let flush_run = |run_id: u64, buf: &[u8]| -> Result<()> {
            if !buf.is_empty() {
                self.write_file_at(run_id * psz as u64, buf)?;
            }
            Ok(())
        };
        let mut run_start: Option<u64> = None; // first page id of the current run
        let _run_off: u64 = 0;
        let mut run_buf: Vec<u8> = Vec::new();
        // Bounded coalescing: cap the run buffer at 256 pages (1 MiB at
        // the default page size). Unbounded accumulation materialized the
        // ENTIRE committed WAL in RAM during a big commit's checkpoint —
        // a 7250-page (29 MiB) first-build commit peaked the process at
        // ~40 MiB where SQLite checkpoints in bounded passes. 1 MiB
        // pwrites are already contiguous and near-optimal for throughput;
        // splitting longer runs into 1 MiB chunks trades nothing
        // measurable for a hard RSS bound.
        let run_buf_max: usize = psz as usize * 256;
        let mut frame = vec![0u8; psz as usize];
        for &id in ids.iter() {
            if id == 0 {
                continue; // header goes LAST (crash safety)
            }
            let cached_page = cache_read_page(id);
            let bytes: &[u8] = if let Some(b) = cached_page.as_deref() {
                b
            } else {
                let off = state.map[&id];
                state.wal.read_frame_at(off, &mut frame)?;
                &frame[..]
            };
            let contiguous = match run_start {
                Some(s) => (s + (run_buf.len() / psz as usize) as u64) == id as u64,
                None => false,
            };
            if !contiguous || run_buf.len() >= run_buf_max {
                flush_run(run_start.unwrap_or(0), &run_buf)?;
                run_buf.clear();
                run_start = Some(id as u64);
                let _ = state.map[&id];
            }
            run_buf.extend_from_slice(bytes);
        }
        flush_run(run_start.unwrap_or(0), &run_buf)?;
        // Header (page 0) LAST (crash safety — see flush_inner_delete's
        // ordering contract): the n_pages-bearing header commit lands
        // after every other page.
        if let Some(&offset) = state.map.get(&0) {
            if trunc == 0 {
                let mut buf = vec![0u8; psz as usize];
                state.wal.read_frame_at(offset, &mut buf)?;
                self.write_file_at(0, &buf)?;
            } else {
                // The header carries the truncation's new page count —
                // write it even when a truncation is armed (it is the
                // commit record for the compact state). Prefer the
                // in-cache bytes (flush_wal refreshed them from the
                // atomics).
                let buf: Vec<u8> = {
                    let cache = self.cache.read();
                    cache
                        .get(0)
                        .map(|pr| {
                            let b = pr.lock();
                            b.data.clone()
                        })
                        .unwrap_or_default()
                };
                if !buf.is_empty() {
                    self.write_file_at(0, &buf)?;
                } else {
                    let mut b = vec![0u8; psz as usize];
                    state.wal.read_frame_at(offset, &mut b)?;
                    self.write_file_at(0, &b)?;
                }
            }
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
        // SQLite WAL durability semantics at checkpoint: OFF never syncs;
        // NORMAL syncs the WAL's committed frames to disk before the WAL
        // reset discards them (the durability point for NORMAL commits);
        // FULL/extra additionally synced at commit time (flush_wal). The
        // old unconditional sync made `PRAGMA synchronous=OFF` pay a full
        // fsync per checkpoint — a parity break vs SQLite (and ~2 ms on
        // rotational-ish storage per VACUUM).
        let sync_mode = self.synchronous.load(Ordering::Acquire);
        if sync_mode >= 1 && !self.skip_fsync.load(Ordering::Acquire) {
            self.store.sync_all()?;
        }
        state.wal.reset_synced(sync_mode >= 1)?;
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
        let mut dirty_ids: Vec<PageId> = {
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
        // Page-ordered frames: a sorted append makes consecutive pages
        // land at consecutive WAL offsets, which lets `checkpoint_wal`
        // coalesce them into one contiguous read+write per run (a
        // 500-page commit checkpoints as ONE 2 MiB pwrite instead of 500
        // 4 KiB syscalls).
        dirty_ids.sort_unstable();

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
            // Absorb the mid-txn SPILL frames: they sit in the log BEFORE
            // this commit's trailing commit frame, so recovery replays
            // them as part of this commit — register their offsets as the
            // committed version. A spill frame is ALWAYS newer than any
            // map entry from a PREVIOUS commit (the page was dirtied
            // after that commit), so plain insert is the correct
            // precedence; the only newer frames are THIS batch's, for
            // pages that spilled and were then re-fetched and re-dirtied
            // (they left `spilled` at get_page time, so the two sets are
            // disjoint by construction — the guard below is defensive).
            let batch_ids: std::collections::HashSet<PageId> =
                frame_offsets.iter().map(|(id, _)| *id).collect();
            let spilled: Vec<(PageId, u64)> = state.spilled.drain().collect();
            for (id, off) in spilled {
                if !batch_ids.contains(&id) {
                    state.map.insert(id, off);
                }
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

    /// Install a fully compacted database image IN PLACE (VACUUM's
    /// write-back). The image is a complete database: pages
    /// `0..new_n` replace the old content, every page `>= new_n` is
    /// gone, the freelist is empty, and page 0's header fields (except
    /// the ones `flush_wal` refreshes from atomics — n_pages, freelist,
    /// schema cookie) ride the image bytes (user_version /
    /// application_id carry over).
    ///
    /// The install lands as ONE WAL commit — frames for every image page
    /// with the trailing commit frame — which is exactly SQLite's own
    /// VACUUM shape: a crash mid-install leaves recovery at the last
    /// real commit (the pre-VACUUM state, fully consistent); a crash
    /// after the commit marker yields the compact state. The explicit
    /// checkpoint afterwards copies the compact pages into the main
    /// file, truncates the tail, and resets the WAL, so the steady
    /// state after VACUUM is a checkpointed main file — same as
    /// SQLite.
    ///
    /// WAL mode only (DELETE-mode flushes cannot make a whole-tree
    /// replacement atomic — the header-last ordering contract protects
    /// page additions, not a full rewrite; callers fall back to the
    /// rewrite-and-reopen path there).
    pub fn install_compact_image(&self, image: &[u8]) -> Result<()> {
        if self.wal.read().is_none() {
            return Err(Error::InvalidArgument(
                "install_compact_image requires WAL mode".into(),
            ));
        }
        let psz = self.page_size() as usize;
        if image.is_empty() || image.len() % psz != 0 {
            return Err(Error::corruption(format!(
                "vacuum image not page-aligned (len={}, page_size={})",
                image.len(),
                psz
            )));
        }
        let new_n = (image.len() / psz) as u32;
        let old_n = self.n_pages.load(Ordering::Acquire);

        // 1. Retire every page >= new_n through the standard truncate
        //    path: scrubs the cache / LRU / dirty set / WAL map, rewinds
        //    n_pages, arms the physical truncation, and bumps the write
        //    version (advisory caches built over the old tree must not
        //    survive). Runs BEFORE the image pages are installed so the
        //    freelist-trunk walk reads the OLD state.
        if new_n < old_n {
            let freed: Vec<PageId> = (new_n..old_n).collect();
            self.truncate_tail(&freed)?;
        }

        // 2. Install the image pages as dirty (the flush below emits them
        //    as WAL frames). Pages already cached are overwritten in
        //    place; pages not in the cache are created directly — no
        //    disk read, the bytes come from the image.
        for (i, chunk) in image.chunks_exact(psz).enumerate() {
            let id = i as PageId;
            let page_ref = {
                let cache = self.cache.read();
                cache.get(id).cloned()
            };
            let page_ref = match page_ref {
                Some(pr) => pr,
                None => {
                    let pr: PageRef =
                        Arc::new(Mutex::new(crate::storage::page::Page::new(id, psz as u32)));
                    let mut cache = self.cache.write();
                    cache.insert(id, pr.clone());
                    pr
                }
            };
            {
                let mut p = page_ref.lock();
                p.data.copy_from_slice(chunk);
                p.dirty = true;
            }
            self.note_dirty(id);
            self.dirty_count_approx.fetch_add(1, Ordering::Relaxed);
        }

        // 3. The compact image has no freelist; the schema moved (roots
        //    were remapped), so the cookie must change. flush_wal writes
        //    page 0's header FROM these atomics — setting them here makes
        //    the committed frame self-consistent. (NEVER the
        //    direct-file-write `bump_schema_cookie`: a write outside the
        //    WAL would be invisible to WAL-served readers and clobbered
        //    by the next checkpoint.)
        self.freelist_head.store(0, Ordering::Release);
        self.freelist_count.store(0, Ordering::Release);
        self.schema_cookie.fetch_add(1, Ordering::AcqRel);

        // 4. The single VACUUM commit: every image page as frames, the
        //    last frame carrying the commit marker. (flush_wal refreshes
        //    page 0 from the atomics first, so the header frame is the
        //    new one.)
        self.flush()?;

        // 5. Copy the compact pages into the main file, apply the armed
        //    tail truncation, sync, and reset the WAL. The main file IS
        //    the compact database afterwards; the sidecar holds only its
        //    fresh header.
        self.checkpoint_wal()?;

        // 6. Stale BEGIN-time pre-images from earlier committed-view
        //    scopes must not serve old-tree bytes over the compact
        //    state.
        self.clear_committed_view();
        Ok(())
    }

    /// True when the pager is in WAL journal mode.
    pub fn wal_active(&self) -> bool {
        self.wal.read().is_some()
    }

    /// IN-PLACE VACUUM install (WAL file stores, no codec): compact the
    /// live pages down to a dense prefix {0, 1, .., n-1} directly in the
    /// page cache, then commit once and checkpoint — SQLite's own VACUUM
    /// shape, without building a transient image. The plan (see
    /// `vacuum::plan_in_place_compaction`) is MONOTONE: every page moves
    /// to a slot <= its old id, so an ascending-new-id move loop can
    /// never overwrite a source page before its bytes are read.
    ///
    /// Per page: one 8 KiB copy when it moves (zero when it stays), plus
    /// surgical 4-byte reference patches (interior children, right-most
    /// pointers, leaf overflow-chain heads, overflow next links) — only
    /// the patches whose referenced page actually moved. A fully-dense
    /// database (the steady state after a previous VACUUM, or
    /// bulk-load + suffix-DELETE churn) moves NOTHING: the whole VACUUM
    /// is one header commit + tail truncation.
    ///
    /// Crash safety: identical to `install_compact_image` — the single
    /// commit frame's header atomically records the new n_pages,
    /// freelist, and cookie; the checkpoint then applies the armed
    /// physical truncation.
    pub(crate) fn install_in_place_compaction(
        &self,
        plan: &crate::storage::vacuum::InPlaceCompaction,
    ) -> Result<()> {
        if self.wal.read().is_none() {
            return Err(Error::InvalidArgument(
                "install_in_place_compaction requires WAL mode".into(),
            ));
        }
        let old_n = self.n_pages.load(Ordering::Acquire);
        let new_n = plan.sorted.len() as u32;
        // 1. THE MOVES (ascending new id — see the monotonicity argument
        //    above). Slot k's destination page is created directly on a
        //    cache miss (no disk read: whatever bytes it holds are about
        //    to be overwritten).
        let psz = self.page_size() as usize;
        for k in 1..new_n {
            let o = plan.sorted[k as usize];
            if o == k && !plan.refs.contains_key(&o) {
                continue; // identity, no referenced page moved
            }
            let o_ref = if o == k {
                None // patch in place: no copy
            } else {
                Some(self.get_page(o)?)
            };
            // Destination: cached, or created fresh (no file read).
            let dst = {
                let cache = self.cache.read();
                cache.get(k).cloned()
            };
            let dst = match dst {
                Some(pr) => pr,
                None => {
                    let pr: PageRef =
                        Arc::new(Mutex::new(crate::storage::page::Page::new(k, psz as u32)));
                    let mut cache = self.cache.write();
                    cache.insert(k, pr.clone());
                    pr
                }
            };
            if let Some(src) = &o_ref {
                let s = src.lock();
                let mut d = dst.lock();
                d.data.copy_from_slice(&s.data);
                d.dirty = true;
            }
            // Reference patches: rewrite the 4-byte page ids whose
            // targets moved. Identity pages take surgical in-place
            // writes only.
            if let Some(rlist) = plan.refs.get(&o) {
                let mut d = dst.lock();
                let mut changed = false;
                for &(off, old_ref) in rlist {
                    if let Some(&nr) = plan.map.get(&old_ref) {
                        if nr != old_ref {
                            let off = off as usize;
                            if off + 4 <= d.data.len() {
                                d.data[off..off + 4].copy_from_slice(&nr.to_be_bytes());
                                changed = true;
                            }
                        }
                    }
                }
                if changed {
                    d.dirty = true;
                }
            }
            self.note_dirty(k);
            self.dirty_count_approx.fetch_add(1, Ordering::Relaxed);
        }
        // 1b. Schema-row rootpage patches: the roots that moved are
        //     re-anchored in the schema rows' column-3 bodies (same tag,
        //     same byte width — verified at plan time). Applied at each
        //     schema leaf's NEW slot after the moves.
        for patch in &plan.schema_patches {
            let slot = plan
                .map
                .get(&patch.leaf_old)
                .copied()
                .unwrap_or(patch.leaf_old);
            let leaf = self.get_page(slot)?;
            {
                let mut d = leaf.lock();
                let off = patch.body_off as usize;
                let end = off + patch.width as usize;
                if end <= d.data.len() {
                    let v = patch.new_root as i64;
                    for i in 0..patch.width as usize {
                        d.data[off + i] = ((v >> (8 * i)) & 0xFF) as u8; // LE body
                    }
                    d.dirty = true;
                } else {
                    return Err(Error::corruption(format!(
                        "in-place vacuum: schema rootpage patch out of bounds (page {slot})"
                    )));
                }
            }
            self.note_dirty(slot);
            self.dirty_count_approx.fetch_add(1, Ordering::Relaxed);
        }
        // 2. Reset the freelist BEFORE the tail retirement: moved pages
        //    may have overwritten in-range freelist trunk pages, so the
        //    trunk-chain scrub inside truncate_tail must not walk them.
        //    The compact image has no freelist by construction.
        self.freelist_head.store(0, Ordering::Release);
        self.freelist_count.store(0, Ordering::Release);
        // 3. Retire the tail through the standard truncate path (cache /
        //    LRU / dirty / WAL-map eviction, n_pages rewind, armed
        //    physical truncation). With the head reset above, the scrub
        //    is a no-op walk.
        if new_n < old_n {
            let freed: Vec<PageId> = (new_n..old_n).collect();
            self.truncate_tail(&freed)?;
        }
        // 4. The cookie must change — statement plans keyed on it
        //    rebuild; flush_wal materializes page 0's header FROM these
        //    atomics.
        self.schema_cookie.fetch_add(1, Ordering::AcqRel);
        // 5. Ensure flush() does not take its O(1) clean fast path
        //    (new_n == old_n with no moves retired nothing).
        self.dirty_count_approx.fetch_add(1, Ordering::Relaxed);
        // 6. The single VACUUM commit: the refreshed header page plus
        //    every moved page as WAL frames.
        self.flush()?;
        // 7. Copy the committed frames into the main file, apply the
        //    armed truncation, and reset the WAL.
        self.checkpoint_wal()?;
        // 8. Stale BEGIN-time pre-images from earlier committed-view
        //    scopes must not serve old-tree bytes over the compact state.
        self.clear_committed_view();
        Ok(())
    }

    /// Retire this pager: its database file is about to be replaced out
    /// from under it (VACUUM's rewrite-and-reopen path). A retired pager
    /// skips ALL close-time bookkeeping in `Drop` — the checkpoint and
    /// sidecar removal there would write the OLD pager's view back over
    /// the freshly installed image (an empty checkpoint still re-asserts
    /// the old page count via `set_len`) and delete sidecars owned by
    /// the fresh pager that replaces it.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }

    /// Discard the DELETE-mode spill sidecar (records + file). Called
    /// from the ROLLBACK paths (the spilled records are uncommitted
    /// work) and best-effort from `Drop`.
    fn drop_delete_spill(&self) {
        let mut spill_guard = self.delete_spill.lock();
        if let Some(spill) = spill_guard.take() {
            drop(spill);
            let _ = std::fs::remove_file(spill_path_for(&self.path));
        }
    }

    /// True when no page is currently spilled to the DELETE-mode
    /// sidecar (quiescent-state check for journal-mode switches and
    /// integrity probes).
    pub fn delete_spill_empty(&self) -> bool {
        self.delete_spill
            .lock()
            .as_ref()
            .map_or(true, |s| s.pages.is_empty())
    }

    /// DELETE-mode rollback of the bottom savepoint: the spill records
    /// are uncommitted mutations being rolled back — discard them. (The
    /// pre-images of pre-existing pages restore IN-CACHE; pages
    /// allocated this transaction are dropped from the cache, and their
    /// spilled records die with this drop.)
    pub fn drop_spill_for_rollback(&self) {
        self.drop_delete_spill();
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

        // DELETE-MODE SPILL DRAIN: mid-transaction cache-pressure evictions
        // wrote the newest uncommitted versions to the sidecar (never the
        // main file). This is the COMMIT moment — copy every spilled record
        // into the main file, still BEFORE the header write (the crash
        // ordering contract: data pages first, header last). A partially
        // failed drain behaves exactly like a failed dirty-page write above
        // (the error propagates; ROLLBACK discards the rest).
        {
            let mut spill_guard = self.delete_spill.lock();
            if let Some(spill) = spill_guard.as_mut() {
                if !spill.pages.is_empty() {
                    let psz = self.page_size() as usize;
                    let mut buf = vec![0u8; psz];
                    let entries: Vec<(PageId, u64)> = spill.pages.drain().collect();
                    for (id, off) in entries {
                        spill.read_page_at(off, &mut buf)?;
                        self.codec_write_page(id, &buf)?;
                    }
                    // Reset the sidecar for the next transaction (truncate
                    // in place — cheaper than delete + recreate).
                    use std::io::Write;
                    let _ = spill.file.set_len(0);
                    let _ = spill.file.flush();
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
        // 0. DROP the DELETE-mode spill: its records are this
        // transaction's uncommitted work (the main file was never
        // touched mid-txn), so rollback simply discards them.
        self.drop_delete_spill();
        // 1. Drop the entire cache.
        {
            let mut cache = self.cache.write();
            cache.clear();
        }
        self.lru.lock().clear();
        self.dirty_pages.lock().clear();
        // BEGIN-level rollback: ALL uncommitted spill frames are dead work
        // (recovery ignores trailing uncommitted frames; in-process, the
        // next fetch of any of these pages must read the committed
        // version — the map or the file).
        if let Some(state) = self.wal.write().as_mut() {
            state.spilled.clear();
        }
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
        while cache.len() >= self.cache_capacity.load(Ordering::Relaxed) && attempts > 0 {
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
                    // PIN GUARD: a strong count above 1 means btree/pager
                    // code still holds this page in flight — a split
                    // holding the leaf while allocating its sibling, an
                    // overflow-chain guard, a patch loop. Evicting it now
                    // would orphan every write the holder makes afterwards
                    // (the spill frame would capture pre-write bytes and
                    // the holder's Arc would drift from the cache). This
                    // is SQLite's page pinning, expressed through the Arc.
                    // Race-free: all clones flow through the cache locks,
                    // and this whole check runs under the write lock.
                    if Arc::strong_count(p) > 1 {
                        false
                    } else if p.lock().dirty {
                        let mut pg = p.lock();
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
                        } else if evict_id != 0 {
                            // WAL SPILL: append the dirty page as an
                            // UNCOMMITTED frame and drop it from the cache.
                            // This is what bounds RSS during big write
                            // transactions (SQLite's own pager design): the
                            // newest version lives in the WAL, `get_page`
                            // misses read it back through the `spilled`
                            // index, and the frames become committed by the
                            // next COMMIT's trailing commit frame.
                            //
                            // try_write: eviction holds the cache write
                            // lock, and the commit path holds the WAL write
                            // lock while probing the cache — a blocking
                            // acquisition here is an ABBA deadlock. On a
                            // rare contention miss the page stays cached
                            // (pushed back) and a later pass retries.
                            let guard = self.wal.try_write();
                            if let Some(mut guard) = guard {
                                if let Some(state) = guard.as_mut() {
                                    match state.wal.append(evict_id, &pg.data, false) {
                                        Ok(off) => {
                                            state.spilled.insert(evict_id, off);
                                            true
                                        }
                                        Err(_) => false,
                                    }
                                } else {
                                    // DELETE mode (no WAL): spill the dirty
                                    // page to the sidecar so eviction can
                                    // proceed WITHOUT touching the main file
                                    // mid-transaction (its bytes must stay
                                    // pre-BEGIN until COMMIT). The newest
                                    // version is re-read from the spill on
                                    // the next get_page miss; COMMIT drains
                                    // the records into the main file; a
                                    // failure keeps the page cached (retry).
                                    let mut spill = self.delete_spill.lock();
                                    match spill.as_mut() {
                                        Some(s) => match s.append(evict_id, &pg.data) {
                                            Ok(off) => {
                                                s.pages.insert(evict_id, off);
                                                true
                                            }
                                            Err(_) => false,
                                        },
                                        None => {
                                            let path = spill_path_for(&self.path);
                                            match OpenOptions::new()
                                                .read(true)
                                                .write(true)
                                                .create(true)
                                                .truncate(true)
                                                .open(&path)
                                            {
                                                Ok(file) => {
                                                    let mut s = DeleteSpill {
                                                        file,
                                                        pages: std::collections::HashMap::default(),
                                                    };
                                                    match s.append(evict_id, &pg.data) {
                                                        Ok(off) => {
                                                            s.pages.insert(evict_id, off);
                                                            *spill = Some(s);
                                                            true
                                                        }
                                                        Err(_) => false,
                                                    }
                                                }
                                                Err(_) => false,
                                            }
                                        }
                                    }
                                }
                            } else {
                                false
                            }
                        } else {
                            false // page 0 (header): commit always rewrites it in-cache
                        }
                    } else {
                        true
                    }
                }
                None => true,
            };
            if should_evict {
                cache.remove(evict_id);
                if self.wal.read().is_some() {
                    // The spilled page is no longer in the cache: take it
                    // out of the dirty tracking so the commit batch's
                    // cache probe doesn't wait on a page that isn't there
                    // (its frame is already in the WAL).
                    self.dirty_pages.lock().remove(&evict_id);
                }
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

    /// PRAGMA parallel_scan: the minimum estimated row count at which
    /// single-table aggregate scans split across worker threads. 0 =
    /// disabled (always serial). See `executor::parallel`.
    pub fn parallel_scan_min_rows(&self) -> i64 {
        self.parallel_scan_min_rows
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// PRAGMA parallel_scan write side. 0/negative disables; a positive
    /// value is the custom min-rows threshold.
    pub fn set_parallel_scan_min_rows(&self, n: i64) {
        self.parallel_scan_min_rows
            .store(n.max(0), std::sync::atomic::Ordering::Release);
    }

    /// `true` when `PRAGMA temp_store = 2` (MEMORY): ephemeral
    /// structures must stay in RAM instead of spilling to temp files.
    pub fn temp_store_memory(&self) -> bool {
        self.temp_store.load(std::sync::atomic::Ordering::Acquire) == 2
    }

    /// Current `PRAGMA temp_store` value (0/1/2).
    pub fn temp_store(&self) -> i64 {
        self.temp_store.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Set `PRAGMA temp_store` (clamped to SQLite's 0–2 range).
    pub fn set_temp_store(&self, v: i64) {
        let v = v.clamp(0, 2);
        self.temp_store
            .store(v, std::sync::atomic::Ordering::Release);
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache.read().len() * self.page_size() as usize
    }

    pub fn cache_size(&self) -> usize {
        self.cache.read().len()
    }

    pub fn cache_capacity(&self) -> usize {
        self.cache_capacity.load(Ordering::Relaxed)
    }

    /// `PRAGMA cache_size`: positive = page count, negative = KiB
    /// (SQLite semantics; -2000 = 2000 KiB). Applied at runtime: the
    /// NEXT cache insert's eviction pass trims an over-capacity cache.
    /// In-memory databases ignore it (cache-is-the-store).
    pub fn set_cache_capacity(&self, pages: usize) {
        self.cache_capacity.store(pages.max(1), Ordering::Relaxed);
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
    ///
    /// Returns the number of payload bytes appended to `out` (callers
    /// that need length validation — e.g. the full-payload reassembly —
    /// compare it against the requested range).
    pub(crate) fn gather_overflow_chain_range(
        &self,
        first: PageId,
        local_len: usize,
        start: usize,
        end: usize,
        out: &mut Vec<u8>,
    ) -> Result<usize> {
        if start >= end || first == 0 {
            return Ok(0);
        }
        let mut appended = 0usize;
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
                    return Ok(appended);
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
                                appended += to - from;
                            }
                        }
                        if next == 0 {
                            return Ok(appended);
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
                            appended += to - from;
                        }
                    }
                    (next, take)
                };
                if next == 0 {
                    return Ok(appended);
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
