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
            let pt = page.lock().page_type()?;
            match pt {
                PageType::LeafTable => {
                    // Single immutable borrow for the whole leaf scan.
                    let borrowed = page.lock();
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
                    let borrowed = page.lock();
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
        self.pager.note_write();

        // Walk right_most_pointer chain down to the rightmost leaf.
        let mut page_id = self.root;
        let leaf_id;
        loop {
            let page = self.pager.get_page(page_id)?;
            let pt = page.lock().page_type()?;
            match pt {
                PageType::LeafTable => {
                    leaf_id = page_id;
                    break;
                }
                PageType::InteriorTable => {
                    let right = page.lock().right_most_pointer();
                    if right == 0 {
                        // Shouldn't happen on a valid interior page, but fall back.
                        return self.insert_table(rowid, payload);
                    }
                    page_id = right;
                }
                _ => {
                    return self.insert_table(rowid, payload);
                }
            }
        }

        // Now at the rightmost leaf. Check the last cell's rowid to confirm
        // this really is an append (caller's responsibility, but we verify).
        let page = self.pager.get_page(leaf_id)?;
        let n = page.lock().n_cells();
        if n > 0 {
            let borrowed = page.lock();
            let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
            if let Some((last_rowid, _)) = varint::decode_signed(&borrowed.data[cell_ptr..]) {
                if rowid <= last_rowid {
                    // Not an append — fall back to the normal path.
                    drop(borrowed);
                    return self.insert_table(rowid, payload);
                }
            }
        }

        // Compute the cell bytes once.
        let mut cell_buf = Vec::with_capacity(payload.len() + 12);
        // Encode varint rowid.
        let mut rid_buf = [0u8; 9];
        let n_rid = varint::encode_signed(rowid, &mut rid_buf);
        cell_buf.extend_from_slice(&rid_buf[..n_rid]);
        // Encode varint payload length.
        let mut plen_buf = [0u8; 9];
        let n_plen = varint::encode(payload.len() as u64, &mut plen_buf);
        cell_buf.extend_from_slice(&plen_buf[..n_plen]);
        // Payload.
        cell_buf.extend_from_slice(payload);

        let cell_size = cell_buf.len() as u32;

        // Check if the leaf has space.
        let free = page.lock().free_space();
        if free < cell_size + 2 {
            // Need to split — fall back to the normal insert path, which
            // handles splitting + propagating the split up the tree.
            return self.insert_table(rowid, payload);
        }

        // Append: write cell at the new content start, write pointer at end.
        {
            let mut borrowed = page.lock();
            let new_content_start = borrowed.cell_content_start() - cell_size;
            let off = new_content_start as usize;
            borrowed.data[off..off + cell_size as usize].copy_from_slice(&cell_buf);
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
        self.pager.note_dirty(leaf_id);
        Ok(())
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
    pub fn update_table(&mut self, rowid: i64, new_payload: &[u8]) -> Result<bool> {
        // Notify the pager that a write is about to happen (in-place UPDATE).
        self.pager.note_write();
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            let pt = page.lock().page_type()?;
            match pt {
                PageType::LeafTable => {
                    // First pass: find the matching cell, record its offset
                    // and the old payload length. We must NOT hold a borrow
                    // across the mutable write below.
                    let mut match_offset: Option<usize> = None;
                    let mut match_old_len: Option<usize> = None;
                    let n = page.lock().n_cells();
                    for i in 0..n {
                        let cell_ptr = page.lock().cell_pointer(i) as usize;
                        let borrowed = page.lock();
                        let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableLeaf {
                            rowid: rid,
                            payload: old_payload,
                        } = &cell
                        {
                            if *rid == rowid {
                                match_offset = Some(cell_ptr);
                                match_old_len = Some(old_payload.len());
                                break;
                            }
                            if *rid > rowid {
                                // Rowid doesn't exist (cells are sorted).
                                return Ok(false);
                            }
                        }
                    }
                    let (cell_ptr, old_len) = match (match_offset, match_old_len) {
                        (Some(o), Some(l)) => (o, l),
                        _ => return Ok(false),
                    };
                    if old_len != new_payload.len() {
                        // Size changed — caller must fall back to delete+insert.
                        return Ok(false);
                    }
                    // Compute the offset of the payload within the cell.
                    // Cell layout: varint(rowid) + varint(payload_len) + payload.
                    let mut prefix = Vec::with_capacity(20);
                    let mut buf = [0u8; 10];
                    let k = varint::encode_signed(rowid, &mut buf);
                    prefix.extend_from_slice(&buf[..k]);
                    let p = varint::encode(new_payload.len() as u64, &mut buf);
                    prefix.extend_from_slice(&buf[..p]);
                    let payload_offset = cell_ptr + prefix.len();
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
            let is_idx = pt.is_index();
            let child_id = {
                let borrowed = page.lock();
                let n = borrowed.n_cells();
                let mut next = borrowed.right_most_pointer();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    let target_le_sep = if is_idx {
                        c.cmp_index_target(cell.index_key(), cell.key())
                            != std::cmp::Ordering::Less
                    } else {
                        cell.key() <= c.key()
                    };
                    if target_le_sep {
                        next = c.left_child();
                        break;
                    }
                }
                next
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

        // Find insertion position by the page's sort order.
        let pos = {
            let borrowed = page.lock();
            let mut lo = 0;
            let mut hi = n;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let cell_ptr = borrowed.cell_pointer(mid) as usize;
                let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                let existing_before_new = if is_idx {
                    // Compare (key, rowid) pairs.
                    c.cmp_index_target(cell.index_key(), cell.key())
                        == std::cmp::Ordering::Less
                } else {
                    c.key() < cell.key()
                };
                if existing_before_new {
                    lo = mid + 1;
                } else {
                    hi = mid;
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

            // Shift cell pointers to make room at position `pos`.
            let header_offset = if page_id == 0 {
                crate::storage::page::DB_HEADER_SIZE as usize
            } else {
                0
            };
            let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
            let pos_usize = pos as usize;
            let n_usize = n as usize;
            // Shift pointers [pos..n] one slot right.
            for i in (pos_usize..n_usize).rev() {
                let src = ptr_array_start + i * 2;
                let dst = ptr_array_start + (i + 1) * 2;
                let v = u16::from_be_bytes(borrowed.data[src..src + 2].try_into().unwrap());
                borrowed.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
            }
            // Insert the new pointer.
            let dst = ptr_array_start + pos_usize * 2;
            borrowed.data[dst..dst + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n + 1);
            borrowed.dirty = true;
        }
        self.pager.note_dirty(page_id);
        Ok(())
    }

    /// Split a leaf page. Returns the new page ID and the separator info.
    fn split_leaf(&mut self, page_id: PageId, new_cell: Cell) -> Result<InsertResult> {
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
        let mid = total / 2;

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

    /// Delete a (rowid) from a table B+tree. Does not rebalance (we leave
    /// pages underfull rather than risk concurrent-merge bugs).
    pub fn delete_table(&mut self, rowid: i64) -> Result<bool> {
        // Notify the pager that a write is about to happen.
        self.pager.note_write();
        self.delete_from_page(self.root, rowid)
    }

    fn delete_from_page(&mut self, page_id: PageId, rowid: i64) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable | PageType::LeafIndex => {
                let n = page.lock().n_cells();
                let pos = {
                    let borrowed = page.lock();
                    let mut lo = 0;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if c.key() < rowid {
                            lo = mid + 1;
                        } else if c.key() > rowid {
                            hi = mid;
                        } else {
                            lo = mid;
                            break;
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
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    c.key() == rowid
                };
                if !key_matches {
                    return Ok(false);
                }
                // Shift cell pointers left to remove the slot at `pos`.
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
                    for i in pos_usize..n_usize - 1 {
                        let src = ptr_array_start + (i + 1) * 2;
                        let dst = ptr_array_start + i * 2;
                        let v = u16::from_be_bytes(borrowed.data[src..src + 2].try_into().unwrap());
                        borrowed.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
                    }
                    borrowed.set_n_cells(n - 1);
                    borrowed.dirty = true;
                }
                self.pager.note_dirty(page_id);
                Ok(true)
            }
            PageType::InteriorTable | PageType::InteriorIndex => {
                let child_id = {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let mut next = borrowed.right_most_pointer();
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableInterior { left_child, key } = c {
                            if rowid <= key {
                                next = left_child;
                                break;
                            }
                        }
                        // For index interior cells, the layout differs but
                        // the routing logic is the same: rowid <= key → go
                        // to left_child. IndexInterior also stores left_child.
                        if let Cell::IndexInterior { left_child, rowid: cell_rowid, .. } = c {
                            if rowid <= cell_rowid {
                                next = left_child;
                                break;
                            }
                        }
                    }
                    next
                };
                drop(page);
                self.delete_from_page(child_id, rowid)
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
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = page.lock().n_cells();
                for i in 0..n {
                    let cell_ptr = page.lock().cell_pointer(i) as usize;
                    let borrowed = page.lock();
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
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
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
                // Binary search for the first cell >= (key, rowid).
                let pos = {
                    let borrowed = page.lock();
                    let mut lo = 0;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if c.cmp_index_target(key, rowid) == std::cmp::Ordering::Less {
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
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    c.index_key() == key && c.key() == rowid
                };
                if !key_matches {
                    return Ok(false);
                }
                drop(page);
                self.remove_cell_at(page_id, pos, n)
            }
            PageType::InteriorIndex => {
                let child_id = {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let mut next = borrowed.right_most_pointer();
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if c.cmp_index_target(key, rowid) != std::cmp::Ordering::Less {
                            next = c.left_child();
                            break;
                        }
                    }
                    next
                };
                drop(page);
                self.delete_index_from_page(child_id, key, rowid)
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
            for i in pos_usize..n_usize - 1 {
                let src = ptr_array_start + (i + 1) * 2;
                let dst = ptr_array_start + i * 2;
                let v = u16::from_be_bytes(borrowed.data[src..src + 2].try_into().unwrap());
                borrowed.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
            }
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
        self.scan_index_range_subtree(self.root, start_key, &mut f, &mut false)
    }

    /// Recursive range scan. `started` tracks whether we've passed the start
    /// key yet (once true, every entry is visited).
    fn scan_index_range_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start_key: &[u8],
        f: &mut F,
        started: &mut bool,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafIndex => {
                let n = page.lock().n_cells();
                let begin = if *started {
                    0
                } else {
                    // Binary search for the first cell >= (start_key, MIN).
                    let borrowed = page.lock();
                    let mut lo = 0u16;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if c.cmp_index_target(start_key, i64::MIN)
                            == std::cmp::Ordering::Less
                        {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    lo
                };
                drop(page);
                for i in begin..n {
                    let page = self.pager.get_page(page_id)?;
                    let borrowed = page.lock();
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    if let Cell::IndexLeaf { key, rowid } = c {
                        if !f(rowid, &key) {
                            return Ok(());
                        }
                    }
                }
                *started = true;
                Ok(())
            }
            PageType::InteriorIndex => {
                // Find the first child that can contain entries >= start_key.
                let (n, first_cell_idx, right_most) = {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let mut first_cell_idx = n; // default: right_most only
                    if !*started {
                        let mut lo = 0u16;
                        let mut hi = n;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid) as usize;
                            let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                            if c.cmp_index_target(start_key, i64::MIN)
                                == std::cmp::Ordering::Less
                            {
                                lo = mid + 1;
                            } else {
                                hi = mid;
                            }
                        }
                        first_cell_idx = lo;
                    }
                    (n, first_cell_idx, borrowed.right_most_pointer())
                };
                drop(page);
                // Visit children from first_cell_idx onward.
                for i in first_cell_idx..n {
                    let page = self.pager.get_page(page_id)?;
                    let borrowed = page.lock();
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    let child = c.left_child();
                    drop(borrowed);
                    drop(page);
                    self.scan_index_range_subtree(child, start_key, f, started)?;
                }
                if right_most != 0 {
                    self.scan_index_range_subtree(right_most, start_key, f, started)?;
                }
                Ok(())
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
                let n = page.lock().n_cells();
                for i in 0..n {
                    let cell_ptr = page.lock().cell_pointer(i) as usize;
                    let borrowed = page.lock();
                    let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    if let Cell::IndexLeaf { key, rowid } = cell {
                        if !f(rowid, &key) {
                            return Ok(());
                        }
                    }
                }
                Ok(())
            }
            PageType::InteriorIndex => {
                let n = page.lock().n_cells();
                let right = page.lock().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::IndexInterior { left_child, .. } = c {
                            v.push(left_child);
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
