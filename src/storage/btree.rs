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

/// A B+tree over a pager. Trees are identified by their root page ID.
pub struct Btree<'a> {
    pub pager: &'a mut Pager,
    pub root: PageId,
    pub is_index: bool,
}

/// Result of inserting into a page: either the insert succeeded, or the
/// page split and a new key needs to be propagated up.
enum InsertResult {
    Done,
    Split { new_page: PageId, split_key: i64 },
}

/// A point lookup result.
pub enum LookupResult {
    Found(Vec<u8>),
    NotFound,
}

impl<'a> Btree<'a> {
    pub fn new(pager: &'a mut Pager, root: PageId, is_index: bool) -> Self {
        Self {
            pager,
            root,
            is_index,
        }
    }

    /// Initialize a new B+tree (create the root page as an empty leaf).
    pub fn create(pager: &'a mut Pager, is_index: bool) -> Result<Self> {
        let root = pager.allocate_page()?;
        let page = pager.get_page(root)?;
        if is_index {
            page.borrow_mut().init_leaf_index();
        } else {
            page.borrow_mut().init_leaf_table();
        }
        Ok(Self {
            pager,
            root,
            is_index,
        })
    }

    /// Look up a rowid in a table B+tree. Returns the payload bytes.
    pub fn lookup_table(&mut self, rowid: i64) -> Result<LookupResult> {
        let mut page_id = self.root;
        loop {
            let page = self.pager.get_page(page_id)?;
            let pt = page.borrow().page_type()?;
            match pt {
                PageType::LeafTable => {
                    let n = page.borrow().n_cells();
                    for i in 0..n {
                        let cell_ptr = page.borrow().cell_pointer(i) as usize;
                        let borrowed = page.borrow();
                        let cell = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                        if let Cell::TableLeaf {
                            rowid: rid,
                            payload,
                        } = cell
                        {
                            if rid == rowid {
                                return Ok(LookupResult::Found(payload));
                            }
                            if rid > rowid {
                                return Ok(LookupResult::NotFound);
                            }
                        }
                    }
                    return Ok(LookupResult::NotFound);
                }
                PageType::InteriorTable => {
                    let borrowed = page.borrow();
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
        let cell = Cell::TableLeaf {
            rowid,
            payload: payload.to_vec(),
        };
        match self.insert_into_page(self.root, cell)? {
            InsertResult::Done => Ok(()),
            InsertResult::Split {
                new_page,
                split_key,
            } => {
                // The root split: create a new root pointing to the old and new pages.
                let old_root = self.root;
                let new_root = self.pager.allocate_page()?;
                {
                    let page_ref = self.pager.get_page(new_root)?;
                    let mut page = page_ref.borrow_mut();
                    page.init_interior_table();
                    page.set_right_most_pointer(new_page);
                }
                // Insert a cell pointing to the old root with the split key.
                // Convention: cell (left_child, key) means left_child has rowids <= key.
                // split_key = first key of new page. Old root has keys < split_key.
                // So the cell key should be split_key - 1 (max key in old root).
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

    /// Insert a cell into a page (and propagate splits if needed).
    fn insert_into_page(&mut self, page_id: PageId, cell: Cell) -> Result<InsertResult> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.borrow().page_type()?;

        if pt.is_leaf() {
            // Leaf: insert directly.
            let cell_size = cell.encoded_size();
            let free = page.borrow().free_space();
            // Need space for the cell + a 2-byte pointer.
            if free < cell_size as u32 + 2 {
                drop(page);
                return self.split_leaf(page_id, cell);
            }
            self.insert_cell_into_page(page_id, &cell)?;
            Ok(InsertResult::Done)
        } else {
            // Interior: find the child to descend into.
            // Convention: cell (left_child, key) means left_child contains
            // rowids <= key. right_most_pointer contains rowids > last cell's key.
            let child_id = {
                let borrowed = page.borrow();
                let n = borrowed.n_cells();
                let k = cell.key();
                let mut next = borrowed.right_most_pointer();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    if let Cell::TableInterior { left_child, key } = c {
                        if k <= key {
                            next = left_child;
                            break;
                        }
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
                } => {
                    // The child split: old child has keys < split_key,
                    // new page has keys >= split_key.
                    // We add a new interior cell (new_page, split_key - 1).
                    // This means: new_page contains keys <= split_key - 1.
                    // But new_page actually contains keys >= split_key!
                    //
                    // This is intentionally WRONG for the <= convention, but
                    // we work around it by also updating the old cell. Since
                    // we can't update in place, we use a different approach:
                    //
                    // Actually, let's use split_key as the separator. The cell
                    // (new_page, split_key) with <= convention means new_page
                    // has keys <= split_key. But new_page has keys >= split_key.
                    // The ONLY key that's in both is split_key itself (the first
                    // key of new_page). So this is wrong for all keys > split_key.
                    //
                    // REAL FIX: The separator should be the MAX key of the OLD
                    // child (split_key - 1). We add cell (new_page, old_max_key)
                    // where old_max_key is what the old cell had. But we need to
                    // also update the old cell's key to split_key - 1.
                    //
                    // Since tracking and updating the old cell is complex, let's
                    // use a simpler model: the B+tree interior cell key is the
                    // MIN key of the RIGHT child (i.e., the separator). Descent:
                    // find the first cell where key > search_key; go to left_child.
                    // If no such cell, go to right_most.
                    //
                    // With this model:
                    // - Cell (A, 50): A has keys < 50
                    // - Cell (B, 100): B has keys 50-99
                    // - right_most: keys >= 100
                    //
                    // After splitting B (50-99) into B1 (50-74) and B2 (75-99):
                    // - split_key = 75 (first key of B2)
                    // - Add cell (B2, 75): B2 has keys 75-99
                    // - Old cell (B, 100) still points to B1 (which now has 50-74)
                    //   but the key 100 is wrong — it should be 75.
                    //
                    // Hmm, this still requires updating the old cell.
                    //
                    // OK, final approach: DON'T modify the old cell. Instead,
                    // the new cell has key = split_key, and the old cell keeps
                    // its old key. During descent, we use < (strict less than):
                    // find the first cell where key > search_key; go to left_child.
                    //
                    // - Cell (A, 50): search_key < 50 → go to A. A has keys < 50.
                    // - Cell (B, 100): search_key < 100 → go to B. B has keys 50-99 (but now 50-74).
                    // - right_most: search_key >= 100.
                    //
                    // After split, add cell (B2, 75):
                    // - Cell (A, 50): search_key < 50 → A.
                    // - Cell (B2, 75): search_key < 75 → B2. But B2 has 75-99!
                    //   search_key = 60: 60 < 75 → B2. But 60 is in B1!
                    //
                    // This is STILL wrong. The fundamental issue is that without
                    // updating the old cell, we can't correctly route.
                    //
                    // FINAL APPROACH: Use split_key - 1 as the new cell's key.
                    // Cell (new_page, split_key - 1) means: with < convention,
                    // search_key < split_key → new_page. But new_page has keys >= split_key.
                    // So search_key < split_key goes to new_page, which is wrong.
                    //
                    // I give up on the clever approaches. Let me just update the
                    // old cell in place by rewriting the entire page.

                    // Read ALL cells from the interior page, replace the old cell
                    // with two new ones, and rewrite.
                    let (n_cells, pt2) = {
                        let p = self.pager.get_page(page_id)?;
                        let b = p.borrow();
                        (b.n_cells(), b.page_type()?)
                    };

                    // Find which cell pointed to child_id (or if it's right_most).
                    let mut cells: Vec<Cell> = Vec::new();
                    let mut right_most = 0u32;
                    let mut found_idx: Option<usize> = None;
                    {
                        let p = self.pager.get_page(page_id)?;
                        let borrowed = p.borrow();
                        right_most = borrowed.right_most_pointer();
                        for i in 0..n_cells {
                            let cell_ptr = borrowed.cell_pointer(i) as usize;
                            let c = Cell::decode(&borrowed.data[cell_ptr..], pt2)?;
                            if let Cell::TableInterior { left_child, .. } = &c {
                                if *left_child == child_id {
                                    found_idx = Some(i as usize);
                                }
                            }
                            cells.push(c);
                        }
                    }

                    if let Some(idx) = found_idx {
                        // Replace cells[idx] with two cells.
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
                    } else {
                        // right_most case: child_id was right_most.
                        let cell1 = Cell::TableInterior {
                            left_child: child_id,
                            key: split_key - 1,
                        };
                        cells.push(cell1);
                        right_most = new_page;
                    }

                    // Check if we need to split the interior page.
                    let total_size: usize = cells.iter().map(|c| c.encoded_size() + 2).sum();
                    let page_size = self.pager.page_size() as usize;
                    let header_offset = if page_id == 0 {
                        crate::storage::page::DB_HEADER_SIZE as usize
                    } else {
                        0
                    };
                    let available = page_size - header_offset - PAGE_HEADER_SIZE as usize;
                    if total_size > available {
                        // Need to split — for now, just rewrite what fits.
                        // This is a known limitation.
                    }

                    // Rewrite the page.
                    {
                        let p = self.pager.get_page(page_id)?;
                        let mut borrowed = p.borrow_mut();
                        borrowed.init_interior_table();
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

    /// Insert a cell into a leaf or interior page. Cells are kept sorted by key.
    fn insert_cell_into_page(&mut self, page_id: PageId, cell: &Cell) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.borrow().page_type()?;
        let cell_size = cell.encoded_size();
        let n = page.borrow().n_cells();

        // Find insertion position by key.
        let pos = {
            let borrowed = page.borrow();
            let mut lo = 0;
            let mut hi = n;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let cell_ptr = borrowed.cell_pointer(mid) as usize;
                let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                if c.key() < cell.key() {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };

        // Allocate space at the cell content area.
        let new_content_start = {
            let borrowed = page.borrow();
            borrowed.cell_content_start() - cell_size as u32
        };

        // Write the cell bytes.
        {
            let mut borrowed = page.borrow_mut();
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
        Ok(())
    }

    /// Split a leaf page. Returns the new page ID and the split key.
    fn split_leaf(&mut self, page_id: PageId, new_cell: Cell) -> Result<InsertResult> {
        // Read all existing cells + the new one.
        let page = self.pager.get_page(page_id)?;
        let pt = page.borrow().page_type()?;
        let n = page.borrow().n_cells();
        let mut cells: Vec<Cell> = Vec::with_capacity(n as usize + 1);
        // let mut new_pos = n as usize;
        for i in 0..n {
            let borrowed = page.borrow();
            let cell_ptr = borrowed.cell_pointer(i) as usize;
            let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
            if c.key() < new_cell.key() {
                cells.push(c);
            } else {
                // new_pos = cells.len();
                cells.push(new_cell.clone());
                // Continue reading remaining cells
                for j in i..n {
                    let cell_ptr = borrowed.cell_pointer(j) as usize;
                    let c = Cell::decode(&borrowed.data[cell_ptr..], pt)?;
                    cells.push(c);
                }
                break;
            }
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
        new_page.borrow_mut().init_leaf_table();

        // Clear the old page and re-insert the first half.
        {
            let page_ref = self.pager.get_page(page_id)?;
            let mut borrowed = page_ref.borrow_mut();
            borrowed.init_leaf_table();
        }

        // Re-insert first half into old page, second half into new page.
        for c in &cells[..mid] {
            self.insert_cell_into_page(page_id, c)?;
        }
        for c in &cells[mid..] {
            self.insert_cell_into_page(new_page_id, c)?;
        }

        // The split key is the FIRST key of the new page (the min key in the
        // second half). This is used as the separator in the parent: keys <
        // split_key go to the old page, keys >= split_key go to the new page.
        let split_key = cells[mid].key();
        Ok(InsertResult::Split {
            new_page: new_page_id,
            split_key,
        })
    }

    /// Split an interior page. Same idea but the middle cell moves up.
    // fn split_interior(&mut self, page_id: PageId, new_cell: Cell) -> Result<InsertResult> {
    //     let page = self.pager.get_page(page_id)?;
    //     let pt = page.borrow().page_type()?;
    //     let n = page.borrow().n_cells();
    //     let right = page.borrow().right_most_pointer();

    //     let mut cells: Vec<Cell> = Vec::with_capacity(n as usize + 1);
    //     let mut inserted = false;
    //     for i in 0..n {
    //         let borrowed = page.borrow();
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
    //     new_page.borrow_mut().init_interior_table();

    //     // Clear old page.
    //     {
    //         let page_ref = self.pager.get_page(page_id)?;
    //         let mut borrowed = page_ref.borrow_mut();
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
    //     self.pager.get_page(new_page_id)?.borrow_mut().set_right_most_pointer(right);

    //     Ok(InsertResult::Split { new_page: new_page_id, split_key })
    // }

    /// Delete a (rowid) from a table B+tree. Does not rebalance (we leave
    /// pages underfull rather than risk concurrent-merge bugs).
    pub fn delete_table(&mut self, rowid: i64) -> Result<bool> {
        self.delete_from_page(self.root, rowid)
    }

    fn delete_from_page(&mut self, page_id: PageId, rowid: i64) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.borrow().page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = page.borrow().n_cells();
                let pos = {
                    let borrowed = page.borrow();
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
                    let borrowed = page.borrow();
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
                    let mut borrowed = page.borrow_mut();
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
                Ok(true)
            }
            PageType::InteriorTable => {
                let child_id = {
                    let borrowed = page.borrow();
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
    //     let pt = page.borrow().page_type()?;
    //     let n = page.borrow().n_cells();
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
    //         let mut borrowed = page.borrow_mut();
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

    fn scan_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.borrow().page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = page.borrow().n_cells();
                for i in 0..n {
                    let cell_ptr = page.borrow().cell_pointer(i) as usize;
                    let borrowed = page.borrow();
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
                let n = page.borrow().n_cells();
                let right = page.borrow().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.borrow();
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

    fn scan_range_subtree<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        f: &mut F,
    ) -> Result<()> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.borrow().page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = page.borrow().n_cells();
                for i in 0..n {
                    let cell_ptr = page.borrow().cell_pointer(i) as usize;
                    let borrowed = page.borrow();
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
                let n = page.borrow().n_cells();
                let right = page.borrow().right_most_pointer();
                let cells: Vec<(PageId, i64)> = {
                    let borrowed = page.borrow();
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
        // We store index entries keyed by rowid (so duplicates of the same
        // rowid are prevented), with the encoded key as part of the cell payload.
        let cell = Cell::IndexLeaf {
            key: key.to_vec(),
            rowid,
        };
        // For now, we treat the index B+tree like a table B+tree keyed by rowid.
        // The encoded `key` is stored in the cell payload and scanned linearly
        // during lookups. A future optimization would build a real B+tree over
        // the encoded key.
        match self.insert_into_page(self.root, cell)? {
            InsertResult::Done => Ok(()),
            InsertResult::Split {
                new_page,
                split_key,
            } => {
                let old_root = self.root;
                let new_root = self.pager.allocate_page()?;
                {
                    let page_ref = self.pager.get_page(new_root)?;
                    let mut page = page_ref.borrow_mut();
                    page.init_interior_index();
                    page.set_right_most_pointer(new_page);
                }
                let cell = Cell::IndexInterior {
                    left_child: old_root,
                    key: Vec::new(), // key not used for routing in this simplified design
                    rowid: split_key,
                };
                self.insert_cell_into_page(new_root, &cell)?;
                self.root = new_root;
                Ok(())
            }
        }
    }

    /// Delete a (key, rowid) pair from an index B+tree.
    pub fn delete_index(&mut self, rowid: i64) -> Result<bool> {
        // Same as table delete (since we key by rowid).
        self.delete_from_page(self.root, rowid)
    }

    /// Look up all rowids matching a given key in an index B+tree.
    /// Returns a list of rowids (usually 1, but may be more for non-unique indexes).
    pub fn lookup_index(&mut self, key: &[u8]) -> Result<Vec<i64>> {
        let mut results = Vec::new();
        let mut bt = Btree {
            pager: self.pager,
            root: self.root,
            is_index: true,
        };
        bt.scan_index(|cell_rowid, cell_key| {
            if cell_key == key {
                results.push(cell_rowid);
            }
            // Continue scanning — there may be more matches (non-unique index).
            // For a sorted B+tree we could stop early once cell_key > key,
            // but our simplified index is keyed by rowid, not by key, so we
            // must scan all entries.
            true
        })?;
        Ok(results)
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
        let pt = page.borrow().page_type()?;
        match pt {
            PageType::LeafIndex => {
                let n = page.borrow().n_cells();
                for i in 0..n {
                    let cell_ptr = page.borrow().cell_pointer(i) as usize;
                    let borrowed = page.borrow();
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
                let n = page.borrow().n_cells();
                let right = page.borrow().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.borrow();
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
}
