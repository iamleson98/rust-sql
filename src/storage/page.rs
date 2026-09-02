//! On-disk page format.
//!
//! The file is divided into fixed-size pages (default 4 KiB, configurable).
//! Each page has a 16-byte header followed by cell content.
//!
//! ```text
//! +--------+--------+--------+--------+
//! | page_type | n_cells | cell_content_offset | right_pointer |  <- 16 bytes
//! +--------+--------+--------+--------+
//! | cell pointer array (u16 × n_cells) |
//! +--------+--------+--------+--------+
//! | ... free space ...                 |
//! +--------+--------+--------+--------+
//! | cell content (variable)            |
//! +--------+--------+--------+--------+
//! ```
//!
//! Pages come in four B+tree flavors:
//! - `LeafTable` — table b-tree leaf, holds (rowid, payload) pairs
//! - `InteriorTABLE` — table b-tree interior, holds (key, child_page) pairs
//! - `LeafIndex` — index b-tree leaf, holds (key, ...) pairs
//! - `InteriorIndex` — index b-tree interior

use crate::error::{Error, Result};
use std::convert::TryInto;

/// Default page size. 4 KiB matches SQLite's default (since 3.12):
/// same page granularity means same on-disk footprint for identical
/// workloads, and benchmarks show it equal-or-faster than 8 KiB on this
/// engine's hot paths (fewer bytes to fault in per leaf touch, better
/// cache locality for the leaf cache).
pub const DEFAULT_PAGE_SIZE: u32 = 4096;

/// Smallest allowed page size.
pub const MIN_PAGE_SIZE: u32 = 512;

/// Largest allowed page size (must be a power of 2).
pub const MAX_PAGE_SIZE: u32 = 65536;

/// Magic header for the database file.
/// On-disk format version. `RSQLDB03`: index order keys for TEXT/BLOB
/// switched from (length-prefix, bytes) to true lexicographic
/// (bytes + NUL terminator) — index range scans and ORDER BY on text of
/// differing lengths were wrong. Bumped to `RSQLDB02` with the compact row
/// codec (size-classed integers, varint lengths, rowid-alias elision) —
/// files written by v1 are rejected with a clear "unsupported format"
/// error instead of silently decoding garbage.
pub const DB_MAGIC: [u8; 8] = *b"RSQLDB03";

/// Page 0 is special: it holds the database header (100 bytes) followed by
/// the first B+tree page (typically the schema table's root).
pub const DB_HEADER_SIZE: u32 = 100;

/// Page type tags stored as the first byte of a B+tree page.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageType {
    /// Interior table B+tree page.
    InteriorTable = 0x05,
    /// Leaf table B+tree page.
    LeafTable = 0x0D,
    /// Interior index B+tree page.
    InteriorIndex = 0x02,
    /// Leaf index B+tree page.
    LeafIndex = 0x0A,
    /// Overflow chain page: `[be_u32 next (0 = end of chain)] + payload
    /// bytes`. Reached from a table-leaf cell whose payload does not fit
    /// in-page; the cell stores a local prefix plus the first overflow
    /// page id (SQLite overflow-chain equivalent).
    Overflow = 0x04,
}

impl PageType {
    pub fn from_byte(b: u8) -> Result<Self> {
        match b {
            0x05 => Ok(PageType::InteriorTable),
            0x0D => Ok(PageType::LeafTable),
            0x04 => Ok(PageType::Overflow),
            0x02 => Ok(PageType::InteriorIndex),
            0x0A => Ok(PageType::LeafIndex),
            _ => Err(Error::corruption(format!(
                "invalid page type byte: {:#x}",
                b
            ))),
        }
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, PageType::LeafTable | PageType::LeafIndex)
    }

    pub fn is_interior(&self) -> bool {
        matches!(self, PageType::InteriorTable | PageType::InteriorIndex)
    }

    pub fn is_table(&self) -> bool {
        matches!(self, PageType::InteriorTable | PageType::LeafTable)
    }

    pub fn is_index(&self) -> bool {
        matches!(self, PageType::InteriorIndex | PageType::LeafIndex)
    }
}

/// B+tree page header (8 bytes, but we round to 16 with the cell content offset).
///
/// Layout (after the page-type byte):
/// ```text
/// offset 0: page_type (1 byte)
/// offset 1: reserved (3 bytes, for alignment)
/// offset 4: n_cells (u16)
/// offset 6: cell_content_start (u16, 0 means 65536)
/// offset 8: right_most_pointer (u32, only for interior pages; 0 for leaf)
/// ```
pub const PAGE_HEADER_SIZE: u32 = 12;

/// A page number. Page 0 is reserved for the file header.
/// Pages are 1-indexed to match SQLite's convention.
pub type PageId = u32;

/// The first valid B+tree page number.
pub const FIRST_PAGE_ID: PageId = 1;

/// In-memory representation of a raw page.
///
/// The `data` buffer is exactly `page_size` bytes long. The `dirty` flag
/// tells the pager whether to flush this page to disk.
#[derive(Debug)]
pub struct Page {
    pub id: PageId,
    pub data: Vec<u8>,
    pub dirty: bool,
}

impl Page {
    pub fn new(id: PageId, page_size: u32) -> Self {
        Self {
            id,
            data: vec![0u8; page_size as usize],
            dirty: false,
        }
    }

    pub fn page_size(&self) -> u32 {
        self.data.len() as u32
    }

    /// Bounds-checked cell-content slice. `ptr` is a byte offset read from
    /// a page's cell-pointer array — and the page may be CORRUPTED (fuzzed
    /// file, torn write), so the offset may point outside the page. Direct
    /// `data[ptr..]` slicing panics on that; this returns SQLITE_CORRUPT
    /// instead. Every read of cell content from a page that was not just
    /// written by this engine must go through here.
    pub fn cell_slice_checked(&self, ptr: usize) -> Result<&[u8]> {
        if ptr >= self.data.len() {
            return Err(crate::error::Error::corruption(format!(
                "cell pointer {} out of range for {}-byte page",
                ptr,
                self.data.len()
            )));
        }
        Ok(&self.data[ptr..])
    }

    /// Bounds-checked cell slice by cell INDEX (the common shape:
    /// `cell_slice(i)` reads cell i's content). Combines the
    /// cell-pointer-array read with the range check so callers cannot
    /// forget one of the two.
    pub fn cell_slice(&self, idx: u16) -> Result<&[u8]> {
        let ptr = self.cell_pointer(idx) as usize;
        self.cell_slice_checked(ptr)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Read the B+tree page type. Page 0 is special — it has a 100-byte
    /// file header before the B+tree header, so we offset by `DB_HEADER_SIZE`.
    pub fn page_type(&self) -> Result<PageType> {
        let offset = self.header_offset();
        PageType::from_byte(self.data[offset as usize])
    }

    pub fn set_page_type(&mut self, pt: PageType) {
        let offset = self.header_offset() as usize;
        self.data[offset] = pt as u8;
        self.dirty = true;
    }

    /// Number of cells in this page.
    pub fn n_cells(&self) -> u16 {
        let offset = self.header_offset() as usize + 4;
        u16::from_be_bytes(self.data[offset..offset + 2].try_into().unwrap())
    }

    pub fn set_n_cells(&mut self, n: u16) {
        let offset = self.header_offset() as usize + 4;
        self.data[offset..offset + 2].copy_from_slice(&n.to_be_bytes());
        self.dirty = true;
    }

    /// Offset within the page where cell content area starts.
    /// 0 is interpreted as `page_size` (i.e. empty page).
    pub fn cell_content_start(&self) -> u32 {
        let offset = self.header_offset() as usize + 6;
        let v = u16::from_be_bytes(self.data[offset..offset + 2].try_into().unwrap());
        if v == 0 {
            self.page_size()
        } else {
            v as u32
        }
    }

    pub fn set_cell_content_start(&mut self, offset: u32) {
        let v = if offset >= self.page_size() {
            0u16
        } else {
            offset as u16
        };
        let off = self.header_offset() as usize + 6;
        self.data[off..off + 2].copy_from_slice(&v.to_be_bytes());
        self.dirty = true;
    }

    /// Right-most child pointer for interior pages (0 for leaf).
    pub fn right_most_pointer(&self) -> u32 {
        let offset = self.header_offset() as usize + 8;
        u32::from_be_bytes(self.data[offset..offset + 4].try_into().unwrap())
    }

    pub fn set_right_most_pointer(&mut self, p: u32) {
        let offset = self.header_offset() as usize + 8;
        self.data[offset..offset + 4].copy_from_slice(&p.to_be_bytes());
        self.dirty = true;
    }

    /// Cell pointer at the given index (0-based).
    pub fn cell_pointer(&self, idx: u16) -> u16 {
        let base = self.header_offset() as usize + PAGE_HEADER_SIZE as usize;
        let off = base + idx as usize * 2;
        u16::from_be_bytes(self.data[off..off + 2].try_into().unwrap())
    }

    pub fn set_cell_pointer(&mut self, idx: u16, ptr: u16) {
        let base = self.header_offset() as usize + PAGE_HEADER_SIZE as usize;
        let off = base + idx as usize * 2;
        self.data[off..off + 2].copy_from_slice(&ptr.to_be_bytes());
        self.dirty = true;
    }

    /// Free space (in bytes) between the cell pointer array and the cell content area.
    pub fn free_space(&self) -> u32 {
        let n = self.n_cells() as u32;
        let ptr_array_end = self.header_offset() + PAGE_HEADER_SIZE + n * 2;
        self.cell_content_start().saturating_sub(ptr_array_end)
    }

    /// Header offset: page 0 has the 100-byte file header before its B+tree header.
    fn header_offset(&self) -> u32 {
        if self.id == 0 {
            DB_HEADER_SIZE
        } else {
            0
        }
    }

    /// Total usable space (excluding the header and the reserved bytes at the end).
    pub fn usable_size(&self) -> u32 {
        self.page_size()
    }

    /// Initialize an empty overflow chain page. Layout: the standard
    /// 12-byte page header (type byte = Overflow, n_cells = 0,
    /// cell_content_start = 12), then `[be_u32 next (0 = end of chain)]`
    /// at offset 12, with payload bytes from offset 16.
    pub fn init_overflow(&mut self) {
        self.set_page_type(PageType::Overflow);
        self.set_n_cells(0);
        self.set_cell_content_start(12);
        self.set_right_most_pointer(0);
        // next = 0 (end of chain)
        let off = self.header_offset() as usize + 12;
        self.data[off..off + 4].copy_from_slice(&0u32.to_be_bytes());
        self.dirty = true;
    }

    /// Overflow chain: the next page id (0 = end of chain).
    pub fn overflow_next(&self) -> PageId {
        let off = self.header_offset() as usize + 12;
        u32::from_be_bytes(self.data[off..off + 4].try_into().unwrap())
    }

    /// Overflow chain: set the next page id.
    pub fn set_overflow_next(&mut self, next: PageId) {
        let off = self.header_offset() as usize + 12;
        self.data[off..off + 4].copy_from_slice(&next.to_be_bytes());
        self.dirty = true;
    }

    /// Overflow chain: data region (after the 12-byte page header + 4-byte
    /// next pointer).
    pub fn overflow_data(&self) -> &[u8] {
        let off = self.header_offset() as usize + 16;
        &self.data[off..]
    }

    /// Overflow chain: mutable data region.
    pub fn overflow_data_mut(&mut self) -> &mut [u8] {
        let off = self.header_offset() as usize + 16;
        &mut self.data[off..]
    }

    /// Initialize an empty leaf table page.
    pub fn init_leaf_table(&mut self) {
        self.set_page_type(PageType::LeafTable);
        self.set_n_cells(0);
        self.set_cell_content_start(0); // 0 means page_size
        self.set_right_most_pointer(0);
        self.dirty = true;
    }

    /// Initialize an empty interior table page.
    pub fn init_interior_table(&mut self) {
        self.set_page_type(PageType::InteriorTable);
        self.set_n_cells(0);
        self.set_cell_content_start(0);
        self.set_right_most_pointer(0);
        self.dirty = true;
    }

    /// Initialize an empty leaf index page.
    pub fn init_leaf_index(&mut self) {
        self.set_page_type(PageType::LeafIndex);
        self.set_n_cells(0);
        self.set_cell_content_start(0);
        self.set_right_most_pointer(0);
        self.dirty = true;
    }

    /// Initialize an empty interior index page.
    pub fn init_interior_index(&mut self) {
        self.set_page_type(PageType::InteriorIndex);
        self.set_n_cells(0);
        self.set_cell_content_start(0);
        self.set_right_most_pointer(0);
        self.dirty = true;
    }
}

/// The 100-byte file header (page 0 only).
///
/// Layout:
/// ```text
/// 0..8    magic: "RSQLDB01"
/// 8..12   page_size (u32 LE)
/// 12..16  file change counter (u32 LE)
/// 16..20  database size in pages (u32 LE)
/// 20..24  first freelist page (u32 LE)
/// 24..28  number of freelist pages (u32 LE)
/// 28..32  schema cookie (u32 LE)
/// 32..36  schema format version (u32 LE, currently 1)
/// 36..40  default cache size hint (u32 LE)
/// 40..48  largest root b-tree page (used for vacuum, u64 LE)
/// 48..56  text encoding (1=UTF8, always), padded
/// 56..60  user version (u32 LE)
/// 60..64  incremental vacuum mode (u32 LE, 0=off)
/// 64..68  application id (u32 LE)
/// 68..92  reserved (zeros)
/// 92..96  version-valid-for (u32 LE)
/// 96..100  SQLite version magic (we use our own version, e.g. 1)
/// ```
pub struct FileHeader;

impl FileHeader {
    pub fn write(buf: &mut [u8], page_size: u32, db_size_pages: u32, schema_cookie: u32) {
        buf[..8].copy_from_slice(&DB_MAGIC);
        buf[8..12].copy_from_slice(&page_size.to_le_bytes());
        buf[12..16].copy_from_slice(&1u32.to_le_bytes()); // change counter
        buf[16..20].copy_from_slice(&db_size_pages.to_le_bytes());
        buf[20..24].copy_from_slice(&0u32.to_le_bytes()); // freelist head
        buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // freelist count
        buf[28..32].copy_from_slice(&schema_cookie.to_le_bytes());
        buf[32..36].copy_from_slice(&1u32.to_le_bytes()); // schema format v1
        buf[36..40].copy_from_slice(&(2048u32).to_le_bytes()); // cache hint
        buf[40..48].copy_from_slice(&0u64.to_le_bytes()); // largest root page
        buf[48..52].copy_from_slice(&1u32.to_le_bytes()); // encoding=UTF8
                                                          // remainder is zeros (already zeroed in fresh page)
    }

    pub fn magic(buf: &[u8]) -> Option<&[u8; 8]> {
        if buf.len() < 8 {
            return None;
        }
        buf.get(..8).and_then(|s| s.try_into().ok())
    }

    pub fn page_size(buf: &[u8]) -> Result<u32> {
        if buf.len() < 12 {
            return Err(Error::corruption("file too small for header"));
        }
        Ok(u32::from_le_bytes(buf[8..12].try_into().unwrap()))
    }

    pub fn db_size_pages(buf: &[u8]) -> u32 {
        u32::from_le_bytes(buf[16..20].try_into().unwrap())
    }

    pub fn schema_cookie(buf: &[u8]) -> u32 {
        u32::from_le_bytes(buf[28..32].try_into().unwrap())
    }

    pub fn set_schema_cookie(buf: &mut [u8], cookie: u32) {
        buf[28..32].copy_from_slice(&cookie.to_le_bytes());
        buf[12..16].copy_from_slice(&cookie.wrapping_add(1).to_le_bytes()); // bump change counter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_init_leaf_table() {
        let mut p = Page::new(1, DEFAULT_PAGE_SIZE);
        p.init_leaf_table();
        assert_eq!(p.page_type().unwrap(), PageType::LeafTable);
        assert_eq!(p.n_cells(), 0);
        assert_eq!(p.cell_content_start(), DEFAULT_PAGE_SIZE);
        assert_eq!(p.right_most_pointer(), 0);
    }

    #[test]
    fn page_header_roundtrip() {
        let mut p = Page::new(1, DEFAULT_PAGE_SIZE);
        p.init_interior_table();
        p.set_n_cells(3);
        p.set_cell_content_start(2048);
        p.set_right_most_pointer(42);

        let p2 = Page {
            id: 1,
            data: p.data.clone(),
            dirty: false,
        };
        assert_eq!(p2.page_type().unwrap(), PageType::InteriorTable);
        assert_eq!(p2.n_cells(), 3);
        assert_eq!(p2.cell_content_start(), 2048);
        assert_eq!(p2.right_most_pointer(), 42);
    }

    #[test]
    fn page_zero_has_header_offset() {
        let mut p = Page::new(0, DEFAULT_PAGE_SIZE);
        // Page 0 starts with the file header (100 bytes), then the B+tree header.
        assert_eq!(p.header_offset(), DB_HEADER_SIZE);
        // We can still initialize page 0 as a leaf table page (the schema root).
        p.init_leaf_table();
        assert_eq!(p.data[DB_HEADER_SIZE as usize], PageType::LeafTable as u8);
    }

    #[test]
    fn file_header_roundtrip() {
        let mut buf = vec![0u8; 100];
        FileHeader::write(&mut buf, 4096, 10, 7);
        assert_eq!(FileHeader::magic(&buf).unwrap(), &DB_MAGIC);
        assert_eq!(FileHeader::page_size(&buf).unwrap(), 4096);
        assert_eq!(FileHeader::db_size_pages(&buf), 10);
        assert_eq!(FileHeader::schema_cookie(&buf), 7);
    }
}
