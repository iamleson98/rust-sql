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
use crate::storage::page::{
    FileHeader, Page, PageId, DB_HEADER_SIZE, DEFAULT_PAGE_SIZE, FIRST_PAGE_ID,
};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

// On Unix, use positioned I/O (pread/pwrite) so multiple threads can read
// and write the file concurrently without serializing on the file offset.
#[cfg(unix)]
use std::os::unix::fs::FileExt;

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
        Self { slots: Vec::new(), overflow: PageCacheMap::default(), count: 0 }
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

pub struct Pager {
    file: File,
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
    /// Whether FOREIGN KEY constraints are enforced (PRAGMA foreign_keys).
    /// Lives on the pager so both Database (api.rs) and the executor's
    /// static statement dispatcher can reach it through a shared &Pager.
    foreign_keys_enabled: AtomicBool,
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
}

impl Pager {
    /// Open or create a database file at the given path.
    pub fn open<P: AsRef<Path>>(path: P, cache_capacity: usize) -> Result<Self> {
        let path = path.ref_to_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let pager = Self {
            file,
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
            skip_fsync: AtomicBool::new(false),
            foreign_keys_enabled: AtomicBool::new(false),
            lazy_writeback: AtomicBool::new(false),
            last_noted_dirty: std::sync::atomic::AtomicU32::new(u32::MAX),
            dirty_count_approx: AtomicUsize::new(0),
            dirty_pages: Mutex::new(PageIdSet::default()),
        };

        let file_size = pager.file.metadata()?.len();
        if file_size == 0 {
            pager.is_new.store(true, Ordering::Release);
            pager.initialize_new_db()?;
        } else {
            pager.read_header()?;
        }
        Ok(pager)
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
        self.file.sync_all()?;
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
        if FileHeader::magic(&header) != Some(&crate::storage::page::DB_MAGIC) {
            // Distinguish "not a rustqlite file" from "old format version"
            // so users get an actionable message.
            let m = FileHeader::magic(&header).map(|m| String::from_utf8_lossy(m).into_owned()).unwrap_or_default();
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
        let actual_size = self.file.metadata()?.len();
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

    pub fn n_pages(&self) -> u32 {
        self.n_pages.load(Ordering::Acquire)
    }

    /// Number of pages currently on the freelist (available for reuse by
    /// `allocate_page` without growing the file).
    pub fn freelist_count(&self) -> u32 {
        self.freelist_count.load(Ordering::Acquire)
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
        let new_cookie = self.schema_cookie.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |x| Some(x.wrapping_add(1)),
        ).unwrap_or_else(|x| x);
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
                return Ok(page_ref);
            }
        }

        // Slow path: cache miss — take write lock, double-check, then read from disk.
        let page_ref = {
            let mut cache = self.cache.write();
            // Double-check: another thread may have inserted while we waited.
            if let Some(page_ref) = cache.get(id).cloned() {
                return Ok(page_ref);
            }
            let psz = self.page_size();
            let mut page = Page::new(id, psz);
            let offset = id as u64 * psz as u64;
            let n = self.read_file_at(offset, &mut page.data)?;
            if n != psz as usize {
                return Err(Error::corruption(format!(
                    "short read on page {}: {} of {} bytes",
                    id, n, psz
                )));
            }
            let page_ref = Arc::new(Mutex::new(page));
            self.maybe_evict_locked(&mut cache);
            cache.insert(id, page_ref.clone());
            self.lru.lock().push_back(id);
            page_ref
        };
        Ok(page_ref)
    }

    /// Allocate a new page. Uses the freelist first, then extends the file.
    pub fn allocate_page(&self) -> Result<PageId> {
        let current_free_count = self.freelist_count.load(Ordering::Acquire);
        if current_free_count > 0 {
            // Pop a page from the freelist.
            let head = self.freelist_head.load(Ordering::Acquire);
            let page = self.get_page(head)?;
            let next = {
                let borrowed = page.lock();
                u32::from_le_bytes(borrowed.data[..4].try_into().unwrap())
            };
            let freed = head;
            self.freelist_head.store(next, Ordering::Release);
            // Use fetch_sub to safely decrement without overflow.
            self.freelist_count.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |x| if x > 0 { Some(x - 1) } else { None },
            ).map_err(|_| Error::corruption("freelist underflow"))?;

            // Clear the page before reuse.
            {
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

    /// Mark a page as freed (push it onto the freelist).
    /// The page is added to the head of the freelist.
    pub fn free_page(&self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(Error::InvalidArgument("cannot free page 0".into()));
        }
        let page = self.get_page(id)?;
        let prev_head = self.freelist_head.load(Ordering::Acquire);
        {
            let mut borrowed = page.lock();
            borrowed.data.fill(0);
            borrowed.data[..4].copy_from_slice(&prev_head.to_le_bytes());
            borrowed.dirty = true;
        }
        self.freelist_head.store(id, Ordering::Release);
        self.freelist_count.fetch_add(1, Ordering::AcqRel);
        self.note_write();
        self.note_dirty(id);
        Ok(())
    }

    /// Flush all dirty pages to disk and sync.
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

        let n_pages_val = self.n_pages.load(Ordering::Acquire);
        let freelist_head_val = self.freelist_head.load(Ordering::Acquire);
        let freelist_count_val = self.freelist_count.load(Ordering::Acquire);
        let schema_cookie_val = self.schema_cookie.load(Ordering::Acquire);

        // Update file header on page 0
        let page0_in_cache = self.cache.read().contains_key(0);
        let psz = self.page_size();
        if page0_in_cache {
            let page0 = self.cache.read().get(0).cloned();
            if let Some(page0) = page0 {
                let mut borrowed = page0.lock();
                FileHeader::write(
                    &mut borrowed.data,
                    psz,
                    n_pages_val,
                    schema_cookie_val,
                );
                borrowed.data[20..24].copy_from_slice(&freelist_head_val.to_le_bytes());
                borrowed.data[24..28].copy_from_slice(&freelist_count_val.to_le_bytes());
                borrowed.dirty = true;
                // Mark page 0 as dirty in the dirty_pages set so it gets flushed below.
                drop(borrowed);
                self.dirty_pages.lock().insert(0);
            }
        } else {
            // Page 0 not in cache — read, modify, write directly
            let mut header = vec![0u8; psz as usize];
            self.read_file_at(0, &mut header)?;
            FileHeader::write(&mut header, psz, n_pages_val, schema_cookie_val);
            header[20..24].copy_from_slice(&freelist_head_val.to_le_bytes());
            header[24..28].copy_from_slice(&freelist_count_val.to_le_bytes());
            self.write_file_at(0, &header)?;
        }

        // Flush dirty pages — use the dirty_pages set so this is O(dirty_count),
        // not O(cache_size). This is the key optimization: a 10k-page cache with
        // only 1-2 dirty pages per statement used to scan 10k page-locks per
        // flush; now we iterate only the dirty set (~1-2 entries).
        let dirty_ids: Vec<PageId> = {
            let mut set = self.dirty_pages.lock();
            set.drain().collect::<Vec<_>>()
        };

        for id in dirty_ids {
            // Use a single cache read lock to look up the page; clone the Arc
            // and release the lock before doing I/O.
            let page_ref = self.cache.read().get(id).cloned();
            if let Some(page_ref) = page_ref {
                let mut borrowed = page_ref.lock();
                if borrowed.dirty {
                    let offset = id as u64 * psz as u64;
                    self.write_file_at(offset, &borrowed.data)?;
                    borrowed.dirty = false;
                }
            }
        }
        // If skip_fsync is set (in-memory mode), sync_all is a no-op on
        // tmpfs anyway, but the syscall round-trip still costs ~5-50 µs.
        // Skip the entire call to make in-memory mode match SQLite's `:memory:`
        // performance.
        if !self.skip_fsync.load(Ordering::Acquire) {
            self.file.sync_all()?;
        }
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
        // The set was drained — the last-noted hint is stale (that page is
        // no longer in the set). Reset so future note_dirty calls don't
        // skip a needed insert.
        self.last_noted_dirty.store(u32::MAX, Ordering::Release);

        // 2. Restore mutable metadata.
        self.n_pages.store(snap.n_pages, Ordering::Release);
        self.freelist_head.store(snap.freelist_head, Ordering::Release);
        self.freelist_count.store(snap.freelist_count, Ordering::Release);
        self.schema_cookie.store(snap.schema_cookie, Ordering::Release);

        // 3. Truncate the file back if pages were allocated during the txn.
        let target_size = snap.n_pages as u64 * self.page_size() as u64;
        let current_size = self.file.metadata()?.len();
        if current_size > target_size {
            self.file.set_len(target_size)?;
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
        self.foreign_keys_enabled.store(enabled, std::sync::atomic::Ordering::Release);
    }

    pub fn foreign_keys_enabled(&self) -> bool {
        self.foreign_keys_enabled.load(std::sync::atomic::Ordering::Acquire)
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
    //       read/write without serializing on the file offset. -----

    #[cfg(unix)]
    fn read_file_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.read_at(buf, offset)?)
    }

    #[cfg(unix)]
    fn write_file_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, offset)?;
        Ok(())
    }

    // Non-unix fallback: serialize through seek+read/write.
    #[cfg(not(unix))]
    fn read_file_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.read(buf)?)
    }

    #[cfg(not(unix))]
    fn write_file_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(buf)?;
        Ok(())
    }
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
