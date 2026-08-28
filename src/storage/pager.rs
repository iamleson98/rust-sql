//! Pager: the layer between the B+tree and the raw file.
//!
//! Responsibilities:
//! - Read/write fixed-size pages from the database file
//! - Cache pages in memory (LRU eviction)
//! - Allocate new pages (and maintain a freelist)
//! - Coordinate with the WAL for durability
//! - Snapshot/restore for transaction ROLLBACK
//!
//! Pages are returned via `PageRef` (an `Rc<RefCell<Page>>`) so that the
//! B+tree can hold multiple references to the same page during splits and
//! merges without copying.

use crate::error::{Error, Result};
use crate::storage::page::{
    FileHeader, Page, PageId, DB_HEADER_SIZE, DEFAULT_PAGE_SIZE, FIRST_PAGE_ID,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A shared mutable reference to a page.
pub type PageRef = Rc<RefCell<Page>>;

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
            n_pages: pager.n_pages,
            freelist_head: pager.freelist_head,
            freelist_count: pager.freelist_count,
            schema_cookie: pager.schema_cookie,
        }
    }
}

/// LRU cache of pages. We use a HashMap for O(1) lookup and a doubly-linked
/// list (via `VecDeque` + indices) for LRU ordering.
///
/// For simplicity, we use a `LinkedHashMap`-style structure built on top of
/// `HashMap` + a manual ordering list. This is fast enough for typical
/// workloads (cache sizes of a few thousand pages).
pub struct Pager {
    file: File,
    path: PathBuf,
    page_size: u32,
    /// Total number of pages in the file (cached, updated on writes).
    n_pages: u32,
    /// Head of the freelist (0 if empty).
    freelist_head: PageId,
    /// Number of pages on the freelist.
    freelist_count: u32,
    /// In-memory cache: page_id → page.
    cache: HashMap<PageId, PageRef>,
    /// LRU ordering: most recently used at the back.
    lru: std::collections::VecDeque<PageId>,
    /// Maximum number of pages to keep in the cache.
    cache_capacity: usize,
    /// Schema cookie, bumped on every schema change.
    schema_cookie: u32,
    /// True if this is a freshly created database (no header yet).
    is_new: bool,
}

impl Pager {
    /// Open or create a database file at the given path.
    pub fn open<P: AsRef<Path>>(path: P, cache_capacity: usize) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let mut pager = Self {
            file,
            path,
            page_size: DEFAULT_PAGE_SIZE,
            n_pages: 0,
            freelist_head: 0,
            freelist_count: 0,
            cache: HashMap::new(),
            lru: std::collections::VecDeque::new(),
            cache_capacity,
            schema_cookie: 0,
            is_new: false,
        };

        let file_size = pager.file.metadata()?.len();
        if file_size == 0 {
            pager.is_new = true;
            pager.initialize_new_db()?;
        } else {
            pager.read_header()?;
        }
        Ok(pager)
    }

    /// Create a fresh database: write page 0 with the file header and an
    /// empty leaf page (the schema table root).
    fn initialize_new_db(&mut self) -> Result<()> {
        let mut page0 = Page::new(0, self.page_size);
        FileHeader::write(
            &mut page0.data,
            self.page_size,
            1, // 1 page total
            0, // schema cookie
        );
        // Page 0 is also the schema table's root (a leaf table page).
        // The header is 100 bytes; the B+tree header starts at offset 100.
        page0.data[DB_HEADER_SIZE as usize] = crate::storage::page::PageType::LeafTable as u8;
        page0.data[DB_HEADER_SIZE as usize + 4..DB_HEADER_SIZE as usize + 6]
            .copy_from_slice(&0u16.to_be_bytes()); // n_cells = 0
        page0.data[DB_HEADER_SIZE as usize + 6..DB_HEADER_SIZE as usize + 8]
            .copy_from_slice(&0u16.to_be_bytes()); // cell_content_start = 0 (= page_size)
        page0.data[DB_HEADER_SIZE as usize + 8..DB_HEADER_SIZE as usize + 12]
            .copy_from_slice(&0u32.to_be_bytes()); // right_pointer = 0
        page0.dirty = true;

        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&page0.data)?;
        self.file.sync_all()?;
        self.n_pages = 1;
        self.schema_cookie = 0;
        self.is_new = false;
        Ok(())
    }

    fn read_header(&mut self) -> Result<()> {
        let mut header = [0u8; 100];
        let n = self.file.read(&mut header)?;
        if n < 100 {
            return Err(Error::corruption(format!(
                "file too small for header: {} bytes",
                n
            )));
        }
        if FileHeader::magic(&header) != Some(&crate::storage::page::DB_MAGIC) {
            return Err(Error::corruption("invalid magic header"));
        }
        self.page_size = FileHeader::page_size(&header)?;
        self.n_pages = FileHeader::db_size_pages(&header);
        self.freelist_head = u32::from_le_bytes(header[20..24].try_into().unwrap());
        self.freelist_count = u32::from_le_bytes(header[24..28].try_into().unwrap());
        self.schema_cookie = FileHeader::schema_cookie(&header);

        // Verify file size matches the claimed page count.
        let actual_size = self.file.metadata()?.len();
        let expected_size = self.n_pages as u64 * self.page_size as u64;
        if actual_size < expected_size {
            return Err(Error::corruption(format!(
                "file size {} < expected {} (n_pages={}, page_size={})",
                actual_size, expected_size, self.n_pages, self.page_size
            )));
        }
        Ok(())
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    pub fn n_pages(&self) -> u32 {
        self.n_pages
    }

    pub fn schema_cookie(&self) -> u32 {
        self.schema_cookie
    }

    pub fn bump_schema_cookie(&mut self) -> Result<()> {
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        let mut header = [0u8; 100];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut header)?;
        FileHeader::set_schema_cookie(&mut header, self.schema_cookie);
        header[16..20].copy_from_slice(&self.n_pages.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        Ok(())
    }

    /// Get a page by ID, reading from disk if not cached.
    pub fn get_page(&mut self, id: PageId) -> Result<PageRef> {
        if id >= self.n_pages && id != 0 {
            return Err(Error::corruption(format!(
                "page {} out of range (n_pages={})",
                id, self.n_pages
            )));
        }

        // Check cache
        let cached = self.cache.get(&id).cloned();
        if let Some(page_ref) = cached {
            self.touch_lru(id);
            return Ok(page_ref);
        }

        // Read from disk
        let mut page = Page::new(id, self.page_size);
        let offset = id as u64 * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut page.data)?;

        let page_ref = Rc::new(RefCell::new(page));
        self.maybe_evict();
        self.cache.insert(id, page_ref.clone());
        self.lru.push_back(id);
        Ok(page_ref)
    }

    /// Allocate a new page. Uses the freelist first, then extends the file.
    pub fn allocate_page(&mut self) -> Result<PageId> {
        if self.freelist_count > 0 {
            // Pop a page from the freelist
            let page = self.get_page(self.freelist_head)?;
            let borrowed = page.borrow();
            // First 4 bytes of a freelist page point to the next free page.
            let next = u32::from_le_bytes(borrowed.data[..4].try_into().unwrap());
            drop(borrowed);
            let freed = self.freelist_head;
            self.freelist_head = next;
            self.freelist_count -= 1;

            // Clear the page before reuse
            let mut borrowed = page.borrow_mut();
            borrowed.data.fill(0);
            borrowed.dirty = true;
            drop(borrowed);
            Ok(freed)
        } else {
            // Extend the file
            let id = self.n_pages;
            let mut page = Page::new(id, self.page_size);
            page.dirty = true;
            let page_ref = Rc::new(RefCell::new(page));
            self.maybe_evict();
            self.cache.insert(id, page_ref);
            self.lru.push_back(id);
            self.n_pages += 1;
            Ok(id)
        }
    }

    /// Mark a page as freed (push it onto the freelist).
    /// The page is added to the head of the freelist.
    pub fn free_page(&mut self, id: PageId) -> Result<()> {
        if id == 0 {
            return Err(Error::InvalidArgument("cannot free page 0".into()));
        }
        let page = self.get_page(id)?;
        let mut borrowed = page.borrow_mut();
        borrowed.data.fill(0);
        // Write the previous freelist head into the first 4 bytes.
        borrowed.data[..4].copy_from_slice(&self.freelist_head.to_le_bytes());
        borrowed.dirty = true;
        drop(borrowed);
        self.freelist_head = id;
        self.freelist_count += 1;
        Ok(())
    }

    /// Flush all dirty pages to disk and sync.
    pub fn flush(&mut self) -> Result<()> {
        // Fast path: if no dirty pages, skip everything (including sync_all).
        // This makes `flush()` after a query-only workload a no-op, which
        // matters when `Database::query` calls flush before reads in
        // deferred_flush mode — without this, every SELECT would pay an
        // fsync for no reason.
        let has_dirty = self.cache.values().any(|p| p.borrow().dirty);
        if !has_dirty {
            return Ok(());
        }
        // Update file header on page 0
        if let Some(page0) = self.cache.get(&0) {
            let mut borrowed = page0.borrow_mut();
            FileHeader::write(
                &mut borrowed.data,
                self.page_size,
                self.n_pages,
                self.schema_cookie,
            );
            // Also persist freelist info
            borrowed.data[20..24].copy_from_slice(&self.freelist_head.to_le_bytes());
            borrowed.data[24..28].copy_from_slice(&self.freelist_count.to_le_bytes());
            borrowed.dirty = true;
        } else {
            // Page 0 not in cache — read, modify, write directly
            let mut header = vec![0u8; self.page_size as usize];
            self.file.seek(SeekFrom::Start(0))?;
            self.file.read_exact(&mut header)?;
            FileHeader::write(&mut header, self.page_size, self.n_pages, self.schema_cookie);
            header[20..24].copy_from_slice(&self.freelist_head.to_le_bytes());
            header[24..28].copy_from_slice(&self.freelist_count.to_le_bytes());
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&header)?;
        }

        // Flush dirty pages in cache
        let dirty_ids: Vec<PageId> = self
            .cache
            .iter()
            .filter(|(_, p)| p.borrow().dirty)
            .map(|(id, _)| *id)
            .collect();

        for id in dirty_ids {
            let page = self.cache.get(&id).unwrap().clone();
            let borrowed = page.borrow();
            let offset = id as u64 * self.page_size as u64;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(&borrowed.data)?;
            drop(borrowed);
            page.borrow_mut().dirty = false;
        }
        self.file.sync_all()?;
        Ok(())
    }

    /// Rollback to the state captured by `PagerSnapshot::capture` at BEGIN.
    ///
    /// This discards all in-memory dirty pages (their contents were never
    /// written to disk during the transaction — see `ExecContext::in_transaction`
    /// guard), restores the pager's mutable metadata to the pre-BEGIN values,
    /// and truncates the file back to `n_pages` if the transaction allocated
    /// new pages.
    pub fn rollback_to(&mut self, snap: &PagerSnapshot) -> Result<()> {
        // 1. Drop the entire cache — every cached page may have been modified
        //    during the transaction, and we have no per-page dirty tracking.
        //    Next reads will repopulate the cache from disk, which still
        //    holds the pre-BEGIN state (because no flush occurred during txn).
        self.cache.clear();
        self.lru.clear();

        // 2. Restore mutable metadata.
        self.n_pages = snap.n_pages;
        self.freelist_head = snap.freelist_head;
        self.freelist_count = snap.freelist_count;
        self.schema_cookie = snap.schema_cookie;

        // 3. Truncate the file back if pages were allocated during the txn.
        //    We don't strictly need to do this for correctness (the truncated
        //    tail is now beyond n_pages and won't be read), but it keeps the
        //    file size honest and prevents unbounded growth on repeated
        //    begin/insert/rollback cycles.
        let target_size = self.n_pages as u64 * self.page_size as u64;
        let current_size = self.file.metadata()?.len();
        if current_size > target_size {
            self.file.set_len(target_size)?;
        }

        Ok(())
    }

    /// Take a snapshot of the pager's mutable state, for use with ROLLBACK.
    pub fn snapshot(&self) -> PagerSnapshot {
        PagerSnapshot::capture(self)
    }

    /// Evict pages from the cache until we're under capacity.
    fn maybe_evict(&mut self) {
        while self.cache.len() >= self.cache_capacity {
            // Find the least-recently-used page that is not dirty.
            let evict_id = match self.lru.front().copied() {
                Some(id) => id,
                None => break,
            };
            let should_evict = match self.cache.get(&evict_id) {
                Some(p) => !p.borrow().dirty,
                None => true,
            };
            if should_evict {
                self.lru.pop_front();
                self.cache.remove(&evict_id);
            } else {
                // Move dirty page to the back and try the next one.
                self.lru.pop_front();
                self.lru.push_back(evict_id);
            }
        }
    }

    fn touch_lru(&mut self, id: PageId) {
        if let Some(pos) = self.lru.iter().position(|x| *x == id) {
            self.lru.remove(pos);
            self.lru.push_back(id);
        }
    }

    /// Total bytes used by the cache (for instrumentation).
    pub fn cache_bytes(&self) -> usize {
        self.cache.len() * self.page_size as usize
    }

    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    pub fn cache_capacity(&self) -> usize {
        self.cache_capacity
    }

    /// Count the number of dirty pages currently in the cache. Used by
    /// `Database::execute` to decide whether to force a deferred flush
    /// when `deferred_flush` mode is enabled.
    pub fn dirty_page_count(&self) -> usize {
        self.cache.values().filter(|p| p.borrow().dirty).count()
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
            let mut pager = Pager::open(tmp.path(), 64).unwrap();
            let id = pager.allocate_page().unwrap();
            assert_eq!(id, 1);
            let page = pager.get_page(id).unwrap();
            page.borrow_mut().data[0] = 42;
            page.borrow_mut().mark_dirty();
            pager.flush().unwrap();
        }
        // Reopen and verify
        let mut pager = Pager::open(tmp.path(), 64).unwrap();
        assert_eq!(pager.n_pages(), 2);
        let page = pager.get_page(1).unwrap();
        assert_eq!(page.borrow().data[0], 42);
    }

    #[test]
    fn freelist_recycles_pages() {
        let tmp = NamedTempFile::new().unwrap();
        let mut pager = Pager::open(tmp.path(), 64).unwrap();
        let p1 = pager.allocate_page().unwrap();
        let p2 = pager.allocate_page().unwrap();
        let p3 = pager.allocate_page().unwrap();
        assert_eq!((p1, p2, p3), (1, 2, 3));
        pager.free_page(p2).unwrap();
        let reused = pager.allocate_page().unwrap();
        assert_eq!(reused, p2);
    }
}
