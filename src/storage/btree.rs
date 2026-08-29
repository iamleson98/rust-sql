//! B+tree implementation.
//!
//! Two flavors of trees are supported:
//! - **Table B+tree**: keys are `i64` rowids, payloads are encoded rows.
//!   Leaf cells store `(rowid, payload)`. Interior cells store `(child_page, key)`.
//! - **Index B+tree**: keys are encoded key values, payloads are rowids.
//!   Leaf cells store `(key, rowid)`. Interior cells store `(child_page, key)`.
//!
//! The implementation is intentionally simple (no prefix compression, no
//! suffix truncation), trading some space for clarity. The cell format is
//! varint-encoded to keep small payloads compact.

use crate::error::{Error, Result};
use crate::storage::page::{PageId, PageType, PAGE_HEADER_SIZE};
use crate::storage::pager::Pager;

/// A varint encoder/decoder compatible with SQLite (1-9 bytes, big-endian).
pub mod varint {
    /// Encode an unsigned 64-bit integer as a SQLite-style varint.
    /// 1-9 bytes; the 9th byte (if present) uses all 8 bits.
    pub fn encode(v: u64, out: &mut [u8]) -> usize {
        if v == 0 {
            out[0] = 0;
            return 1;
        }
        if v <= 0x7F {
            out[0] = v as u8;
            return 1;
        }
        // SQLite layout: first 8 bytes carry 7 bits each (high bits first),
        // 9th byte (if present) carries the low 8 bits.
        // Total capacity: 7*8 + 8 = 64 bits.
        //
        // We compute the number of 7-bit groups needed for the high bits,
        // then optionally a 9th byte for the remaining low 8 bits.
        if v <= 0x3FFF {
            // 2 bytes
            out[0] = ((v >> 7) as u8) | 0x80;
            out[1] = (v & 0x7F) as u8;
            return 2;
        }
        // General case: figure out how many 7-bit groups we need.
        // We want to find smallest n (1..=8) such that v fits in n*7 bits,
        // unless v > (1 << 56) - 1, in which case we need 9 bytes.
        if v < (1u64 << 56) {
            // Fits in 1-8 bytes of 7 bits each.
            // Find smallest n.
            let mut n = 1;
            let mut max = 0x7Fu64;
            while v > max {
                n += 1;
                max = (max << 7) | 0x7F;
            }
            // Emit n bytes, high bits first.
            for i in 0..n {
                let shift = (n - 1 - i) * 7;
                let byte = ((v >> shift) & 0x7F) as u8;
                if i < n - 1 {
                    out[i] = byte | 0x80;
                } else {
                    out[i] = byte;
                }
            }
            n
        } else {
            // 9 bytes: first 8 bytes hold the high 56 bits (7 bits each),
            // 9th byte holds the low 8 bits.
            let high = v >> 8;
            let low = (v & 0xFF) as u8;
            for i in 0..8 {
                let shift = (7 - i) * 7;
                let byte = ((high >> shift) & 0x7F) as u8;
                out[i] = byte | 0x80;
            }
            out[8] = low;
            9
        }
    }

    /// Decode a varint. Returns (value, bytes consumed).
    pub fn decode(buf: &[u8]) -> Option<(u64, usize)> {
        if buf.is_empty() {
            return None;
        }
        let mut v: u64 = 0;
        for i in 0..9 {
            if i >= buf.len() {
                return None;
            }
            let b = buf[i];
            if i == 8 {
                // 9th byte: all 8 bits, no continuation.
                v = (v << 8) | b as u64;
                return Some((v, 9));
            }
            v = (v << 7) | (b & 0x7F) as u64;
            if b & 0x80 == 0 {
                return Some((v, i + 1));
            }
        }
        Some((v, 9))
    }

    /// Encode a signed i64 as a varint (using SQLite's zig-zag-like encoding).
    pub fn encode_signed(v: i64, out: &mut [u8]) -> usize {
        // SQLite uses two's complement, big-endian, but stored as varint
        // with the sign bit in the MSB of the i64. We cast to u64.
        encode(v as u64, out)
    }

    pub fn decode_signed(buf: &[u8]) -> Option<(i64, usize)> {
        decode(buf).map(|(v, n)| (v as i64, n))
    }
}

/// A cell in a B+tree. This is a logical representation; on-disk format
/// is varint-encoded.
#[derive(Clone, Debug)]
pub enum Cell {
    /// Table leaf: (rowid, payload).
    TableLeaf { rowid: i64, payload: Vec<u8> },
    /// Table interior: (left_child_page, key).
    TableInterior { left_child: PageId, key: i64 },
    /// Index leaf: (key, rowid).
    IndexLeaf { key: Vec<u8>, rowid: i64 },
    /// Index interior: (left_child_page, key, rowid).
    IndexInterior {
        left_child: PageId,
        key: Vec<u8>,
        rowid: i64,
    },
}

impl Cell {
    pub fn key(&self) -> i64 {
        match self {
            Cell::TableLeaf { rowid, .. } => *rowid,
            Cell::TableInterior { key, .. } => *key,
            Cell::IndexLeaf { rowid, .. } => *rowid,
            Cell::IndexInterior { rowid, .. } => *rowid,
        }
    }

    /// Left child page of an interior cell (table or index).
    pub fn left_child(&self) -> PageId {
        match self {
            Cell::TableInterior { left_child, .. } => *left_child,
            Cell::IndexInterior { left_child, .. } => *left_child,
            _ => 0,
        }
    }

    /// Encoded key bytes of an index cell (empty for table cells).
    pub fn index_key(&self) -> &[u8] {
        match self {
            Cell::IndexLeaf { key, .. } | Cell::IndexInterior { key, .. } => key,
            _ => &[],
        }
    }

    /// Compare an index cell against a target (key, rowid).
    /// Index pages are sorted by (key bytes, rowid).
    pub fn cmp_index_target(&self, key: &[u8], rowid: i64) -> std::cmp::Ordering {
        self.index_key().cmp(key).then(self.key().cmp(&rowid))
    }

    pub fn encoded_size(&self) -> usize {
        let mut buf = [0u8; 10];
        match self {
            Cell::TableLeaf { rowid, payload } => {
                let k = varint::encode_signed(*rowid, &mut buf);
                let p = varint::encode(payload.len() as u64, &mut buf);
                k + p + payload.len()
            }
            Cell::TableInterior { left_child: _, key } => 4 + varint::encode_signed(*key, &mut buf),
            Cell::IndexLeaf { key, rowid } => {
                // Format: varint(rowid) + varint(key_len) + key
                let r = varint::encode_signed(*rowid, &mut buf);
                let kl = varint::encode(key.len() as u64, &mut buf);
                r + kl + key.len()
            }
            Cell::IndexInterior {
                left_child: _,
                key,
                rowid,
            } => {
                // Format: be_u32(left_child) + varint(rowid) + varint(key_len) + key
                let r = varint::encode_signed(*rowid, &mut buf);
                let kl = varint::encode(key.len() as u64, &mut buf);
                4 + r + kl + key.len()
            }
        }
    }

    /// Encode the cell into a byte buffer.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut buf = [0u8; 10];
        match self {
            Cell::TableLeaf { rowid, payload } => {
                let k = varint::encode_signed(*rowid, &mut buf);
                out.extend_from_slice(&buf[..k]);
                let p = varint::encode(payload.len() as u64, &mut buf);
                out.extend_from_slice(&buf[..p]);
                out.extend_from_slice(payload);
            }
            Cell::TableInterior { left_child, key } => {
                out.extend_from_slice(&left_child.to_be_bytes());
                let k = varint::encode_signed(*key, &mut buf);
                out.extend_from_slice(&buf[..k]);
            }
            Cell::IndexLeaf { key, rowid } => {
                let r = varint::encode_signed(*rowid, &mut buf);
                out.extend_from_slice(&buf[..r]);
                let kl = varint::encode(key.len() as u64, &mut buf);
                out.extend_from_slice(&buf[..kl]);
                out.extend_from_slice(key);
            }
            Cell::IndexInterior {
                left_child,
                key,
                rowid,
            } => {
                out.extend_from_slice(&left_child.to_be_bytes());
                let r = varint::encode_signed(*rowid, &mut buf);
                out.extend_from_slice(&buf[..r]);
                let kl = varint::encode(key.len() as u64, &mut buf);
                out.extend_from_slice(&buf[..kl]);
                out.extend_from_slice(key);
            }
        }
    }

    /// Decode a cell from a byte buffer at a given page type.
    pub fn decode(buf: &[u8], page_type: PageType) -> Result<Self> {
        match page_type {
            PageType::LeafTable => {
                let (rowid, n) = varint::decode_signed(buf)
                    .ok_or_else(|| Error::corruption("truncated leaf rowid"))?;
                let rest = &buf[n..];
                let (plen, m) = varint::decode(rest)
                    .ok_or_else(|| Error::corruption("truncated leaf payload length"))?;
                let rest = &rest[m..];
                if rest.len() < plen as usize {
                    return Err(Error::corruption("truncated leaf payload"));
                }
                Ok(Cell::TableLeaf {
                    rowid,
                    payload: rest[..plen as usize].to_vec(),
                })
            }
            PageType::InteriorTable => {
                if buf.len() < 4 {
                    return Err(Error::corruption("truncated interior child"));
                }
                let left_child = u32::from_be_bytes(buf[..4].try_into().unwrap());
                let (key, _) = varint::decode_signed(&buf[4..])
                    .ok_or_else(|| Error::corruption("truncated interior key"))?;
                Ok(Cell::TableInterior { left_child, key })
            }
            PageType::LeafIndex => {
                let (rowid, n) = varint::decode_signed(buf)
                    .ok_or_else(|| Error::corruption("truncated index leaf rowid"))?;
                let rest = &buf[n..];
                let (key_len, m) = varint::decode(rest)
                    .ok_or_else(|| Error::corruption("truncated index leaf key length"))?;
                let rest = &rest[m..];
                if rest.len() < key_len as usize {
                    return Err(Error::corruption("truncated index leaf key"));
                }
                Ok(Cell::IndexLeaf {
                    key: rest[..key_len as usize].to_vec(),
                    rowid,
                })
            }
            PageType::InteriorIndex => {
                if buf.len() < 4 {
                    return Err(Error::corruption("truncated index interior child"));
                }
                let left_child = u32::from_be_bytes(buf[..4].try_into().unwrap());
                let (rowid, n) = varint::decode_signed(&buf[4..])
                    .ok_or_else(|| Error::corruption("truncated index interior rowid"))?;
                let rest = &buf[4 + n..];
                let (key_len, m) = varint::decode(rest)
                    .ok_or_else(|| Error::corruption("truncated index interior key length"))?;
                let rest = &rest[m..];
                if rest.len() < key_len as usize {
                    return Err(Error::corruption("truncated index interior key"));
                }
                Ok(Cell::IndexInterior {
                    left_child,
                    key: rest[..key_len as usize].to_vec(),
                    rowid,
                })
            }
        }
    }
}

/// Allocation-free view of an index cell (leaf or interior).
///
/// Cell layouts:
/// ```text
/// LeafIndex:     [varint: rowid][varint: key_len][key bytes]
/// InteriorIndex: [be_u32: left_child][varint: rowid][varint: key_len][key bytes]
/// ```
///
/// `Cell::decode` heap-allocates the key `Vec<u8>` for every cell — and the
/// interior-page navigation loops decoded EVERY cell of EVERY page on every
/// descent (a 16 KB interior page holds ~150-200 cells, so a single 3-level
/// index descent allocated ~450 key Vecs). This view borrows the key bytes
/// straight from the page buffer: zero allocation, and it enables binary
/// search (the navigation loops were linear scans).
#[derive(Clone, Copy)]
struct IndexCellView<'a> {
    key: &'a [u8],
    rowid: i64,
    left_child: u32,
}

/// Byte length of an INDEX leaf cell at `buf` (varint rowid + varint key
/// length + key). Non-allocating; returns None on truncation.
fn index_leaf_cell_size(buf: &[u8]) -> Option<usize> {
    let (_, n1) = varint::decode_signed(buf)?;
    let rest = &buf[n1..];
    let (klen, n2) = varint::decode(rest)?;
    Some(n1 + n2 + klen as usize)
}

/// Byte length of a TABLE leaf cell at `buf` (varint rowid + varint payload
/// length + payload). Non-allocating; returns None on truncation.
fn table_leaf_cell_size(buf: &[u8]) -> Option<usize> {
    let (_, n1) = varint::decode_signed(buf)?;
    let rest = &buf[n1..];
    let (plen, n2) = varint::decode(rest)?;
    Some(n1 + n2 + plen as usize)
}

fn decode_index_cell(buf: &[u8], interior: bool) -> Option<IndexCellView<'_>> {
    if interior {
        if buf.len() < 4 {
            return None;
        }
        let left_child = u32::from_be_bytes(buf[..4].try_into().unwrap());
        let (rowid, n) = varint::decode_signed(&buf[4..])?;
        let rest = &buf[4 + n..];
        let (key_len, m) = varint::decode(rest)?;
        let rest = &rest[m..];
        let key_len = key_len as usize;
        if rest.len() < key_len {
            return None;
        }
        Some(IndexCellView { key: &rest[..key_len], rowid, left_child })
    } else {
        let (rowid, n) = varint::decode_signed(buf)?;
        let rest = &buf[n..];
        let (key_len, m) = varint::decode(rest)?;
        let rest = &rest[m..];
        let key_len = key_len as usize;
        if rest.len() < key_len {
            return None;
        }
        Some(IndexCellView { key: &rest[..key_len], rowid, left_child: 0 })
    }
}

/// Read the separator key of a table-interior cell without allocating.
/// Layout: [be_u32: left_child][varint: key].
fn decode_table_interior_key(buf: &[u8]) -> Option<i64> {
    if buf.len() < 4 {
        return None;
    }
    varint::decode_signed(&buf[4..]).map(|(k, _)| k)
}

/// Read the left-child pointer of a table-interior cell.
fn decode_table_interior_child(buf: &[u8]) -> Option<u32> {
    if buf.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes(buf[..4].try_into().unwrap()))
}

/// Binary-search an interior INDEX page for the first cell whose
/// (key, rowid) separator is >= the target (key, rowid). Returns
/// (cell_index, that cell's left_child, n_cells, right_most). When all
/// separators are < target, returns (n, right_most, n, right_most).
fn find_index_child(
    data: &[u8],
    n: u16,
    cell_pointer: impl Fn(u16) -> u16,
    right_most: u32,
    key: &[u8],
    rowid: i64,
) -> (usize, u32) {
    let mut lo: u16 = 0;
    let mut hi: u16 = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let ptr = cell_pointer(mid) as usize;
        let Some(v) = decode_index_cell(&data[ptr..], true) else { break };
        // (v.key, v.rowid) < (key, rowid)  → go right
        let ord = v.key.cmp(key).then(v.rowid.cmp(&rowid));
        if ord == std::cmp::Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo >= n {
        (n as usize, right_most)
    } else {
        let ptr = cell_pointer(lo) as usize;
        match decode_index_cell(&data[ptr..], true) {
            Some(v) => (lo as usize, v.left_child),
            None => (n as usize, right_most),
        }
    }
}

/// Decode just the rowid from a leaf-table cell, without allocating the
/// payload. Used by `lookup_table`'s binary search to avoid O(N) heap
/// allocations per lookup.
///
/// Cell layout (LeafTable):
/// ```text
/// [varint: rowid][varint: payload_len][payload bytes...]
/// ```
/// Returns `(rowid, bytes_consumed)` or `None` on truncation.
fn decode_rowid_only(buf: &[u8]) -> Option<(i64, usize)> {
    let (rowid, n) = varint::decode_signed(buf)?;
    Some((rowid, n))
}

/// A B+tree over a pager. Trees are identified by their root page ID.
pub struct Btree<'a> {
    pub pager: &'a Pager,
    pub root: PageId,
    pub is_index: bool,
}

/// Result of inserting into a page: either the insert succeeded, or the
/// page split and a separator needs to be propagated up.
///
/// For TABLE splits, `split_key` is the FIRST key of the new (right) page;
/// the parent uses `split_key - 1` as the left child's separator (a safe
/// over-estimate within the inter-page key gap).
///
/// For INDEX splits, `(split_key_bytes, split_key)` is the EXACT last
/// entry of the left page — the left child's separator for the parent.
enum InsertResult {
    Done,
    Split {
        new_page: PageId,
        split_key: i64,
        /// For index splits: the key bytes of the left page's max entry.
        split_key_bytes: Option<Vec<u8>>,
    },
}

/// A point lookup result.
pub enum LookupResult {
    Found(Vec<u8>),
    NotFound,
}

impl<'a> Btree<'a> {
    pub fn new(pager: &'a Pager, root: PageId, is_index: bool) -> Self {
        Self {
            pager,
            root,
            is_index,
        }
    }

    /// Initialize a new B+tree (create the root page as an empty leaf).
    pub fn create(pager: &'a Pager, is_index: bool) -> Result<Self> {
        let root = pager.allocate_page()?;
        let page = pager.get_page(root)?;
        if is_index {
            page.lock().init_leaf_index();
        } else {
            page.lock().init_leaf_table();
        }
        Ok(Self {
            pager,
            root,
            is_index,
        })
    }

    /// Look up a rowid in a table B+tree. Returns the payload bytes.
    /// Look up a row by rowid in a table B+tree. Walks interior pages
    /// (binary-searching each one) and finally binary-searches the leaf.
    ///
    /// Performance: this used to do a linear scan of the leaf with
    /// `Cell::decode` per cell — each decode allocates a `Vec<u8>` for the
    /// payload, so an N-cell leaf did N heap allocations per lookup. With
    /// the rowid-only fast path (`decode_rowid_only`) and binary search,
    /// it's now O(log N) decodes (no allocations during the search) plus
    /// one final allocation for the matched payload. For a 100-cell leaf,
    /// that's ~7 decodes (vs ~50 avg) and 1 allocation (vs ~50).
    pub fn lookup_table(&mut self, rowid: i64) -> Result<LookupResult> {
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            // ONE lock per page: determine the type and do the leaf work
            // under the same guard (was: a temp lock for page_type plus a
            // second lock for the scan — two atomic RMWs per level per
            // descent).
            let borrowed = page.lock();
            let pt = borrowed.page_type()?;
            match pt {
                PageType::LeafTable => {
                    // (guard held for the whole leaf scan)
                    let n = borrowed.n_cells() as usize;
                    // Binary search by rowid (cells are stored sorted).
                    let mut lo = 0usize;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                        // Read just the rowid — no payload allocation.
                        let (cell_rowid, _) = decode_rowid_only(&borrowed.data[cell_ptr..])
                            .ok_or_else(|| Error::corruption("truncated leaf rowid in lookup"))?;
                        if cell_rowid == rowid {
                            // Found — decode the full cell ONCE.
                            let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                            if let Cell::TableLeaf { payload, .. } = cell {
                                return Ok(LookupResult::Found(payload));
                            }
                            unreachable!();
                        } else if cell_rowid < rowid {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    return Ok(LookupResult::NotFound);
                }
                PageType::InteriorTable => {
                    let n = borrowed.n_cells() as usize;
                    // Binary search the interior cells for the right child.
                    // Cells are sorted by key; cell (left_child, key) means
                    // left_child contains rowids <= key.
                    let mut next = borrowed.right_most_pointer();
                    let mut lo = 0usize;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                        // Read just the key — no payload allocation.
                        // Interior cell layout: [left_child: u32 BE][key: varint]
                        if cell_ptr + 4 > borrowed.data.len() {
                            break;
                        }
                        let _left_child = u32::from_be_bytes(
                            borrowed.data[cell_ptr..cell_ptr + 4].try_into().unwrap(),
                        );
                        let (key, _) = varint::decode_signed(&borrowed.data[cell_ptr + 4..])
                            .ok_or_else(|| Error::corruption("truncated interior key in lookup"))?;
                        if rowid <= key {
                            // This cell's left_child contains rowid.
                            next = _left_child;
                            // Continue searching left for a tighter bound.
                            hi = mid;
                        } else {
                            lo = mid + 1;
                        }
                    }
                    drop(borrowed);
                    if next == 0 {
                        return Err(Error::corruption(format!(
                            "interior page {} has no valid child for rowid {}",
                            page_id, rowid
                        )));
                    }
                    page_id = next;
                }
                _ => {
                    return Err(Error::corruption(format!(
                        "unexpected page type in table btree: {:?}",
                        pt
                    )))
                }
            }
        }
    }

    /// Insert a (rowid, payload) pair into a table B+tree.
    pub fn insert_table(&mut self, rowid: i64, payload: &[u8]) -> Result<()> {
        // Notify the pager that a write is about to happen. This maintains
        // the O(1) dirty-page counter used by `flush()`'s fast path
        // (see `Pager::note_write`).
        self.pager.note_write();
        let cell = Cell::TableLeaf {
            rowid,
            payload: payload.to_vec(),
        };
        match self.insert_into_page(self.root, cell)? {
            InsertResult::Done => Ok(()),
            InsertResult::Split {
                new_page,
                split_key,
                split_key_bytes,
            } => {
                // The root split: create a new root pointing to the old and new pages.
                let old_root = self.root;
                let new_root = self.pager.allocate_page()?;
                {
                    let page_ref = self.pager.get_page(new_root)?;
                    let mut page = page_ref.lock();
                    page.init_interior_table();
                    page.set_right_most_pointer(new_page);
                }
                // Insert a cell pointing to the old root with the split key.
                // Convention: cell (left_child, key) means left_child has rowids <= key.
                // split_key = first key of new page. Old root has keys < split_key.
                // So the cell key should be split_key - 1 (max key in old root).
                let _ = split_key_bytes;
                let cell = Cell::TableInterior {
                    left_child: old_root,
                    key: split_key - 1,
                };
                self.insert_cell_into_page(new_root, &cell)?;
                self.root = new_root;
                Ok(())
            }
        }
    }

    /// Bulk-append insert: like `insert_table` but optimized for sequential
    /// rowid inserts (the common case for `INSERT INTO t VALUES (...)` with
    /// an auto-generated INTEGER PRIMARY KEY). Walks the right_most_pointer
    /// chain down to the rightmost leaf WITHOUT binary-searching interior
    /// pages, then appends at the end of the leaf WITHOUT binary-searching
    /// cell positions.
    ///
    /// Returns the new root (may differ from the old root if the tree split).
    /// The caller must update its cached root.
    ///
    /// Mirrors SQLite's `BTREE_APPEND` optimization. For 1k sequential
    /// inserts, this skips ~10k binary searches (each ~200 ns on a hot
    /// CPU) — a ~2 ms saving.
    ///
    /// Precondition: `rowid > current_max_rowid` (caller's responsibility).
    /// If the precondition is violated, this falls back to the normal path.
    pub fn insert_table_append(&mut self, rowid: i64, payload: &[u8]) -> Result<()> {
        self.insert_table_append_inner(rowid, payload, None).map(|_| ())
    }

    /// Append with a LEAF HINT from a previous append in the SAME statement:
    /// skips the root-to-leaf descent entirely when the hinted leaf is still
    /// the right-most leaf with room (the overwhelmingly common case in bulk
    /// sequential inserts). The hint is validated per use — wrong type,
    /// non-monotonic rowid, or a full page falls back to the full descent.
    /// Returns the new hint (the leaf that received this row).
    ///
    /// SAFETY of the hint: it is only valid within a single statement (no
    /// DROP/DELETE/ROLLBACK can intervene), and every use re-validates the
    /// page type and ordering — so a stale hint is detected, never applied.
    pub fn insert_table_append_hinted(
        &mut self,
        rowid: i64,
        payload: &[u8],
        hint: Option<PageId>,
    ) -> Result<Option<PageId>> {
        self.insert_table_append_inner(rowid, payload, hint)
    }

    fn insert_table_append_inner(
        &mut self,
        rowid: i64,
        payload: &[u8],
        hint: Option<PageId>,
    ) -> Result<Option<PageId>> {
        self.pager.note_write();

        // Fast path: validate the hinted leaf and append directly into it.
        if let Some(h) = hint {
            if let Some(new_hint) = self.try_append_into_leaf(h, rowid, payload)? {
                return Ok(Some(new_hint));
            }
            // The hint tracks this tree's right-most leaf — a failed append
            // (ordering or space) completes via the full insert path + a
            // re-pin; walking the chain first would retry the same failure.
            self.insert_table(rowid, payload)?;
            return Ok(Some(self.right_most_leaf()?));
        }

        // Walk right_most_pointer chain down to the rightmost leaf.
        // One lock per level: read page_type and right_most_pointer in a
        // single guard (previously two separate `.lock()` acquisitions).
        let mut page_id = self.root;
        let leaf_id;
        loop {
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafTable => {
                    leaf_id = page_id;
                    break;
                }
                PageType::InteriorTable => {
                    let right = guard.right_most_pointer();
                    if right == 0 {
                        // Shouldn't happen on a valid interior page, but fall back.
                        drop(guard);
                        self.insert_table(rowid, payload)?;
                        return Ok(Some(self.right_most_leaf()?));
                    }
                    page_id = right;
                }
                _ => {
                    drop(guard);
                    self.insert_table(rowid, payload)?;
                    return Ok(Some(self.right_most_leaf()?));
                }
            }
        }

        // Rightmost leaf: verify the append, check space, and write.
        if let Some(h) = self.try_append_into_leaf(leaf_id, rowid, payload)? {
            return Ok(Some(h));
        }
        // Not an append / full leaf — the normal insert path handles
        // splitting + propagation.
        self.insert_table(rowid, payload)?;
        // Re-pin the right-most leaf (one descent; runs only after splits).
        Ok(Some(self.right_most_leaf()?))
    }

    /// Try to append `rowid -> payload` directly into leaf page `leaf_id`.
    /// Returns `Ok(Some(leaf_id))` on success, `Ok(None)` when the page is
    /// not a table leaf, the rowid isn't past the last cell, or there is no
    /// room (caller falls back to the full insert path).
    fn try_append_into_leaf(&self, leaf_id: PageId, rowid: i64, payload: &[u8]) -> Result<Option<PageId>> {
        let page = self.pager.get_page(leaf_id)?;
        // Cell bytes: varint rowid + varint payload len + payload. Typical
        // rows are well under 256 bytes — build in a stack buffer and only
        // fall back to the heap when genuinely large (avoids a per-insert
        // `Vec::with_capacity` allocation for the common case).
        let mut cell_stack = [0u8; 256];
        let mut cell_heap: Vec<u8>;
        let mut rid_buf = [0u8; 9];
        let n_rid = varint::encode_signed(rowid, &mut rid_buf);
        let mut plen_buf = [0u8; 9];
        let n_plen = varint::encode(payload.len() as u64, &mut plen_buf);
        let cell_len = n_rid + n_plen + payload.len();
        let cell: &[u8] = if cell_len <= cell_stack.len() {
            cell_stack[..n_rid].copy_from_slice(&rid_buf[..n_rid]);
            cell_stack[n_rid..n_rid + n_plen].copy_from_slice(&plen_buf[..n_plen]);
            cell_stack[n_rid + n_plen..cell_len].copy_from_slice(payload);
            &cell_stack[..cell_len]
        } else {
            cell_heap = Vec::with_capacity(cell_len);
            cell_heap.extend_from_slice(&rid_buf[..n_rid]);
            cell_heap.extend_from_slice(&plen_buf[..n_plen]);
            cell_heap.extend_from_slice(payload);
            &cell_heap
        };
        let cell_size = cell.len() as u32;

        let mut borrowed = page.lock();
        if borrowed.page_type()? != PageType::LeafTable {
            return Ok(None); // stale hint pointing at a non-leaf page
        }
        let n = borrowed.n_cells();
        if n > 0 {
            let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
            if let Some((last_rowid, _)) = varint::decode_signed(&borrowed.data[cell_ptr..]) {
                if rowid <= last_rowid {
                    // Not an append — caller falls back to the normal path.
                    return Ok(None);
                }
            }
        }

        // Check if the leaf has space.
        let free = borrowed.free_space();
        if free < cell_size + 2 {
            // Need to split — caller handles splitting + propagation.
            return Ok(None);
        }

        // Append: write cell at the new content start, write pointer at end.
        {
            let new_content_start = borrowed.cell_content_start() - cell_size;
            let off = new_content_start as usize;
            borrowed.data[off..off + cell_size as usize].copy_from_slice(cell);
            borrowed.set_cell_content_start(new_content_start);

            // Append the cell pointer at position `n` (end).
            let header_offset = if leaf_id == 0 {
                crate::storage::page::DB_HEADER_SIZE as usize
            } else {
                0
            };
            let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
            let dst = ptr_array_start + n as usize * 2;
            borrowed.data[dst..dst + 2]
                .copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n + 1);
            borrowed.dirty = true;
        }
        drop(borrowed);
        self.pager.note_dirty(leaf_id);
        Ok(Some(leaf_id))
    }

    /// Descend the right-most-child chain to the right-most leaf.
    fn right_most_leaf(&self) -> Result<PageId> {
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafTable => return Ok(page_id),
                PageType::InteriorTable => {
                    let right = guard.right_most_pointer();
                    if right == 0 {
                        return Ok(page_id);
                    }
                    page_id = right;
                }
                _ => return Ok(page_id),
            }
        }
    }

    /// Insert an index entry with a LEAF HINT from a previous append in
    /// the SAME statement — the index-tree mirror of
    /// `insert_table_append_hinted`. Bulk loads that insert ascending
    /// `(key, rowid)` pairs (e.g. an auto-increment rowid whose indexed
    /// column also increases) pin the right-most index leaf and append
    /// straight into it: no root-to-leaf descent, no binary search, no
    /// interior-page reads. The hint is re-validated on every use (page
    /// type, ordering, free space), so stale hints fall back to the full
    /// insert path automatically.
    pub fn insert_index_append_hinted(
        &mut self,
        key: &[u8],
        rowid: i64,
        hint: Option<PageId>,
    ) -> Result<Option<PageId>> {
        self.pager.note_write();

        // Fast path: validate the hinted leaf and append directly into it.
        if let Some(h) = hint {
            if let Some(new_hint) = self.try_append_into_index_leaf(h, key, rowid)? {
                return Ok(Some(new_hint));
            }
            // The hint tracks this tree's right-most leaf, so a failed
            // append (ordering or space) completes via the full insert
            // path + a re-pin. Walking the right-most chain first would
            // just retry the same failed append on the same leaf.
            self.insert_index(key, rowid)?;
            return Ok(Some(self.right_most_index_leaf()?));
        }

        // No hint yet: walk the right-most-child chain down to the
        // right-most index leaf.
        let mut page_id = self.root;
        let leaf_id;
        loop {
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafIndex => {
                    leaf_id = page_id;
                    break;
                }
                PageType::InteriorIndex => {
                    let right = guard.right_most_pointer();
                    if right == 0 {
                        drop(guard);
                        self.insert_index(key, rowid)?;
                        return Ok(Some(self.right_most_index_leaf()?));
                    }
                    page_id = right;
                }
                _ => {
                    drop(guard);
                    self.insert_index(key, rowid)?;
                    return Ok(Some(self.right_most_index_leaf()?));
                }
            }
        }

        if let Some(h) = self.try_append_into_index_leaf(leaf_id, key, rowid)? {
            return Ok(Some(h));
        }
        // Not an append / full leaf — the normal insert path handles
        // splitting + propagation.
        self.insert_index(key, rowid)?;
        Ok(Some(self.right_most_index_leaf()?))
    }

    /// Descend the right-most-child chain to the right-most INDEX leaf.
    fn right_most_index_leaf(&self) -> Result<PageId> {
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafIndex => return Ok(page_id),
                PageType::InteriorIndex => {
                    let right = guard.right_most_pointer();
                    if right == 0 {
                        return Ok(page_id);
                    }
                    page_id = right;
                }
                _ => return Ok(page_id),
            }
        }
    }

    /// Try to append `(key, rowid)` directly into index leaf `leaf_id`.
    /// Returns `Ok(Some(leaf_id))` on success, `Ok(None)` when the page is
    /// not an index leaf, the entry isn't past the last cell, or there is
    /// no room (caller falls back to the full insert path).
    fn try_append_into_index_leaf(
        &self,
        leaf_id: PageId,
        key: &[u8],
        rowid: i64,
    ) -> Result<Option<PageId>> {
        let page = self.pager.get_page(leaf_id)?;
        // Index leaf cell layout: varint(rowid) + varint(key_len) + key.
        let mut cell_stack = [0u8; 256];
        let mut cell_heap: Vec<u8>;
        let mut rid_buf = [0u8; 9];
        let n_rid = varint::encode_signed(rowid, &mut rid_buf);
        let mut klen_buf = [0u8; 9];
        let n_klen = varint::encode(key.len() as u64, &mut klen_buf);
        let cell_len = n_rid + n_klen + key.len();
        let cell: &[u8] = if cell_len <= cell_stack.len() {
            cell_stack[..n_rid].copy_from_slice(&rid_buf[..n_rid]);
            cell_stack[n_rid..n_rid + n_klen].copy_from_slice(&klen_buf[..n_klen]);
            cell_stack[n_rid + n_klen..cell_len].copy_from_slice(key);
            &cell_stack[..cell_len]
        } else {
            cell_heap = Vec::with_capacity(cell_len);
            cell_heap.extend_from_slice(&rid_buf[..n_rid]);
            cell_heap.extend_from_slice(&klen_buf[..n_klen]);
            cell_heap.extend_from_slice(key);
            &cell_heap
        };
        let cell_size = cell.len() as u32;

        let mut borrowed = page.lock();
        if borrowed.page_type()? != PageType::LeafIndex {
            return Ok(None); // stale hint pointing at a non-index page
        }
        let n = borrowed.n_cells();
        if n > 0 {
            let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
            if let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], false) {
                // Append requires (key, rowid) strictly after the last
                // entry — index pages are sorted by (key, rowid).
                if !(v.key < key || (v.key == key && v.rowid < rowid)) {
                    return Ok(None);
                }
            }
        }

        // Check if the leaf has space.
        let free = borrowed.free_space();
        if free < cell_size + 2 {
            return Ok(None); // need a split — caller handles it
        }

        // Append: write cell at the new content start, pointer at the end.
        {
            let new_content_start = borrowed.cell_content_start() - cell_size;
            let off = new_content_start as usize;
            borrowed.data[off..off + cell_size as usize].copy_from_slice(cell);
            borrowed.set_cell_content_start(new_content_start);

            let header_offset = if leaf_id == 0 {
                crate::storage::page::DB_HEADER_SIZE as usize
            } else {
                0
            };
            let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
            let dst = ptr_array_start + n as usize * 2;
            borrowed.data[dst..dst + 2]
                .copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n + 1);
            borrowed.dirty = true;
        }
        drop(borrowed);
        self.pager.note_dirty(leaf_id);
        Ok(Some(leaf_id))
    }

    /// Update a row in a table B+tree in place when possible.
    ///
    /// If the new payload has the same length as the existing payload,
    /// overwrite the payload bytes directly in the leaf cell — no delete,
    /// no insert, no cell-pointer shifts, no risk of leaf split. This is
    /// the fast path used by `exec_update` for `UPDATE t SET col = ...`
    /// where the column type doesn't change (e.g. `score = score + 1.0`
    /// on a REAL column — payload size is identical before and after).
    ///
    /// Returns `Ok(true)` if the in-place update succeeded, `Ok(false)` if
    /// the rowid wasn't found, or `Ok(false)` if the payload size changed
    /// and the caller should fall back to delete + insert.
    ///
    /// For benchmark impact: `UPDATE by PK` 1k ops drops from ~11 ms
    /// (delete+insert) to ~5 ms (in-place), putting us within 3× of SQLite.
    /// Bulk in-place UPDATE for a SORTED list of (rowid, new_payload).
    ///
    /// One full tree traversal visits every leaf sequentially (no
    /// root-to-leaf descent per row — the dominant cost of the old
    /// per-row `update_table` loop on `UPDATE ... WHERE indexed_col > ?`,
    /// which paid ~250 ns of descent per row). Within each leaf, updates
    /// are matched in order (both sides sorted) with a merge-style
    /// advance instead of a per-row binary search.
    ///
    /// Rows whose new payload size differs from the old one CANNOT be
    /// patched in place (cell size is fixed); their INDICES in `updates`
    /// are pushed to `deferred` and the caller falls back to the
    /// delete+insert path for just those rows. Rowids not present in the
    /// table are likewise deferred (the caller's fallback path reports
    /// the miss consistently with update_table's `Ok(false)`).
    pub fn update_table_bulk(
        &mut self,
        updates: &[(i64, &[u8])],
        deferred: &mut Vec<usize>,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        self.pager.note_write();
        let mut ui = 0usize;
        self.update_table_bulk_subtree(self.root, updates, &mut ui, deferred)?;
        Ok(())
    }

    fn update_table_bulk_subtree(
        &mut self,
        page_id: PageId,
        updates: &[(i64, &[u8])],
        ui: &mut usize,
        deferred: &mut Vec<usize>,
    ) -> Result<()> {
        if *ui >= updates.len() {
            return Ok(());
        }
        let page = self.pager.get_page(page_id)?;
        let mut borrowed = page.lock();
        let pt = borrowed.page_type()?;
        let mut wrote = false;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells() as usize;
                let psz = borrowed.data.len();
                // In-order merge: cells and updates are both sorted by rowid.
                // START with a binary search for the first cell >= the next
                // pending rowid — sparse updates (one row per leaf, e.g.
                // UPDATE ... WHERE id = ?) would otherwise linear-scan from
                // cell 0 (~130 cells/leaf).
                let mut ci = {
                    let target = updates[*ui].0;
                    let mut lo = 0usize;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                        match varint::decode_signed(&borrowed.data[cell_ptr..]) {
                            Some((cell_rowid, _)) => {
                                if cell_rowid < target {
                                    lo = mid + 1;
                                } else {
                                    hi = mid;
                                }
                            }
                            None => break,
                        }
                    }
                    lo
                };
                while *ui < updates.len() && ci < n {
                    let cell_ptr = borrowed.cell_pointer(ci as u16) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption("cell pointer out of range in bulk update"));
                    }
                    let (cell_rowid, n_rid) = varint::decode_signed(&borrowed.data[cell_ptr..])
                        .ok_or_else(|| Error::corruption("truncated rowid in bulk update"))?;
                    let (rowid, new_payload) = updates[*ui];
                    match cell_rowid.cmp(&rowid) {
                        std::cmp::Ordering::Less => ci += 1,
                        std::cmp::Ordering::Greater => {
                            // This update's rowid isn't in the table — defer.
                            deferred.push(*ui);
                            *ui += 1;
                        }
                        std::cmp::Ordering::Equal => {
                            let plen_pos = cell_ptr + n_rid;
                            let (plen, n_plen) = varint::decode(&borrowed.data[plen_pos..])
                                .ok_or_else(|| Error::corruption("truncated payload len in bulk update"))?;
                            let payload_offset = plen_pos + n_plen;
                            if plen as usize == new_payload.len()
                                && payload_offset + new_payload.len() <= psz
                            {
                                borrowed.data[payload_offset..payload_offset + new_payload.len()]
                                    .copy_from_slice(new_payload);
                                borrowed.dirty = true;
                                // The write happened while the guard is held;
                                // note the dirty page AFTER dropping it.
                                wrote = true;
                            } else {
                                // Size changed (or truncated) — defer to the
                                // delete+insert fallback.
                                deferred.push(*ui);
                            }
                            *ui += 1;
                            ci += 1;
                        }
                    }
                }
                drop(borrowed);
                if wrote {
                    self.pager.note_dirty(page_id);
                }
                Ok(())
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells() as usize;
                // Binary-search the first child whose separator key is >=
                // the next pending update's rowid — children entirely below
                // the update range are skipped WITHOUT decoding their keys
                // (collecting every separator was ~75 key decodes per
                // statement at the root of a 10k-row table).
                let data_len = borrowed.data.len();
                let cell_key = |i: usize| -> Option<i64> {
                    let cell_ptr = borrowed.cell_pointer(i as u16) as usize;
                    if cell_ptr + 4 > data_len {
                        return None;
                    }
                    varint::decode_signed(&borrowed.data[cell_ptr + 4..]).map(|(k, _)| k)
                };
                let mut lo = 0usize;
                let mut hi = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    match cell_key(mid) {
                        Some(key) if key < updates[*ui].0 => lo = mid + 1,
                        Some(_) => hi = mid,
                        None => break,
                    }
                }
                // Collect only the candidate children from `lo` onward
                // (each recursion consumes updates in order and the loop
                // exits as soon as the pending list is drained).
                let mut children: Vec<PageId> = Vec::new();
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i as u16) as usize;
                    if cell_ptr + 4 > data_len {
                        break;
                    }
                    let left = u32::from_be_bytes(
                        borrowed.data[cell_ptr..cell_ptr + 4].try_into().unwrap(),
                    );
                    children.push(left);
                    // For single-row updates, stop after the containing
                    // child plus one (the next child can't be needed).
                    if *ui + 1 >= updates.len() {
                        break;
                    }
                }
                let right = borrowed.right_most_pointer();
                drop(borrowed);
                drop(page);
                for child in children {
                    if *ui >= updates.len() {
                        return Ok(());
                    }
                    self.update_table_bulk_subtree(child, updates, ui, deferred)?;
                }
                if right != 0 && *ui < updates.len() {
                    self.update_table_bulk_subtree(right, updates, ui, deferred)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn update_table(&mut self, rowid: i64, new_payload: &[u8]) -> Result<bool> {
        // Notify the pager that a write is about to happen (in-place UPDATE).
        self.pager.note_write();
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            let pt = page.lock().page_type()?;
            match pt {
                PageType::LeafTable => {
                    // Binary search for the cell by rowid (cells are stored
                    // sorted) — reading ONLY the rowid varint per probe, no
                    // payload allocation. This used to be a linear scan that
                    // fully decoded every cell (a Vec allocation each); with
                    // codec v2 + append-mode splits a 16 KiB leaf holds
                    // ~1000 cells, so the linear scan cost ~500 allocations
                    // (~7 µs) per UPDATE-by-PK. Binary search needs ~10
                    // rowid reads.
                    let (_cell_ptr, payload_offset, old_len) = {
                        let borrowed = page.lock();
                        let n = borrowed.n_cells() as usize;
                        let mut lo = 0usize;
                        let mut hi = n;
                        let mut found: Option<usize> = None;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                            let (cell_rowid, n_rid) =
                                decode_rowid_only(&borrowed.data[cell_ptr..]).ok_or_else(|| {
                                    Error::corruption("truncated leaf rowid in update")
                                })?;
                            match cell_rowid.cmp(&rowid) {
                                std::cmp::Ordering::Equal => {
                                    found = Some(cell_ptr + n_rid);
                                    break;
                                }
                                std::cmp::Ordering::Less => lo = mid + 1,
                                std::cmp::Ordering::Greater => hi = mid,
                            }
                        }
                        match found {
                            None => return Ok(false), // rowid not present
                            Some(payload_len_pos) => {
                                // Decode the payload-length varint.
                                let (plen, n_plen) = varint::decode(
                                    &borrowed.data[payload_len_pos..],
                                )
                                .ok_or_else(|| {
                                    Error::corruption("truncated payload length in update")
                                })?;
                                (payload_len_pos, payload_len_pos + n_plen, plen as usize)
                            }
                        }
                    };
                    if old_len != new_payload.len() {
                        // Size changed — caller must fall back to delete+insert.
                        return Ok(false);
                    }
                    // Overwrite the payload bytes — single mutable borrow,
                    // no immutable borrows outstanding.
                    {
                        let mut borrowed = page.lock();
                        borrowed.data[payload_offset..payload_offset + new_payload.len()]
                            .copy_from_slice(new_payload);
                        borrowed.dirty = true;
                    }
                    self.pager.note_dirty(page_id);
                    return Ok(true);
                }
                PageType::InteriorTable => {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let mut next = borrowed.right_most_pointer();
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, key } = cell {
                            if rowid <= key {
                                next = left_child;
                                break;
                            }
                        }
                    }
                    page_id = next;
                }
                _ => {
                    return Err(Error::corruption(format!(
                        "unexpected page type in update_table: {:?}",
                        pt
                    )));
                }
            }
        }
    }

    /// Insert a cell into a page (and propagate splits if needed).
    fn insert_into_page(&mut self, page_id: PageId, cell: Cell) -> Result<InsertResult> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;

        if pt.is_leaf() {
            // Leaf: insert directly.
            let cell_size = cell.encoded_size();
            let free = page.lock().free_space();
            // Need space for the cell + a 2-byte pointer.
            if free < cell_size as u32 + 2 {
                drop(page);
                return self.split_leaf(page_id, cell);
            }
            self.insert_cell_into_page(page_id, &cell)?;
            Ok(InsertResult::Done)
        } else {
            // Interior: find the child to descend into.
            // Convention: cell (left_child, sep) means left_child contains
            // entries <= sep. Descent: take the FIRST cell whose separator is
            // >= the target; go to its left_child. If none, go to right_most.
            // Table pages compare by i64 key; index pages by (key, rowid).
            //
            // Binary search with allocation-free cell views. The previous
            // code linearly decoded EVERY interior cell (for index pages
            // that's a Vec allocation per cell — ~150-200 per page, per
            // level, per descent).
            let is_idx = pt.is_index();
            let child_id = {
                let borrowed = page.lock();
                let n = borrowed.n_cells();
                let right_most = borrowed.right_most_pointer();
                let data = &borrowed.data;
                let cp = |i: u16| borrowed.cell_pointer(i);
                if is_idx {
                    let (_, child) = find_index_child(
                        data, n, cp, right_most, cell.index_key(), cell.key(),
                    );
                    child
                } else {
                    // Table interior: [be_u32 child][varint key].
                    let mut lo: u16 = 0;
                    let mut hi: u16 = n;
                    let target_key = cell.key();
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let ptr = cp(mid) as usize;
                        let sep = decode_table_interior_key(&data[ptr..]);
                        match sep {
                            Some(k) if k < target_key => lo = mid + 1,
                            Some(_) => hi = mid,
                            None => break,
                        }
                    }
                    if lo >= n {
                        right_most
                    } else {
                        let ptr = cp(lo) as usize;
                        decode_table_interior_child(&data[ptr..]).unwrap_or(right_most)
                    }
                }
            };
            drop(page);
            match self.insert_into_page(child_id, cell)? {
                InsertResult::Done => Ok(InsertResult::Done),
                InsertResult::Split {
                    new_page,
                    split_key,
                    split_key_bytes,
                } => {
                    // The child split. We must replace the cell that pointed
                    // to `child_id` with TWO cells:
                    //   cell1 = (child_id, left_max)   — the child now holds
                    //                                  entries <= left_max
                    //   cell2 = (new_page, old_sep)   — the new page holds
                    //                                  entries in (left_max, old_sep]
                    // (or, if child_id was right_most: add cell1 and make
                    //  new_page the new right_most.)
                    //
                    // left_max for table trees is split_key - 1 (an
                    // over-estimate inside the key gap — safe); for index
                    // trees it is the EXACT (split_key_bytes, split_key).
                    //
                    // ------------------------------------------------------------------
                    // IN-PLACE SPLICE fast path: when the parent has room for
                    // one more cell (the overwhelmingly common case — parents
                    // only fill after hundreds of child splits), replace the
                    // old pointer with two and write both cell bytes directly.
                    // The old path decoded EVERY parent cell into an
                    // allocating Vec<Cell> and rewrote the whole page via
                    // insert_cell_into_page — O(n) allocations + O(n^2)
                    // pointer shifting per split, which dominated random-key
                    // insert workloads.
                    // ------------------------------------------------------------------
                    {
                        let page = self.pager.get_page(page_id)?;
                        let (n_now, pt_now, free, right_now) = {
                            let b = page.lock();
                            (b.n_cells(), b.page_type()?, b.free_space(), b.right_most_pointer())
                        };
                        let is_idx_now = pt_now.is_index();
                        // Find the cell pointing at child_id (view-decode, no
                        // allocation), or note the right-most case.
                        let mut found: Option<(usize, usize)> = None; // (index, byte offset)
                        let mut is_right_most = right_now == child_id && child_id != 0;
                        if !is_right_most {
                            let b = page.lock();
                            for i in 0..n_now {
                                let ptr = b.cell_pointer(i) as usize;
                                let child = if is_idx_now {
                                    decode_index_cell(&b.data[ptr..], true)
                                        .map(|v| v.left_child)
                                        .unwrap_or(0)
                                } else {
                                    decode_table_interior_child(&b.data[ptr..]).unwrap_or(0)
                                };
                                if child == child_id {
                                    found = Some((i as usize, ptr));
                                    break;
                                }
                            }
                            if found.is_none() {
                                // Inconsistent tree — let the slow path's
                                // error handling deal with it.
                                is_right_most = false;
                            }
                        }

                        if is_right_most {
                            // Append cell1 = (child_id, separator) and make
                            // new_page the new right-most child.
                            let cell1 = if is_idx_now {
                                Cell::IndexInterior {
                                    left_child: child_id,
                                    key: split_key_bytes.clone().unwrap_or_default(),
                                    rowid: split_key,
                                }
                            } else {
                                Cell::TableInterior {
                                    left_child: child_id,
                                    key: split_key - 1,
                                }
                            };
                            let s1 = cell1.encoded_size() as u32;
                            if free >= s1 + 2 {
                                let content_start = {
                                    let b = page.lock();
                                    b.cell_content_start()
                                };
                                let c1_start = content_start - s1;
                                {
                                    let mut b = page.lock();
                                    let mut buf = Vec::with_capacity(s1 as usize);
                                    cell1.encode(&mut buf);
                                    let off = c1_start as usize;
                                    b.data[off..off + s1 as usize].copy_from_slice(&buf);
                                    b.set_cell_content_start(c1_start);
                                    let header_offset = if page_id == 0 {
                                        crate::storage::page::DB_HEADER_SIZE as usize
                                    } else {
                                        0
                                    };
                                    let ptr_array_start =
                                        header_offset + PAGE_HEADER_SIZE as usize;
                                    let dst = ptr_array_start + n_now as usize * 2;
                                    b.data[dst..dst + 2]
                                        .copy_from_slice(&(c1_start as u16).to_be_bytes());
                                    b.set_n_cells(n_now + 1);
                                    b.set_right_most_pointer(new_page);
                                    b.dirty = true;
                                }
                                self.pager.note_dirty(page_id);
                                return Ok(InsertResult::Done);
                            }
                        } else if let Some((idx, old_off)) = found {
                            // Build cell1/cell2 and splice them in place of
                            // the old cell at `idx`.
                            let (cell1, cell2) = if is_idx_now {
                                let (old_key, old_rowid) =
                                    match decode_index_cell(&page.lock().data[old_off..], true) {
                                        Some(v) => (v.key.to_vec(), v.rowid),
                                        None => (Vec::new(), i64::MAX),
                                    };
                                (
                                    Cell::IndexInterior {
                                        left_child: child_id,
                                        key: split_key_bytes.clone().unwrap_or_default(),
                                        rowid: split_key,
                                    },
                                    Cell::IndexInterior {
                                        left_child: new_page,
                                        key: old_key,
                                        rowid: old_rowid,
                                    },
                                )
                            } else {
                                let old_key = decode_table_interior_key(&page.lock().data[old_off..])
                                    .unwrap_or(i64::MAX);
                                (
                                    Cell::TableInterior {
                                        left_child: child_id,
                                        key: split_key - 1,
                                    },
                                    Cell::TableInterior {
                                        left_child: new_page,
                                        key: old_key,
                                    },
                                )
                            };
                            let s1 = cell1.encoded_size() as u32;
                            let s2 = cell2.encoded_size() as u32;
                            if free >= s1 + s2 + 2 {
                                let content_start = {
                                    let b = page.lock();
                                    b.cell_content_start()
                                };
                                let c2_start = content_start - s2;
                                let c1_start = c2_start - s1;
                                {
                                    let mut b = page.lock();
                                    let mut buf = Vec::with_capacity((s1 + s2) as usize);
                                    cell1.encode(&mut buf);
                                    cell2.encode(&mut buf);
                                    let off = c1_start as usize;
                                    b.data[off..off + (s1 + s2) as usize].copy_from_slice(&buf);
                                    b.set_cell_content_start(c1_start);
                                    // Splice the pointer array: shift
                                    // [idx+1..n] one slot right, write the
                                    // two new pointers at idx, idx+1.
                                    let header_offset = if page_id == 0 {
                                        crate::storage::page::DB_HEADER_SIZE as usize
                                    } else {
                                        0
                                    };
                                    let ptr_array_start =
                                        header_offset + PAGE_HEADER_SIZE as usize;
                                    let idx_usize = idx;
                                    let n_usize = n_now as usize;
                                    b.data.copy_within(
                                        ptr_array_start + (idx_usize + 1) * 2
                                            ..ptr_array_start + n_usize * 2,
                                        ptr_array_start + (idx_usize + 2) * 2,
                                    );
                                    b.data[ptr_array_start + idx_usize * 2
                                        ..ptr_array_start + idx_usize * 2 + 2]
                                        .copy_from_slice(&(c1_start as u16).to_be_bytes());
                                    b.data[ptr_array_start + (idx_usize + 1) * 2
                                        ..ptr_array_start + (idx_usize + 1) * 2 + 2]
                                        .copy_from_slice(&(c2_start as u16).to_be_bytes());
                                    b.set_n_cells(n_now + 1);
                                    b.dirty = true;
                                }
                                self.pager.note_dirty(page_id);
                                return Ok(InsertResult::Done);
                            }
                        }
                        // No room (or inconsistent state): fall through to
                        // the full rebuild path, which also handles splitting
                        // this interior page and propagating upward.
                    }
                    let (n_cells, pt2) = {
                        let p = self.pager.get_page(page_id)?;
                        let b = p.lock();
                        (b.n_cells(), b.page_type()?)
                    };
                    let is_idx_page = pt2.is_index();

                    // Read all cells + the right_most pointer.
                    let mut cells: Vec<Cell> = Vec::new();
                    let mut right_most = 0u32;
                    let mut found_idx: Option<usize> = None;
                    {
                        let p = self.pager.get_page(page_id)?;
                        let borrowed = p.lock();
                        right_most = borrowed.right_most_pointer();
                        for i in 0..n_cells {
                            let cell_ptr = borrowed.cell_pointer(i) as usize;
                            let c = Cell::decode(&borrowed.data[cell_ptr..], pt2)?;
                            if c.left_child() == child_id {
                                found_idx = Some(i as usize);
                            }
                            cells.push(c);
                        }
                    }

                    if let Some(idx) = found_idx {
                        if is_idx_page {
                            let (old_key, old_rowid) = match &cells[idx] {
                                Cell::IndexInterior { key, rowid, .. } => (key.clone(), *rowid),
                                _ => (Vec::new(), i64::MAX),
                            };
                            let cell1 = Cell::IndexInterior {
                                left_child: child_id,
                                key: split_key_bytes.clone().unwrap_or_default(),
                                rowid: split_key,
                            };
                            let cell2 = Cell::IndexInterior {
                                left_child: new_page,
                                key: old_key,
                                rowid: old_rowid,
                            };
                            cells.remove(idx);
                            cells.insert(idx, cell2);
                            cells.insert(idx, cell1);
                        } else {
                            let old_key = if let Cell::TableInterior { key, .. } = &cells[idx] {
                                *key
                            } else {
                                i64::MAX
                            };
                            let cell1 = Cell::TableInterior {
                                left_child: child_id,
                                key: split_key - 1,
                            };
                            let cell2 = Cell::TableInterior {
                                left_child: new_page,
                                key: old_key,
                            };
                            cells.remove(idx);
                            cells.insert(idx, cell2);
                            cells.insert(idx, cell1);
                        }
                    } else {
                        // right_most case: child_id was right_most.
                        if is_idx_page {
                            let cell1 = Cell::IndexInterior {
                                left_child: child_id,
                                key: split_key_bytes.clone().unwrap_or_default(),
                                rowid: split_key,
                            };
                            cells.push(cell1);
                            right_most = new_page;
                        } else {
                            let cell1 = Cell::TableInterior {
                                left_child: child_id,
                                key: split_key - 1,
                            };
                            cells.push(cell1);
                            right_most = new_page;
                        }
                    }

                    // Does the rewritten page still fit? If not, split this
                    // interior page too and propagate upward.
                    let total_size: usize = cells.iter().map(|c| c.encoded_size() + 2).sum();
                    let page_size = self.pager.page_size() as usize;
                    let header_offset = if page_id == 0 {
                        crate::storage::page::DB_HEADER_SIZE as usize
                    } else {
                        0
                    };
                    let available = page_size - header_offset - PAGE_HEADER_SIZE as usize;
                    if total_size > available {
                        // Split the interior page: keep the left half, move
                        // the right half to a new page, and propagate the
                        // left half's LAST separator upward (the left page
                        // contains entries <= that separator).
                        let total = cells.len();
                        let mid = total / 2;
                        // Separator to propagate = cells[mid-1]'s separator.
                        let (prop_rowid, prop_key_bytes) = if is_idx_page {
                            match &cells[mid - 1] {
                                Cell::IndexInterior { key, rowid, .. } => {
                                    (*rowid, Some(key.clone()))
                                }
                                _ => (cells[mid - 1].key(), None),
                            }
                        } else {
                            // For table pages the propagated value follows the
                            // leaf-split convention: split_key = "first key of
                            // the right page", and the parent applies -1 to
                            // get the left separator. The left half's max
                            // separator is cells[mid-1].key(), so the right
                            // page's minimum entry is that + 1.
                            (cells[mid - 1].key() + 1, None)
                        };

                        let new_interior = self.pager.allocate_page()?;
                        {
                            let p = self.pager.get_page(new_interior)?;
                            if is_idx_page {
                                p.lock().init_interior_index();
                            } else {
                                p.lock().init_interior_table();
                            }
                        }

                        // Rewrite the left page with cells[..mid].
                        {
                            let p = self.pager.get_page(page_id)?;
                            let mut borrowed = p.lock();
                            if is_idx_page {
                                borrowed.init_interior_index();
                            } else {
                                borrowed.init_interior_table();
                            }
                        }
                        for c in &cells[..mid] {
                            self.insert_cell_into_page(page_id, c)?;
                        }
                        // Right page gets cells[mid..] and the old right_most.
                        for c in &cells[mid..] {
                            self.insert_cell_into_page(new_interior, c)?;
                        }
                        self.pager
                            .get_page(new_interior)?
                            .lock()
                            .set_right_most_pointer(right_most);

                        return Ok(InsertResult::Split {
                            new_page: new_interior,
                            split_key: prop_rowid,
                            split_key_bytes: prop_key_bytes,
                        });
                    }

                    // Rewrite the page in place (fits).
                    {
                        let p = self.pager.get_page(page_id)?;
                        let mut borrowed = p.lock();
                        if is_idx_page {
                            borrowed.init_interior_index();
                        } else {
                            borrowed.init_interior_table();
                        }
                        borrowed.set_right_most_pointer(right_most);
                    }
                    for c in &cells {
                        self.insert_cell_into_page(page_id, c)?;
                    }

                    Ok(InsertResult::Done)
                }
            }
        }
    }

    /// Insert a cell into a leaf or interior page. Cells are kept sorted:
    /// table pages by i64 key, index pages by (key bytes, rowid).
    fn insert_cell_into_page(&mut self, page_id: PageId, cell: &Cell) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        let cell_size = cell.encoded_size();
        let n = page.lock().n_cells();
        let is_idx = pt.is_index();

        // Find insertion position by the page's sort order — allocation-free
        // binary search over cell VIEWS. (The old code decoded a full
        // allocating `Cell` — one Vec per probe, ~9 probes per insert —
        // which made page rebuilds after splits O(n) allocations.)
        let pos = {
            let borrowed = page.lock();
            let mut lo: u16 = 0;
            let mut hi: u16 = n;
            if is_idx {
                let nk = cell.index_key();
                let nr = cell.key();
                let interior = pt.is_interior();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], interior)
                    else {
                        return Err(Error::corruption("truncated index cell in search"));
                    };
                    if (v.key, v.rowid) < (nk, nr) {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
            } else {
                let target = cell.key();
                let interior = pt.is_interior();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let sep = if interior {
                        decode_table_interior_key(&borrowed.data[cell_ptr..])
                    } else {
                        varint::decode_signed(&borrowed.data[cell_ptr..]).map(|(k, _)| k)
                    };
                    match sep {
                        Some(k) if k < target => lo = mid + 1,
                        Some(_) => hi = mid,
                        None => break,
                    }
                }
            }
            lo
        };

        // Allocate space at the cell content area.
        let new_content_start = {
            let borrowed = page.lock();
            borrowed.cell_content_start() - cell_size as u32
        };

        // Write the cell bytes.
        {
            let mut borrowed = page.lock();
            let mut buf = Vec::with_capacity(cell_size);
            cell.encode(&mut buf);
            let off = new_content_start as usize;
            borrowed.data[off..off + cell_size].copy_from_slice(&buf);
            borrowed.set_cell_content_start(new_content_start);

            // Shift cell pointers to make room at position `pos` — one
            // memmove (copy_within) instead of a per-element byte-swap
            // loop (O(n) with a tiny constant vs O(n) with ~4 ns/element).
            let header_offset = if page_id == 0 {
                crate::storage::page::DB_HEADER_SIZE as usize
            } else {
                0
            };
            let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
            let pos_usize = pos as usize;
            let n_usize = n as usize;
            borrowed.data.copy_within(
                ptr_array_start + pos_usize * 2..ptr_array_start + n_usize * 2,
                ptr_array_start + (pos_usize + 1) * 2,
            );
            // Insert the new pointer.
            let dst = ptr_array_start + pos_usize * 2;
            borrowed.data[dst..dst + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n + 1);
            borrowed.dirty = true;
        }
        self.pager.note_dirty(page_id);
        Ok(())
    }

    /// Fast mid-split for PACKED pages (no gaps in the content area —
    /// the natural state of pages built by inserts, whatever the byte
    /// order). Instead of decoding every cell into an allocating `Cell`
    /// and re-inserting all of them (~n allocations + n binary searches +
    /// O(n^2) pointer shifting — ~200 us on a full 16 KB page), rewrite
    /// both halves directly from (pointer, size) byte views: one scratch
    /// copy of the old content area + n small byte copies + pointer
    /// writes. ~3-6 us per split, matching SQLite's balance_deeper cost.
    ///
    /// Fragmented pages (gaps left by deletes) fall back to the general
    /// rebuild path, which compacts them as a side effect.
    fn try_fast_mid_split(
        &mut self,
        page_id: PageId,
        pt: PageType,
        new_cell: &Cell,
    ) -> Result<Option<InsertResult>> {
        let page_size = self.pager.page_size() as usize;
        let (n, content_start) = {
            let page = self.pager.get_page(page_id)?;
            let b = page.lock();
            (b.n_cells(), b.cell_content_start() as usize)
        };
        if n < 2 {
            return Ok(None);
        }
        let is_idx = pt.is_index();
        let n_usize = n as usize;

        // Per-cell (ptr, size) + packed check.
        let mut ptrs: Vec<u32> = Vec::with_capacity(n_usize);
        let mut sizes: Vec<u32> = Vec::with_capacity(n_usize);
        let mut total_bytes = 0usize;
        {
            let page = self.pager.get_page(page_id)?;
            let b = page.lock();
            for i in 0..n {
                let p = b.cell_pointer(i) as usize;
                let size = if is_idx {
                    index_leaf_cell_size(&b.data[p..])
                } else {
                    table_leaf_cell_size(&b.data[p..])
                };
                let Some(size) = size else { return Ok(None) };
                if size == 0 || p + size > page_size {
                    return Ok(None);
                }
                ptrs.push(p as u32);
                sizes.push(size as u32);
                total_bytes += size;
            }
        }
        // Packed: every byte in [content_start, page_size) belongs to a
        // cell. (Bytes may be in ANY order — the pointer array is sorted,
        // the content area is insertion-ordered.)
        if total_bytes + content_start != page_size {
            return Ok(None); // fragmented (deleted cells left gaps)
        }

        // Insertion position of the new cell in the sorted order (same
        // comparison semantics as insert_cell_into_page).
        let pos = {
            let page = self.pager.get_page(page_id)?;
            let b = page.lock();
            let mut lo: u16 = 0;
            let mut hi: u16 = n;
            if is_idx {
                let nk = new_cell.index_key();
                let nr = new_cell.key();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let p = b.cell_pointer(mid) as usize;
                    let Some(v) = decode_index_cell(&b.data[p..], false) else {
                        return Ok(None);
                    };
                    if (v.key, v.rowid) < (nk, nr) {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
            } else {
                let target = new_cell.key();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let p = b.cell_pointer(mid) as usize;
                    match varint::decode_signed(&b.data[p..]) {
                        Some((k, _)) if k < target => lo = mid + 1,
                        Some(_) => hi = mid,
                        None => break,
                    }
                }
            }
            lo as usize
        };

        // Merged sequence: existing[0..n) with the new cell at `pos`.
        let mid = (n_usize + 1) / 2;

        // Per-sequence-entry byte source: (page-local offset, size) for
        // existing cells; the encoded new cell is separate.
        let mut new_buf = Vec::with_capacity(new_cell.encoded_size());
        new_cell.encode(&mut new_buf);
        let s_new = new_buf.len();

        // Space checks for both halves (conservative: header + pointers).
        let header_offset = if page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let left_cells = mid;
        let right_cells = n_usize + 1 - mid;
        let left_bytes = {
            let mut b = 0usize;
            for i in 0..mid {
                if i == pos {
                    b += s_new;
                } else {
                    b += sizes[if i > pos { i - 1 } else { i }] as usize;
                }
            }
            if mid == pos {
                b += s_new;
            }
            b
        };
        let avail = page_size - header_offset - PAGE_HEADER_SIZE as usize;
        if left_bytes + left_cells * 2 > avail
            || (total_bytes - left_bytes + s_new * (if pos >= mid { 1 } else { 0 }) + right_cells * 2)
                > avail
        {
            return Ok(None); // giant cell — general path
        }

        // Scratch copy of the old content area (the rewrite overwrites it).
        let scratch: Vec<u8> = {
            let page = self.pager.get_page(page_id)?;
            let b = page.lock();
            b.data[content_start..page_size].to_vec()
        };
        // helper: source slice for sequence index i (existing cell or new)
        let src = |i: usize| -> (&[u8], u32) {
            if i == pos {
                (&new_buf, s_new as u32)
            } else {
                let e = if i > pos { i - 1 } else { i };
                let off = ptrs[e] as usize - content_start;
                (&scratch[off..off + sizes[e] as usize], sizes[e])
            }
        };

        // Allocate + init the new leaf.
        let new_page_id = self.pager.allocate_page()?;
        {
            let np = self.pager.get_page(new_page_id)?;
            let mut npb = np.lock();
            if pt == PageType::LeafIndex {
                npb.init_leaf_index();
            } else {
                npb.init_leaf_table();
            }
        }
        let new_header_offset = if new_page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let new_ptr_array_start = new_header_offset + PAGE_HEADER_SIZE as usize;
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;

        // Rewrite the LEFT page: sequence [0..mid), cells descending from
        // the page end (cell 0 highest).
        {
            let page = self.pager.get_page(page_id)?;
            let mut b = page.lock();
            let mut cur = page_size;
            for i in 0..mid {
                let (bytes, sz) = src(i);
                cur -= sz as usize;
                b.data[cur..cur + sz as usize].copy_from_slice(bytes);
                let v = cur as u16;
                let dst = ptr_array_start + i * 2;
                b.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
            }
            b.set_cell_content_start(cur as u32);
            b.set_n_cells(mid as u16);
            b.dirty = true;
        }
        // Build the RIGHT page: sequence [mid..n+1).
        {
            let np = self.pager.get_page(new_page_id)?;
            let mut nb = np.lock();
            let mut cur = page_size;
            for i in mid..=n_usize {
                let (bytes, sz) = src(i);
                cur -= sz as usize;
                nb.data[cur..cur + sz as usize].copy_from_slice(bytes);
                let v = cur as u16;
                let dst = new_ptr_array_start + (i - mid) * 2;
                nb.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
            }
            nb.set_cell_content_start(cur as u32);
            nb.set_n_cells(right_cells as u16);
            nb.dirty = true;
        }
        self.pager.note_dirty(page_id);
        self.pager.note_dirty(new_page_id);

        // Separator for the parent.
        if is_idx {
            // Exact separator: the left page's last entry = sequence[mid-1].
            let (sep_key, sep_rowid) = {
                let page = self.pager.get_page(page_id)?;
                let b = page.lock();
                let p = b.cell_pointer((mid - 1) as u16) as usize;
                match decode_index_cell(&b.data[p..], false) {
                    Some(v) => (v.key.to_vec(), v.rowid),
                    None => return Ok(None),
                }
            };
            Ok(Some(InsertResult::Split {
                new_page: new_page_id,
                split_key: sep_rowid,
                split_key_bytes: Some(sep_key),
            }))
        } else {
            // split_key = first key of the right page.
            let split_key = {
                let np = self.pager.get_page(new_page_id)?;
                let nb = np.lock();
                let p = nb.cell_pointer(0) as usize;
                match varint::decode_signed(&nb.data[p..]) {
                    Some((k, _)) => k,
                    None => return Ok(None),
                }
            };
            Ok(Some(InsertResult::Split {
                new_page: new_page_id,
                split_key,
                split_key_bytes: None,
            }))
        }
    }

    /// O(1) append-mode split: the old page is left completely untouched
    /// (all its cells stay exactly where they are), and the new page
    /// receives only the incoming cell. The separator for the parent:
    ///   - table trees: the new cell's rowid (first key of the new page)
    ///   - index trees: the old page's LAST entry (the exact separator,
    ///     mirroring the mid-split convention)
    fn append_split(
        &mut self,
        page_id: PageId,
        pt: PageType,
        new_cell: Cell,
    ) -> Result<InsertResult> {
        let new_page_id = self.pager.allocate_page()?;
        let new_page = self.pager.get_page(new_page_id)?;
        {
            let mut np = new_page.lock();
            if pt == PageType::LeafIndex {
                np.init_leaf_index();
            } else {
                np.init_leaf_table();
            }
        }
        // Write ONLY the new cell into the new page.
        self.insert_cell_into_page(new_page_id, &new_cell)?;

        if pt == PageType::LeafIndex {
            // Separator = the old page's last (max) entry, exactly as the
            // mid-split convention: everything <= separator is in the left
            // (old) page.
            let page = self.pager.get_page(page_id)?;
            let n = page.lock().n_cells();
            let (sep_key, sep_rowid) = {
                let borrowed = page.lock();
                let last_ptr = borrowed.cell_pointer(n - 1) as usize;
                match decode_index_cell(&borrowed.data[last_ptr..], false) {
                    Some(v) => (v.key.to_vec(), v.rowid),
                    None => (new_cell.index_key().to_vec(), new_cell.key()),
                }
            };
            Ok(InsertResult::Split {
                new_page: new_page_id,
                split_key: sep_rowid,
                split_key_bytes: Some(sep_key),
            })
        } else {
            // Table separator: first key of the new page = the new rowid.
            Ok(InsertResult::Split {
                new_page: new_page_id,
                split_key: new_cell.key(),
                split_key_bytes: None,
            })
        }
    }

    /// Split a leaf page. Returns the new page ID and the separator info.
    fn split_leaf(&mut self, page_id: PageId, new_cell: Cell) -> Result<InsertResult> {
        // ------------------------------------------------------------------------
        // APPEND-MODE SPLIT — O(1) fast path.
        //
        // When the new cell sorts strictly AFTER every existing cell (bulk
        // loads, auto-increment PKs, any monotonically increasing key), the
        // split doesn't need to move ANY existing cells: the old page keeps
        // everything, and the new page receives ONLY the new cell.
        //
        // The previous implementation decoded every cell on the page into an
        // allocating `Cell` (~560 Vec allocations for a full 16 KB leaf),
        // cleared the page, and re-inserted all of them — ~200 us per split,
        // ~20 splits on a 10k-row bulk load = ~4 ms of pure split overhead.
        // This path makes a split O(1): one page allocation + one cell write.
        // ------------------------------------------------------------------------
        {
            let page = self.pager.get_page(page_id)?;
            let pt = page.lock().page_type()?;
            let is_idx_split = pt.is_index();
            let n_existing = page.lock().n_cells();
            let is_append_split = if n_existing == 0 {
                false // empty page can't be "full"; let the normal path handle it
            } else {
                let borrowed = page.lock();
                let last_ptr = borrowed.cell_pointer(n_existing - 1) as usize;
                if is_idx_split {
                    if let Some(last) = decode_index_cell(&borrowed.data[last_ptr..], false) {
                        let nk = new_cell.index_key();
                        nk > last.key
                            || (nk == last.key && new_cell.key() > last.rowid)
                    } else {
                        false
                    }
                } else {
                    if let Some((last_rowid, _)) =
                        varint::decode_signed(&borrowed.data[last_ptr..])
                    {
                        new_cell.key() > last_rowid
                    } else {
                        false
                    }
                }
            };
            if is_append_split {
                return self.append_split(page_id, pt, new_cell);
            }
            // Mid split on a PACKED page: byte-view rewrite instead of the
            // decode-all / reinsert-all rebuild (~200 us -> ~5 us).
            if let Some(res) = self.try_fast_mid_split(page_id, pt, &new_cell)? {
                return Ok(res);
            }
        }

        // Capture the new cell's identity before the merge below moves it.
        let new_cell_key_rowid = new_cell.key();
        let new_cell_key: Option<Vec<u8>> = if matches!(
            new_cell,
            Cell::IndexLeaf { .. } | Cell::IndexInterior { .. }
        ) {
            Some(new_cell.index_key().to_vec())
        } else {
            None
        };
        // Read all existing cells + the new one, merged in sort order.
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        let is_idx = pt.is_index();
        let n = page.lock().n_cells();
        let mut cells: Vec<Cell> = Vec::with_capacity(n as usize + 1);
        for i in 0..n {
            let borrowed = page.lock();
            let cell_ptr = borrowed.cell_pointer(i) as usize;
            let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
            let existing_before_new = if is_idx {
                c.cmp_index_target(new_cell.index_key(), new_cell.key())
                    == std::cmp::Ordering::Less
            } else {
                c.key() < new_cell.key()
            };
            if !existing_before_new {
                drop(borrowed);
                cells.push(new_cell.clone());
                // Continue reading remaining cells.
                for j in i..n {
                    let borrowed = page.lock();
                    let cell_ptr = borrowed.cell_pointer(j) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    cells.push(c);
                }
                break;
            }
            cells.push(c);
        }
        if cells.len() == n as usize {
            cells.push(new_cell);
        }
        drop(page);

        let total = cells.len();
        // ---- Split-point selection ----
        //
        // Mid split (default): the classic B-tree 50/50 split — best for
        // random inserts, keeps both siblings half-full so either can
        // absorb future inserts.
        //
        // APPEND-MODE split: when the new cell sorts AFTER every existing
        // cell (a right-edge append — bulk loads, auto-increment
        // INTEGER PRIMARY KEY, any monotonically increasing key), keep ALL
        // existing cells in the old page and give the new page ONLY the
        // new cell. Sequential inserts then fill pages to ~100% instead of
        // leaving every left sibling frozen at 50% forever — for the 10k-row
        // insert benchmark this halves the file size (the left-behind
        // half-pages were the dominant on-disk waste). Mirrors SQLite's
        // `balance_quick()` right-edge optimization.
        //
        // Detecting the append HERE (by comparing against the last cell)
        // rather than threading a flag through the recursion also covers
        // generic-path inserts that happen to land on the right edge.
        let is_append = {
            // cells was built by merging; the new cell is last iff its key
            // is strictly greater than the previous last cell's. If the
            // merge never hit the early-break, the new cell was pushed at
            // the very end (see `cells.len() == n` above).
            let new_is_last = cells.last().map(|c| {
                if is_idx {
                    c.index_key() == new_cell_key.as_ref().map(|k| k.as_slice()).unwrap_or(&[])
                        && c.key() == new_cell_key_rowid
                } else {
                    c.key() == new_cell_key_rowid
                }
            }).unwrap_or(false);
            // A true append also requires the cell before it to be an
            // EXISTING cell (i.e. the new cell is strictly after all n
            // originals). If the new cell equaled the last original's key
            // the merge would have placed it after (cmp not Less), but that
            // is a duplicate-key index insert, not an append — treat only
            // strictly-after as append-mode.
            new_is_last && {
                let orig_last_before: Option<(Vec<u8>, i64)> = if n > 0 {
                    let page_ref = self.pager.get_page(page_id)?;
                    let borrowed = page_ref.lock();
                    let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    Some((c.index_key().to_vec(), c.key()))
                } else {
                    None
                };
                match orig_last_before {
                    None => true, // empty leaf: everything is an "append"
                    Some((k, r)) => {
                        if is_idx {
                            // strictly greater than the original last entry
                            let ord = new_cell_key
                                .as_ref()
                                .map(|nk| nk.as_slice().cmp(&k))
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then(new_cell_key_rowid.cmp(&r));
                            ord == std::cmp::Ordering::Greater
                        } else {
                            new_cell_key_rowid > r
                        }
                    }
                }
            }
        };
        let mid = if is_append && total > 1 { total - 1 } else { total / 2 };

        // Allocate a new leaf page.
        let new_page_id = self.pager.allocate_page()?;
        let new_page = self.pager.get_page(new_page_id)?;
        // Preserve the leaf page type — index splits must produce LeafIndex
        // pages, table splits must produce LeafTable pages. Previously this
        // was hardcoded to `init_leaf_table()`, which silently corrupted
        // index B+trees on the first split (turning a LeafIndex page into a
        // LeafTable page; subsequent scan_index calls panic with
        // "unexpected page type in index scan: LeafTable").
        let is_index = matches!(pt, PageType::LeafIndex);
        if is_index {
            new_page.lock().init_leaf_index();
        } else {
            new_page.lock().init_leaf_table();
        }
        // Note: new_page_id is already in dirty_pages via allocate_page.

        // Clear the old page and re-insert the first half.
        {
            let page_ref = self.pager.get_page(page_id)?;
            let mut borrowed = page_ref.lock();
            if is_index {
                borrowed.init_leaf_index();
            } else {
                borrowed.init_leaf_table();
            }
        }
        // init_leaf_table/index sets dirty=true directly; track it in the
        // dirty_pages set so flush() will write it back.
        self.pager.note_dirty(page_id);

        // Re-insert first half into old page, second half into new page.
        for c in &cells[..mid] {
            self.insert_cell_into_page(page_id, c)?;
        }
        for c in &cells[mid..] {
            self.insert_cell_into_page(new_page_id, c)?;
        }

        if is_idx {
            // For index splits, return the EXACT separator: the last entry
            // of the left page. The parent uses it verbatim as the left
            // child's separator.
            let left_max = &cells[mid - 1];
            Ok(InsertResult::Split {
                new_page: new_page_id,
                split_key: left_max.key(),
                split_key_bytes: Some(left_max.index_key().to_vec()),
            })
        } else {
            // The split key is the FIRST key of the new page (the min key in
            // the second half). The parent applies the -1 gap convention.
            let split_key = cells[mid].key();
            Ok(InsertResult::Split {
                new_page: new_page_id,
                split_key,
                split_key_bytes: None,
            })
        }
    }

    /// Split an interior page. Same idea but the middle cell moves up.
    // fn split_interior(&mut self, page_id: PageId, new_cell: Cell) -> Result<InsertResult> {
    //     let page = self.pager.get_page(page_id)?;
    //     let pt = page.lock().page_type()?;
    //     let n = page.lock().n_cells();
    //     let right = page.lock().right_most_pointer();

    //     let mut cells: Vec<Cell> = Vec::with_capacity(n as usize + 1);
    //     let mut inserted = false;
    //     for i in 0..n {
    //         let borrowed = page.lock();
    //         let cell_ptr = borrowed.cell_pointer(i) as usize;
    //         let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
    //         if !inserted && new_cell.key() < c.key() {
    //             cells.push(new_cell.clone());
    //             inserted = true;
    //         }
    //         cells.push(c);
    //     }
    //     if !inserted {
    //         cells.push(new_cell);
    //     }
    //     drop(page);

    //     let total = cells.len();
    //     let mid = total / 2;
    //     let split_cell = cells[mid].clone();
    //     let split_key = split_cell.key();

    //     let new_page_id = self.pager.allocate_page()?;
    //     let new_page = self.pager.get_page(new_page_id)?;
    //     new_page.lock().init_interior_table();

    //     // Clear old page.
    //     {
    //         let page_ref = self.pager.get_page(page_id)?;
    //         let mut borrowed = page_ref.lock();
    //         borrowed.init_interior_table();
    //         // Right pointer of old page becomes the left child of the split cell.
    //         if let Cell::TableInterior { left_child, .. } = &split_cell {
    //             borrowed.set_right_most_pointer(*left_child);
    //         }
    //     }

    //     // Insert first half into old page.
    //     for c in &cells[..mid] {
    //         self.insert_cell_into_page(page_id, c)?;
    //     }
    //     // Insert second half (after mid) into new page.
    //     for c in &cells[mid + 1..] {
    //         self.insert_cell_into_page(new_page_id, c)?;
    //     }
    //     // Right pointer of new page is the original right pointer.
    //     self.pager.get_page(new_page_id)?.lock().set_right_most_pointer(right);

    //     Ok(InsertResult::Split { new_page: new_page_id, split_key })
    // }

    /// If `child_id` is a LEAF page with zero cells, unlink it from its
    /// parent `parent_id` and push it onto the pager freelist.
    ///
    /// Unlinking rules (parent is an interior page):
    /// - Child referenced by a separator cell `(child, sep)`: remove that
    ///   cell. The child's now-empty key range is covered by the next
    ///   sibling (routing falls through to it, and inserts binary-search
    ///   into the correct position).
    /// - Child is the rightmost: the LAST separator cell `(prev, sep)`
    ///   becomes the new rightmost child (remove the cell, set
    ///   right_most = prev). If the parent has no cells, the empty child
    ///   is its only child — leave it (a 0-cell interior with a rightmost
    ///   pointer is valid and traversable).
    ///
    /// Interior children are never recycled (a 0-cell interior with a
    /// rightmost pointer is left in place — one page, bounded waste, and
    /// collapsing it requires full rebalancing).
    fn maybe_recycle_empty_child(&mut self, parent_id: PageId, child_id: PageId) -> Result<()> {
        if child_id == 0 || child_id == self.root {
            return Ok(());
        }
        // Child must be an EMPTY leaf.
        {
            let child_ref = self.pager.get_page(child_id)?;
            let borrowed = child_ref.lock();
            match borrowed.page_type()? {
                PageType::LeafTable | PageType::LeafIndex => {
                    if borrowed.n_cells() != 0 {
                        return Ok(());
                    }
                }
                _ => return Ok(()), // interior child — don't recycle
            }
        }
        // Scan the parent for the cell referencing child_id.
        let (n_cells, pt, found_cell_idx) = {
            let parent_ref = self.pager.get_page(parent_id)?;
            let borrowed = parent_ref.lock();
            let n = borrowed.n_cells();
            let pt = borrowed.page_type()?;
            let mut found = None;
            for i in 0..n {
                let cell_ptr = borrowed.cell_pointer(i) as usize;
                let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                if c.left_child() == child_id {
                    found = Some(i as usize);
                    break;
                }
            }
            (n, pt, found)
        };
        let header_offset = if parent_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
        match found_cell_idx {
            Some(idx) => {
                // Remove the separator cell slot.
                let parent_ref = self.pager.get_page(parent_id)?;
                let mut borrowed = parent_ref.lock();
                let n_usize = n_cells as usize;
                for i in idx..n_usize - 1 {
                    let src = ptr_array_start + (i + 1) * 2;
                    let dst = ptr_array_start + i * 2;
                    let v = u16::from_be_bytes(borrowed.data[src..src + 2].try_into().unwrap());
                    borrowed.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
                }
                borrowed.set_n_cells(n_cells - 1);
                borrowed.dirty = true;
            }
            None => {
                // Child is the rightmost. The last cell's left_child becomes
                // the new rightmost.
                if n_cells == 0 {
                    return Ok(()); // only child — keep the empty leaf
                }
                let last_idx = n_cells as usize - 1;
                let new_rightmost = {
                    let parent_ref = self.pager.get_page(parent_id)?;
                    let borrowed = parent_ref.lock();
                    let cell_ptr = borrowed.cell_pointer(last_idx as u16) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    c.left_child()
                };
                let parent_ref = self.pager.get_page(parent_id)?;
                let mut borrowed = parent_ref.lock();
                borrowed.set_right_most_pointer(new_rightmost);
                // Remove the last cell slot (no shift needed — it's the tail).
                borrowed.set_n_cells(n_cells - 1);
                borrowed.dirty = true;
            }
        }
        self.pager.note_dirty(parent_id);
        // Free the empty leaf — the pager zeroes it and links it onto the
        // freelist; the next allocate_page pops it instead of growing the
        // file.
        self.pager.free_page(child_id)?;
        Ok(())
    }

    /// Delete a (rowid) from a table B+tree. Does not rebalance (we leave
    /// pages underfull rather than risk concurrent-merge bugs).
    pub fn delete_table(&mut self, rowid: i64) -> Result<bool> {
        // Notify the pager that a write is about to happen.
        self.pager.note_write();
        self.delete_from_page(self.root, rowid)
    }

    /// Delete a rowid from a TABLE B+tree and return the deleted cell's
    /// payload. Used by the DELETE fast path: the executor needs the old
    /// row bytes for index maintenance / RETURNING, and this avoids a
    /// separate `lookup_table` descent. Returns `Ok(None)` when the rowid
    /// doesn't exist.
    pub fn delete_table_get_payload(&mut self, rowid: i64) -> Result<Option<Vec<u8>>> {
        self.pager.note_write();
        // Find the leaf and capture the payload before removing the cell.
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            let pt = page.lock().page_type()?;
            match pt {
                PageType::LeafTable => {
                    let payload = {
                        let borrowed = page.lock();
                        let n = borrowed.n_cells() as usize;
                        let mut lo = 0usize;
                        let mut hi = n;
                        let mut found: Option<usize> = None;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                            let (cell_rowid, _) = decode_rowid_only(&borrowed.data[cell_ptr..])
                                .ok_or_else(|| Error::corruption("truncated leaf rowid in delete"))?;
                            match cell_rowid.cmp(&rowid) {
                                std::cmp::Ordering::Equal => {
                                    found = Some(cell_ptr);
                                    break;
                                }
                                std::cmp::Ordering::Less => lo = mid + 1,
                                std::cmp::Ordering::Greater => hi = mid,
                            }
                        }
                        match found {
                            None => None,
                            Some(cell_ptr) => {
                                // Decode the payload length + body.
                                let plen_pos = {
                                    let (_, n_rid) = decode_rowid_only(&borrowed.data[cell_ptr..])
                                        .ok_or_else(|| Error::corruption("truncated rowid"))?;
                                    cell_ptr + n_rid
                                };
                                let (plen, n_plen) = varint::decode(&borrowed.data[plen_pos..])
                                    .ok_or_else(|| Error::corruption("truncated payload length"))?;
                                let start = plen_pos + n_plen;
                                Some(borrowed.data[start..start + plen as usize].to_vec())
                            }
                        }
                    };
                    if payload.is_none() {
                        return Ok(None);
                    }
                    drop(page);
                    let deleted = self.delete_from_page(page_id, rowid)?;
                    debug_assert!(deleted, "binary search found the cell but delete_from_page didn't");
                    return Ok(payload);
                }
                PageType::InteriorTable => {
                    // Binary search over view cells (was: linear scan with
                    // an allocating Cell::decode per cell).
                    let next = {
                        let borrowed = page.lock();
                        let n = borrowed.n_cells();
                        let mut lo: u16 = 0;
                        let mut hi: u16 = n;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid) as usize;
                            let sep = decode_table_interior_key(&borrowed.data[cell_ptr..])
                                .ok_or_else(|| Error::corruption("truncated interior cell"))?;
                            if rowid <= sep {
                                hi = mid;
                            } else {
                                lo = mid + 1;
                            }
                        }
                        if lo >= n {
                            borrowed.right_most_pointer()
                        } else {
                            let cell_ptr = borrowed.cell_pointer(lo) as usize;
                            decode_table_interior_child(&borrowed.data[cell_ptr..])
                                .unwrap_or_else(|| borrowed.right_most_pointer())
                        }
                    };
                    drop(page);
                    page_id = next;
                }
                _ => {
                    return Err(Error::corruption(format!(
                        "unexpected page type in delete: {:?}",
                        pt
                    )))
                }
            }
        }
    }

    fn delete_from_page(&mut self, page_id: PageId, rowid: i64) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable | PageType::LeafIndex => {
                let n = page.lock().n_cells();
                // Allocation-free binary search over cell views (the old
                // code decoded an allocating Cell per probe).
                let pos = {
                    let borrowed = page.lock();
                    let mut lo = 0;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let k = if pt == PageType::LeafIndex {
                            match decode_index_cell(&borrowed.data[cell_ptr..], false) {
                                Some(v) => v.rowid,
                                None => return Err(Error::corruption("truncated index cell")),
                            }
                        } else {
                            match varint::decode_signed(&borrowed.data[cell_ptr..]) {
                                Some((k, _)) => k,
                                None => return Err(Error::corruption("truncated rowid")),
                            }
                        };
                        if k < rowid {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    lo
                };
                if pos >= n {
                    return Ok(false);
                }
                // Verify the cell at pos has the right key.
                let key_matches = {
                    let borrowed = page.lock();
                    let cell_ptr = borrowed.cell_pointer(pos) as usize;
                    if pt == PageType::LeafIndex {
                        decode_index_cell(&borrowed.data[cell_ptr..], false)
                            .map(|v| v.rowid == rowid)
                            .unwrap_or(false)
                    } else {
                        varint::decode_signed(&borrowed.data[cell_ptr..])
                            .map(|(k, _)| k == rowid)
                            .unwrap_or(false)
                    }
                };
                if !key_matches {
                    return Ok(false);
                }
                // Shift cell pointers left to remove the slot at `pos` —
                // one memmove (copy_within) instead of a per-element loop.
                let header_offset = if page_id == 0 {
                    crate::storage::page::DB_HEADER_SIZE as usize
                } else {
                    0
                };
                let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
                {
                    let mut borrowed = page.lock();
                    let pos_usize = pos as usize;
                    let n_usize = n as usize;
                    borrowed.data.copy_within(
                        ptr_array_start + (pos_usize + 1) * 2..ptr_array_start + n_usize * 2,
                        ptr_array_start + pos_usize * 2,
                    );
                    borrowed.set_n_cells(n - 1);
                    borrowed.dirty = true;
                }
                self.pager.note_dirty(page_id);
                Ok(true)
            }
            PageType::InteriorTable | PageType::InteriorIndex => {
                // Binary search over cell views (was: linear scan decoding
                // an allocating Cell per cell). Convention: cell
                // (left_child, sep) means left_child holds keys <= sep.
                let child_id = {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let mut lo: u16 = 0;
                    let mut hi: u16 = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let (sep, child) = if pt == PageType::InteriorIndex {
                            match decode_index_cell(&borrowed.data[cell_ptr..], true) {
                                Some(v) => (v.rowid, v.left_child),
                                None => return Err(Error::corruption("truncated index interior cell")),
                            }
                        } else {
                            match decode_table_interior_key(&borrowed.data[cell_ptr..]) {
                                Some(k) => {
                                    let ch = decode_table_interior_child(&borrowed.data[cell_ptr..])
                                        .unwrap_or(0);
                                    (k, ch)
                                }
                                None => return Err(Error::corruption("truncated interior cell")),
                            }
                        };
                        if rowid <= sep {
                            hi = mid;
                        } else {
                            lo = mid + 1;
                        }
                    }
                    if lo >= n {
                        borrowed.right_most_pointer()
                    } else {
                        let cell_ptr = borrowed.cell_pointer(lo) as usize;
                        if pt == PageType::InteriorIndex {
                            decode_index_cell(&borrowed.data[cell_ptr..], true)
                                .map(|v| v.left_child)
                                .unwrap_or_else(|| borrowed.right_most_pointer())
                        } else {
                            decode_table_interior_child(&borrowed.data[cell_ptr..])
                                .unwrap_or_else(|| borrowed.right_most_pointer())
                        }
                    }
                };
                drop(page);
                let deleted = self.delete_from_page(child_id, rowid)?;
                if deleted {
                    // A leaf that just became empty is unlinked from this
                    // interior page and pushed onto the pager freelist, so
                    // future inserts reuse it instead of growing the file
                    // (mirrors SQLite's freelist; without it, delete-heavy
                    // churn grows the file forever).
                    self.maybe_recycle_empty_child(page_id, child_id)?;
                }
                Ok(deleted)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in delete: {:?}",
                pt
            ))),
        }
    }

    /// Delete a cell from an interior page by index (used during B+tree
    /// maintenance when a child splits and we need to replace a cell).
    // fn delete_cell_from_interior(&mut self, page_id: PageId, idx: u16) -> Result<()> {
    //     let page = self.pager.get_page(page_id)?;
    //     let pt = page.lock().page_type()?;
    //     let n = page.lock().n_cells();
    //     if idx >= n {
    //         return Ok(());
    //     }
    //     let header_offset = if page_id == 0 {
    //         crate::storage::page::DB_HEADER_SIZE as usize
    //     } else {
    //         0
    //     };
    //     let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
    //     {
    //         let mut borrowed = page.lock();
    //         let pos_usize = idx as usize;
    //         let n_usize = n as usize;
    //         for i in pos_usize..n_usize - 1 {
    //             let src = ptr_array_start + (i + 1) * 2;
    //             let dst = ptr_array_start + i * 2;
    //             let v = u16::from_be_bytes(borrowed.data[src..src + 2].try_into().unwrap());
    //             borrowed.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
    //         }
    //         borrowed.set_n_cells(n - 1);
    //         borrowed.dirty = true;
    //     }
    //     let _ = pt;
    //     Ok(())
    // }

    /// Scan all rows in a table B+tree, calling `f(rowid, payload)` for each.
    /// Stops early if `f` returns false.
    pub fn scan_table<F: FnMut(i64, &[u8]) -> bool>(&mut self, mut f: F) -> Result<()> {
        self.scan_subtree(self.root, &mut f)
    }

    /// Zero-allocation table scan: callback receives `(rowid, payload_bytes)`
    /// where `payload_bytes` is a BORROW into the cached page's data buffer.
    ///
    /// This bypasses `Cell::decode` which would allocate a fresh `Vec<u8>`
    /// per row to copy the payload out. For a 10k-row scan, that's 10k
    /// malloc+free pairs saved (~500μs on a hot CPU).
    ///
    /// The catch: the borrow is tied to the page lock. We hold the Mutex
    /// guard for the whole leaf iteration, so the callback must NOT call
    /// back into the pager (no `get_page`, no `allocate_page`). For pure
    /// decode/transform callbacks this is fine.
    pub fn scan_table_borrowed<F: FnMut(i64, &[u8]) -> bool>(&mut self, mut f: F) -> Result<()> {
        self.scan_subtree_borrowed(self.root, &mut f)
    }

    fn scan_subtree_borrowed<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
                // Single lock for the whole leaf iteration. Inside the
                // lock we read cell pointers and slice payload bytes
                // directly into the page's data buffer — no allocation.
                let borrowed = page.lock();
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    // Decode rowid varint + payload length varint, then
                    // slice the payload bytes — all without allocating.
                    let buf = &borrowed.data[cell_ptr..];
                    let (rowid, n1) = varint::decode_signed(buf)
                        .ok_or_else(|| Error::corruption("truncated leaf rowid in scan_borrowed"))?;
                    let rest = &buf[n1..];
                    let (plen, n2) = varint::decode(rest)
                        .ok_or_else(|| Error::corruption("truncated leaf payload len in scan_borrowed"))?;
                    let payload_start = n1 + n2;
                    let plen = plen as usize;
                    if payload_start + plen > buf.len() {
                        return Err(Error::corruption("truncated leaf payload in scan_borrowed"));
                    }
                    let payload = &buf[payload_start..payload_start + plen];
                    if !f(rowid, payload) {
                        return Ok(());
                    }
                }
                Ok(())
            }
            PageType::InteriorTable => {
                let n = page.lock().n_cells();
                let right = page.lock().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    v.push(right);
                    v
                };
                drop(page);
                for child in cells {
                    self.scan_subtree_borrowed(child, f)?;
                }
                Ok(())
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in scan_borrowed: {:?}",
                pt
            ))),
        }
    }

    /// Count all rows in a table B+tree WITHOUT decoding any cell payloads.
    /// Much faster than `scan_table` + count — for `SELECT COUNT(*) FROM t`
    /// this skips the per-row decode_row_into overhead (which dominates
    /// the scan cost for wide rows). Returns the total number of leaf
    /// table cells, which is the row count for a table B+tree.
    pub fn count_rows(&mut self) -> Result<u64> {
        self.count_subtree(self.root)
    }

    fn count_subtree(&mut self, page_id: PageId) -> Result<u64> {
        let page = self.pager.get_page(page_id)?;
        let (pt, n, right) = {
            let borrowed = page.lock();
            (borrowed.page_type()?, borrowed.n_cells(), borrowed.right_most_pointer())
        };
        match pt {
            PageType::LeafTable => Ok(n as u64),
            PageType::InteriorTable => {
                let mut total: u64 = 0;
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    v.push(right);
                    v
                };
                drop(page);
                for child in cells {
                    total += self.count_subtree(child)?;
                }
                Ok(total)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in count: {:?}",
                pt
            ))),
        }
    }

    fn scan_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        // ONE lock per page (was: one lock for page_type, one for n_cells,
        // then PER CELL: a lock for cell_pointer + another for the data —
        // ~2,000 lock/unlock pairs on a 1,000-cell leaf).
        let borrowed = page.lock();
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    if let Cell::TableLeaf { rowid, payload } = cell {
                        if !f(rowid, &payload) {
                            return Ok(());
                        }
                    }
                }
                Ok(())
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                let cells: Vec<PageId> = {
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    v.push(right);
                    v
                };
                drop(borrowed);
                drop(page);
                for child in cells {
                    self.scan_subtree(child, f)?;
                }
                Ok(())
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in scan: {:?}",
                pt
            ))),
        }
    }

    /// Scan a range of rowids [start, end] (inclusive).
    pub fn scan_table_range<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        start: i64,
        end: i64,
        mut f: F,
    ) -> Result<()> {
        self.scan_range_subtree(self.root, start, end, &mut f)
    }

    /// Zero-allocation range scan: like `scan_table_range` but bypasses
    /// `Cell::decode`'s per-row payload allocation by passing `&[u8]`
    /// borrows directly into the page buffer. Used by `exec_rowid_range`
    /// to speed up `WHERE id BETWEEN ? AND ?` queries.
    pub fn scan_table_range_borrowed<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        start: i64,
        end: i64,
        mut f: F,
    ) -> Result<()> {
        self.scan_range_subtree_borrowed(self.root, start, end, &mut f)
    }

    fn scan_range_subtree_borrowed<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        // ONE lock per page: type check + leaf work under the same guard.
        let borrowed = page.lock();
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    let buf = &borrowed.data[cell_ptr..];
                    let (rowid, n1) = varint::decode_signed(buf)
                        .ok_or_else(|| Error::corruption("truncated leaf rowid in range_borrowed"))?;
                    if rowid > end {
                        return Ok(());
                    }
                    if rowid >= start {
                        let rest = &buf[n1..];
                        let (plen, n2) = varint::decode(rest)
                            .ok_or_else(|| Error::corruption("truncated payload len in range_borrowed"))?;
                        let payload_start = n1 + n2;
                        let plen_usize = plen as usize;
                        if payload_start + plen_usize > buf.len() {
                            return Err(Error::corruption("truncated payload in range_borrowed"));
                        }
                        let payload = &buf[payload_start..payload_start + plen_usize];
                        if !f(rowid, payload) {
                            return Ok(());
                        }
                    }
                }
                Ok(())
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                let cells: Vec<(PageId, i64)> = {
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, key } = c {
                            v.push((left_child, key));
                        }
                    }
                    v.push((right, i64::MAX));
                    v
                };
                drop(borrowed);
                drop(page);
                for (child, key) in cells {
                    if key < start {
                        continue;
                    }
                    self.scan_range_subtree_borrowed(child, start, end, f)?;
                }
                Ok(())
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in range scan: {:?}",
                pt
            ))),
        }
    }

    fn scan_range_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = page.lock().n_cells();
                for i in 0..n {
                    let cell_ptr = page.lock().cell_pointer(i) as usize;
                    let borrowed = page.lock();
                    let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    if let Cell::TableLeaf { rowid, payload } = cell {
                        if rowid > end {
                            return Ok(());
                        }
                        if rowid >= start {
                            if !f(rowid, &payload) {
                                return Ok(());
                            }
                        }
                    }
                }
                Ok(())
            }
            PageType::InteriorTable => {
                let n = page.lock().n_cells();
                let right = page.lock().right_most_pointer();
                let cells: Vec<(PageId, i64)> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, key } = c {
                            v.push((left_child, key));
                        }
                    }
                    v.push((right, i64::MAX));
                    v
                };
                drop(page);
                for (child, key) in cells {
                    // Skip children whose entire range is before `start`.
                    // We can't know the min key of a child without reading it, so we
                    // descend conservatively: skip only if `key < start`.
                    if key < start {
                        continue;
                    }
                    self.scan_range_subtree(child, start, end, f)?;
                }
                Ok(())
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in range scan: {:?}",
                pt
            ))),
        }
    }

    // ========================================================================
    // Index B+tree operations
    // ========================================================================
    //
    // Index B+trees store (key, rowid) pairs where `key` is the encoded
    // concatenation of indexed column values. We use the rowid as the
    // B+tree key for ordering (so multiple rows with the same indexed value
    // are stored together, sorted by rowid).

    /// Insert a (key, rowid) pair into an index B+tree.
    /// The key is the encoded form of the indexed column value(s).
    pub fn insert_index(&mut self, key: &[u8], rowid: i64) -> Result<()> {
        // Notify the pager that a write is about to happen.
        self.pager.note_write();
        // Index entries are sorted by (key, rowid): the cell's btree order
        // is the byte-encoded key first, then the rowid as tiebreaker.
        let cell = Cell::IndexLeaf {
            key: key.to_vec(),
            rowid,
        };
        match self.insert_into_page(self.root, cell)? {
            InsertResult::Done => Ok(()),
            InsertResult::Split {
                new_page,
                split_key,
                split_key_bytes,
            } => {
                let old_root = self.root;
                let new_root = self.pager.allocate_page()?;
                {
                    let page_ref = self.pager.get_page(new_root)?;
                    let mut page = page_ref.lock();
                    page.init_interior_index();
                    page.set_right_most_pointer(new_page);
                }
                // Cell (old_root, sep) where sep = the EXACT max entry of
                // the old root after the split: (split_key_bytes, split_key).
                let cell = Cell::IndexInterior {
                    left_child: old_root,
                    key: split_key_bytes.unwrap_or_default(),
                    rowid: split_key,
                };
                self.insert_cell_into_page(new_root, &cell)?;
                self.root = new_root;
                Ok(())
            }
        }
    }

    /// Delete a (key, rowid) pair from an index B+tree.
    /// The key is required because index pages are sorted by (key, rowid).
    pub fn delete_index(&mut self, key: &[u8], rowid: i64) -> Result<bool> {
        // Notify the pager that a write is about to happen.
        self.pager.note_write();
        self.delete_index_from_page(self.root, key, rowid)
    }

    /// Recursive delete by exact (key, rowid) on index pages.
    fn delete_index_from_page(&mut self, page_id: PageId, key: &[u8], rowid: i64) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafIndex => {
                let n = page.lock().n_cells();
                // Binary search for the first cell >= (key, rowid), using
                // allocation-free cell views.
                let pos = {
                    let borrowed = page.lock();
                    let mut lo = 0;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], false) else {
                            return Err(Error::corruption("truncated index leaf cell"));
                        };
                        if (v.key, v.rowid) < (key, rowid) {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    lo
                };
                if pos >= n {
                    return Ok(false);
                }
                let key_matches = {
                    let borrowed = page.lock();
                    let cell_ptr = borrowed.cell_pointer(pos) as usize;
                    match decode_index_cell(&borrowed.data[cell_ptr..], false) {
                        Some(v) => v.key == key && v.rowid == rowid,
                        None => false,
                    }
                };
                if !key_matches {
                    return Ok(false);
                }
                drop(page);
                self.remove_cell_at(page_id, pos, n)
            }
            PageType::InteriorIndex => {
                // Binary-search the first separator >= (key, rowid) and
                // descend into its left child (right_most if none). The
                // previous code linearly decoded every interior cell — a
                // Vec allocation per cell per level.
                let child_id = {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let data = &borrowed.data;
                    let cp = |i: u16| borrowed.cell_pointer(i);
                    let (_, child) = find_index_child(data, n, cp, borrowed.right_most_pointer(), key, rowid);
                    child
                };
                drop(page);
                let deleted = self.delete_index_from_page(child_id, key, rowid)?;
                if deleted {
                    self.maybe_recycle_empty_child(page_id, child_id)?;
                }
                Ok(deleted)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in index delete: {:?}",
                pt
            ))),
        }
    }

    /// Remove the cell at index `pos` from a leaf page (pointer shift only).
    fn remove_cell_at(&mut self, page_id: PageId, pos: u16, n: u16) -> Result<bool> {
        let header_offset = if page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
        {
            let page = self.pager.get_page(page_id)?;
            let mut borrowed = page.lock();
            let pos_usize = pos as usize;
            let n_usize = n as usize;
            // One memmove instead of a per-element byte-swap loop.
            borrowed.data.copy_within(
                ptr_array_start + (pos_usize + 1) * 2..ptr_array_start + n_usize * 2,
                ptr_array_start + pos_usize * 2,
            );
            borrowed.set_n_cells(n - 1);
            borrowed.dirty = true;
        }
        self.pager.note_dirty(page_id);
        Ok(true)
    }

    /// Look up all rowids matching a given key in an index B+tree.
    /// Returns a list of rowids (usually 1, but may be more for non-unique indexes).
    ///
    /// **Prefix matching**: when the search `key` is SHORTER than the stored
    /// index key (i.e., a composite index lookup where only the leading
    /// columns are constrained), we treat it as a prefix match. This is what
    /// makes `WHERE a = 1` use the index (a, b) correctly: the stored keys
    /// are `encode(a) || encode(b)` and the search key is just `encode(a)`.
    ///
    /// Index pages are sorted by (key, rowid), so this is an O(log N) seek
    /// followed by a forward scan over the matching prefix (which may span
    /// multiple leaves). Previously this was a full O(N) scan of every
    /// index page — the main reason indexed point lookups lagged SQLite.
    pub fn lookup_index(&mut self, key: &[u8]) -> Result<Vec<i64>> {
        let mut results = Vec::new();
        self.scan_index_from(key, |cell_rowid, cell_key| {
            if cell_key.starts_with(key) {
                results.push(cell_rowid);
                true // keep scanning (duplicates / composite prefix)
            } else {
                false // past the prefix range — stop
            }
        })?;
        Ok(results)
    }

    /// Scan index entries in (key, rowid) order, starting at the first entry
    /// whose key is >= `start_key`. Calls `f(rowid, key)` for each; stops
    /// early when `f` returns false. Left subtrees entirely below the start
    /// key are pruned via interior-page binary search.
    pub fn scan_index_from<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        start_key: &[u8],
        f: F,
    ) -> Result<()> {
        let mut f = f;
        self.scan_index_range_subtree(self.root, start_key, &mut f, &mut false)?;
        Ok(())
    }

    /// Recursive range scan. `started` tracks whether we've passed the start
    /// key yet (once true, every entry is visited).
    ///
    /// Returns `Ok(true)` to keep scanning siblings, `Ok(false)` when the
    /// callback stopped the scan ( propagated up so the interior loop
    /// doesn't keep binary-searching leaves past the stop point — a point
    /// lookup previously visited EVERY leaf to the right of the match,
    /// costing ~5 µs per lookup on a 10k-entry index).
    fn scan_index_range_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start_key: &[u8],
        f: &mut F,
        started: &mut bool,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        // ONE lock + ONE page-cache hit per page (was: a temp lock for
        // page_type, a second for n_cells, a third for the binary search,
        // then a full drop + re-get_page + fourth lock for the iteration).
        let borrowed = page.lock();
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafIndex => {
                let n = borrowed.n_cells();
                let begin = if *started {
                    0
                } else {
                    // Binary search for the first cell with key >= start_key
                    // (allocation-free views; (key, MIN) ordering means a
                    // cell is "less" iff its key is strictly less).
                    let mut lo = 0u16;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], false) else {
                            return Err(Error::corruption("truncated index leaf cell in scan"));
                        };
                        if v.key < start_key {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    lo
                };
                // Iterate the leaf under the SAME lock, borrowing key slices
                // straight from the page buffer (Cell::decode allocated a
                // key Vec per cell here).
                for i in begin..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], false) else {
                        continue;
                    };
                    if !f(v.rowid, v.key) {
                        return Ok(false);
                    }
                }
                *started = true;
                Ok(true)
            }
            PageType::InteriorIndex => {
                // Find the first child that can contain entries >= start_key.
                // (uses the single outer guard — no re-lock)
                let (n, first_cell_idx, right_most) = {
                    let n = borrowed.n_cells();
                    let mut first_cell_idx = n; // default: right_most only
                    if !*started {
                        let mut lo = 0u16;
                        let mut hi = n;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid) as usize;
                            let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], true)
                            else {
                                break;
                            };
                            if v.key < start_key {
                                lo = mid + 1;
                            } else {
                                hi = mid;
                            }
                        }
                        first_cell_idx = lo;
                    }
                    (n, first_cell_idx, borrowed.right_most_pointer())
                };
                // Release the parent page before recursing into children.
                drop(borrowed);
                drop(page);
                // Visit children from first_cell_idx onward, stopping as
                // soon as the callback stops.
                for i in first_cell_idx..n {
                    let page = self.pager.get_page(page_id)?;
                    let borrowed = page.lock();
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let child = match decode_index_cell(&borrowed.data[cell_ptr..], true) {
                        Some(v) => v.left_child,
                        None => break,
                    };
                    drop(borrowed);
                    drop(page);
                    if !self.scan_index_range_subtree(child, start_key, f, started)? {
                        return Ok(false);
                    }
                }
                if right_most != 0 {
                    if !self.scan_index_range_subtree(right_most, start_key, f, started)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in index range scan: {:?}",
                pt
            ))),
        }
    }

    /// Scan all entries in an index B+tree, calling `f(rowid, key)` for each.
    pub fn scan_index<F: FnMut(i64, &[u8]) -> bool>(&mut self, mut f: F) -> Result<()> {
        self.scan_index_subtree(self.root, &mut f)
    }

    fn scan_index_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafIndex => {
                // Allocation-free iteration: borrow key slices from the page.
                let borrowed = page.lock();
                let n = borrowed.n_cells();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let Some(v) = decode_index_cell(&borrowed.data[cell_ptr..], false) else {
                        continue;
                    };
                    if !f(v.rowid, v.key) {
                        return Ok(());
                    }
                }
                Ok(())
            }
            PageType::InteriorIndex => {
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let right = borrowed.right_most_pointer();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        if let Some(c) = decode_index_cell(&borrowed.data[cell_ptr..], true) {
                            v.push(c.left_child);
                        }
                    }
                    v.push(right);
                    v
                };
                drop(page);
                for child in cells {
                    self.scan_index_subtree(child, f)?;
                }
                Ok(())
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in index scan: {:?}",
                pt
            ))),
        }
    }
}

/// Helper: safely extract the key from any cell.
// fn child_key_safe(c: &Cell) -> i64 {
//     c.key()
// }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use tempfile::NamedTempFile;

    fn open_pager() -> Pager {
        let tmp = NamedTempFile::new().unwrap();
        Pager::open(tmp.path(), 256).unwrap()
    }

    #[test]
    fn append_mode_split_fills_pages() {
        // Sequential inserts must produce near-100% page fill: page count
        // for N rows should be ~ceil(N * bytes / page_size), NOT 2x that.
        let pager = open_pager();
        let mut bt = Btree::create(&pager, false).unwrap();
        let n = 20_000i64;
        for i in 1..=n {
            // ~30-byte payload → ~500 rows per 16 KiB page → ~40 pages.
            bt.insert_table(i, b"aaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        }
        let pages = pager.n_pages();
        // With mid splits this would be ~80+ pages; append splits ~40.
        assert!(pages < 50, "expected < 50 pages for sequential inserts, got {}", pages);
        // All rows present and ordered.
        let mut seen = Vec::new();
        bt.scan_table_borrowed(|rowid, _p| {
            seen.push(rowid);
            true
        })
        .unwrap();
        assert_eq!(seen.len(), n as usize);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn empty_leaves_recycled_on_delete() {
        // Insert enough rows to build multiple leaves, delete them ALL,
        // verify (a) every row is gone, (b) freed leaves are on the
        // freelist, (c) re-inserting the same volume doesn't grow the file.
        let pager = open_pager();
        let mut bt = Btree::create(&pager, false).unwrap();
        let n = 20_000i64;
        for i in 1..=n {
            bt.insert_table(i, b"aaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        }
        let pages_before = pager.n_pages();
        assert!(pages_before > 5);
        for i in 1..=n {
            assert!(bt.delete_table(i).unwrap(), "delete {} failed", i);
        }
        // Tree must be empty.
        let mut count = 0usize;
        bt.scan_table_borrowed(|_rowid, _p| {
            count += 1;
            true
        })
        .unwrap();
        assert_eq!(count, 0, "tree should be empty after deleting all rows");
        // Freed leaves on the freelist.
        assert!(
            pager.freelist_count() > 0,
            "freelist should be non-empty after deleting all rows (got {})",
            pager.freelist_count()
        );
        // Re-insert the same volume: file must not grow.
        for i in 1..=n {
            bt.insert_table(i, b"aaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        }
        assert!(
            pager.n_pages() <= pages_before,
            "page count grew after churn: {} -> {}",
            pages_before,
            pager.n_pages()
        );
        // Spot-check rows.
        for i in [1i64, 5000, 19999, 20000] {
            assert!(matches!(bt.lookup_table(i).unwrap(), LookupResult::Found(_)));
        }
    }

    #[test]
    fn delete_rightmost_leaf_recycles() {
        // Specifically exercise the rightmost-child unlink path: insert
        // ranges so the rightmost leaf holds the highest rowids, then
        // delete those and confirm the tree stays traversable.
        let pager = open_pager();
        let mut bt = Btree::create(&pager, false).unwrap();
        for i in 1..=10_000i64 {
            bt.insert_table(i, b"zzzzzzzzzzzzzzzzzzzzzzzzzz").unwrap();
        }
        // Delete the tail range (rightmost leaf's rows).
        for i in (9_000..=10_000).rev() {
            assert!(bt.delete_table(i).unwrap());
        }
        // Remaining rows all present.
        for i in 1..9_000i64 {
            assert!(matches!(bt.lookup_table(i).unwrap(), LookupResult::Found(_)));
        }
        for i in 9_000..=10_000i64 {
            assert!(matches!(bt.lookup_table(i).unwrap(), LookupResult::NotFound));
        }
        // Insert into the freed range again — reuses freed pages, ordering
        // must still hold.
        for i in 9_000..=10_000i64 {
            bt.insert_table(i, b"zzzzzzzzzzzzzzzzzzzzzzzzzz").unwrap();
        }
        let mut seen = Vec::new();
        bt.scan_table_borrowed(|rowid, _p| {
            seen.push(rowid);
            true
        })
        .unwrap();
        assert_eq!(seen.len(), 10_000);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn varint_roundtrip() {
        let cases = [0u64, 1, 127, 128, 16383, 16384, 1 << 20, 1 << 35, u64::MAX];
        let mut buf = [0u8; 10];
        for v in cases {
            let n = varint::encode(v, &mut buf);
            let (d, m) = varint::decode(&buf[..n]).unwrap();
            assert_eq!(v, d);
            assert_eq!(n, m);
        }
    }

    #[test]
    fn insert_and_lookup() {
        let mut pager = open_pager();
        // Create a new B+tree rooted at page 1 (allocate).
        let mut bt = Btree::create(&mut pager, false).unwrap();
        for i in 0..100i64 {
            let payload = format!("row-{}", i).into_bytes();
            bt.insert_table(i, &payload).unwrap();
        }
        for i in 0..100i64 {
            match bt.lookup_table(i).unwrap() {
                LookupResult::Found(p) => {
                    assert_eq!(p, format!("row-{}", i).into_bytes());
                }
                _ => panic!("row {} not found", i),
            }
        }
        match bt.lookup_table(1000).unwrap() {
            LookupResult::NotFound => {}
            _ => panic!("row 1000 should not exist"),
        }
    }

    #[test]
    fn scan_returns_all_in_order() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, false).unwrap();
        // Insert in scrambled order
        let order: Vec<i64> = [5, 1, 9, 3, 7, 2, 8, 4, 6, 0].to_vec();
        for &i in &order {
            bt.insert_table(i, b"x").unwrap();
        }
        let mut seen = Vec::new();
        bt.scan_table(|rowid, _| {
            seen.push(rowid);
            true
        })
        .unwrap();
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn delete_removes_row() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, false).unwrap();
        for i in 0..50i64 {
            bt.insert_table(i, b"x").unwrap();
        }
        assert!(bt.delete_table(25).unwrap());
        match bt.lookup_table(25).unwrap() {
            LookupResult::NotFound => {}
            _ => panic!("row 25 should be deleted"),
        }
        // Other rows still present.
        for i in 0..50i64 {
            if i == 25 {
                continue;
            }
            assert!(matches!(
                bt.lookup_table(i).unwrap(),
                LookupResult::Found(_)
            ));
        }
    }

    #[test]
    fn range_scan() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, false).unwrap();
        for i in 0..1000i64 {
            bt.insert_table(i, b"x").unwrap();
        }
        let mut count = 0;
        bt.scan_table_range(100, 200, |_, _| {
            count += 1;
            true
        })
        .unwrap();
        assert_eq!(count, 101);
    }

    /// Regression test for `split_leaf` page-type preservation.
    ///
    /// Before the fix, `split_leaf` hardcoded `init_leaf_table()` for both
    /// the old and new pages — even when the splitting page was a LeafIndex
    /// page. After the first index split, the index B+tree was silently
    /// corrupted (LeafIndex pages turned into LeafTable pages), and
    /// subsequent `scan_index` calls panicked with
    /// "unexpected page type in index scan: LeafTable".
    ///
    /// This test forces an index split by inserting enough rows to overflow
    /// a single leaf page, then verifies that:
    ///   1. scan_index still works (no panic).
    ///   2. All inserted entries are still findable via lookup_index.
    ///   3. The page types of leaf pages are LeafIndex, not LeafTable.
    #[test]
    fn index_split_preserves_page_type() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, true).unwrap();
        // Insert enough entries to force multiple splits.
        for i in 1..=500i64 {
            let key = Value::Integer(i).encode_order_key();
            bt.insert_index(&key, i).unwrap();
        }
        // Verify scan_index works (no panic).
        let mut seen_rowids = Vec::new();
        bt.scan_index(|rowid, _key| {
            seen_rowids.push(rowid);
            true
        })
        .unwrap();
        seen_rowids.sort();
        assert_eq!(seen_rowids, (1..=500).collect::<Vec<_>>());
        // Verify lookup_index finds every entry.
        for i in 1..=500i64 {
            let key = Value::Integer(i).encode_order_key();
            let matches = bt.lookup_index(&key).unwrap();
            assert_eq!(matches, vec![i], "lookup_index for value {} failed", i);
        }
    }

    /// The index B+tree is now sorted by (key, rowid) — verify that RANDOM
    /// (non-monotonic) insertion order still produces a correct, seekable
    /// tree with multi-level splits. This exercises:
    ///   - interior-page descent with (key, rowid) comparisons
    ///   - interior-page splits (500+ shuffled entries force 2+ levels)
    ///   - lookup_index binary search after splits
    #[test]
    fn index_btree_random_insertion_order() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, true).unwrap();
        // Simple LCG shuffle of 1..=600.
        let mut vals: Vec<i64> = (1..=600).collect();
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        for i in (1..vals.len()).rev() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (seed >> 33) as usize % (i + 1);
            vals.swap(i, j);
        }
        for &i in &vals {
            let key = Value::Integer(i).encode_order_key();
            bt.insert_index(&key, i).unwrap();
        }
        // Every entry findable.
        for i in 1..=600i64 {
            let key = Value::Integer(i).encode_order_key();
            let matches = bt.lookup_index(&key).unwrap();
            assert_eq!(matches, vec![i], "lookup_index for value {} failed", i);
        }
        // scan_index visits entries in (key, rowid) order → sorted keys.
        let mut seen: Vec<i64> = Vec::new();
        bt.scan_index(|rowid, _key| {
            seen.push(rowid);
            true
        })
        .unwrap();
        let mut expected: Vec<i64> = (1..=600).collect();
        expected.sort();
        assert_eq!(seen, expected, "index scan should be in sorted key order");
        // Deletions remove exactly the right entry.
        for i in (1..=600i64).step_by(3) {
            let key = Value::Integer(i).encode_order_key();
            assert!(bt.delete_index(&key, i).unwrap(), "delete {} failed", i);
        }
        for i in 1..=600i64 {
            let key = Value::Integer(i).encode_order_key();
            let matches = bt.lookup_index(&key).unwrap();
            // step_by(3) from 1: deleted set = 1, 4, 7, ...
            let deleted = (i - 1) % 3 == 0;
            if deleted {
                assert!(matches.is_empty(), "value {} should be deleted", i);
            } else {
                assert_eq!(matches, vec![i], "value {} should remain", i);
            }
        }
    }

    /// scan_index_from: range scans start at the first key >= start.
    #[test]
    fn index_range_scan_from() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, true).unwrap();
        for i in 1..=500i64 {
            let key = Value::Integer(i * 2).encode_order_key();
            bt.insert_index(&key, i).unwrap();
        }
        // All entries with key >= 700 (values 700, 702, ..., 1000).
        let mut seen: Vec<i64> = Vec::new();
        bt.scan_index_from(&Value::Integer(700).encode_order_key(), |rowid, key| {
            // Collect rowids whose key is < 800, then stop.
            if key > &Value::Integer(800).encode_order_key()[..] {
                return false;
            }
            seen.push(rowid);
            true
        })
        .unwrap();
        // 700..=800 step 2 → 51 entries, rowids 350..=400.
        assert_eq!(seen.len(), 51, "range scan count wrong: {:?}", seen);
        assert_eq!(seen[0], 350);
        assert_eq!(seen[50], 400);
    }

    /// Regression test for `delete_from_page` index page handling.
    ///
    /// Before the fix, `delete_from_page` only handled LeafTable and
    /// InteriorTable page types. When called on an index B+tree (to delete
    /// an entry during UPDATE), it fell through to the corruption-error
    /// path. The error was swallowed by `let _ = delete_index_entry(...)`
    /// in `exec_update`, so the old entry stayed in the index and the new
    /// entry was inserted alongside it — producing duplicate index entries.
    #[test]
    fn index_delete_then_reinsert_no_duplicate() {
        let mut pager = open_pager();
        let mut bt = Btree::create(&mut pager, true).unwrap();
        // Insert a single entry.
        let key = Value::Integer(42).encode_order_key();
        bt.insert_index(&key, 7).unwrap();
        // Delete it.
        assert!(bt.delete_index(&key, 7).unwrap());
        // Re-insert with the same key+rowid.
        bt.insert_index(&key, 7).unwrap();
        // Lookup should return exactly one rowid, not two.
        let matches = bt.lookup_index(&key).unwrap();
        assert_eq!(matches, vec![7], "duplicate index entries after delete+reinsert");
    }
}
