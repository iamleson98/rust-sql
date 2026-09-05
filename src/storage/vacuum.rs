//! Schema-tree root pinning: the schema btree lives at page 0 (a hybrid
//! page — 100-byte file header + btree header at offset 100), and every
//! reader hardcodes that root (`Btree::new(pager, 0, ...)`). When the
//! tree grows past one page, a split moves the root to a freshly
//! allocated interior page — without re-pinning, the root reference is
//! lost and a reopen sees only the rows still under page 0 (silent data
//! loss for schemas of ~25+ objects).
//!
//! `repin_schema_root` swaps the two pages' btree content: the new
//! interior root's layout lands in page 0 (file header preserved, cell
//! bytes kept at their ABSOLUTE in-page offsets), and page 0's old leaf
//! content lands in the allocated page — then the interior's child
//! reference to "page 0" (the old leaf) is rewritten to the allocated
//! page. After the swap the tree is rooted at 0 again.

use crate::error::{Error, Result};
use crate::storage::page::{PageType, DB_HEADER_SIZE, PAGE_HEADER_SIZE};
use crate::storage::pager::Pager;

/// Pull the schema btree's root back into page 0 after it moved to
/// `new_root` (an INTERIOR page produced by a split).
///
/// Callers: after any mutation through `Btree::new(pager, 0, ..)`, check
/// `bt.root != 0` and invoke this. Safe under the engine's single-writer
/// discipline.
pub(crate) fn repin_schema_root(pager: &Pager, new_root: u32) -> Result<()> {
    if new_root == 0 {
        return Ok(());
    }
    let psz = pager.page_size() as usize;
    let hdr = DB_HEADER_SIZE as usize;
    let p0_ref = pager.get_page(0)?;
    let r_ref = pager.get_page(new_root)?;
    let p0_bytes: Vec<u8>;
    let r_bytes: Vec<u8>;
    {
        let p = p0_ref.lock();
        let r = r_ref.lock();
        if p.page_type()? != PageType::LeafTable {
            return Err(Error::corruption(
                "schema root repin: page 0 is not the split leaf",
            ));
        }
        if r.page_type()? != PageType::InteriorTable {
            return Err(Error::corruption(
                "schema root repin: new root is not an interior page",
            ));
        }
        p0_bytes = p.data.clone();
        r_bytes = r.data.clone();
    }
    drop(p0_ref);
    drop(r_ref);

    // ---- Build page 0's new content: the INTERIOR root layout.
    // File header [0..100) preserved; btree header fields copied to
    // [100..112); pointer array [12+2i] -> [112+2i]; cell bytes at their
    // ABSOLUTE offsets (interior cell content starts high — above
    // 112+2n by construction, same as any page).
    let mut new_p0 = p0_bytes.clone();
    {
        let r_n = u16::from_be_bytes(r_bytes[4..6].try_into().unwrap());
        let r_start = u16::from_be_bytes(r_bytes[6..8].try_into().unwrap());
        // btree header (type/n_cells/content_start/right-most).
        new_p0[hdr] = r_bytes[0];
        new_p0[hdr + 4..hdr + 6].copy_from_slice(&r_bytes[4..6]);
        new_p0[hdr + 6..hdr + 8].copy_from_slice(&r_bytes[6..8]);
        new_p0[hdr + 8..hdr + 12].copy_from_slice(&r_bytes[8..12]);
        // Pointer array.
        for i in 0..r_n as usize {
            let src = PAGE_HEADER_SIZE as usize + i * 2;
            let dst = hdr + PAGE_HEADER_SIZE as usize + i * 2;
            new_p0[dst..dst + 2].copy_from_slice(&r_bytes[src..src + 2]);
        }
        // Cells at absolute offsets [r_start..psz).
        let rs = r_start as usize;
        if rs > 12 && rs < psz {
            new_p0[rs..psz].copy_from_slice(&r_bytes[rs..psz]);
        }
        // The cells region must not overlap the relocated pointer array.
        let ptr_end = hdr + PAGE_HEADER_SIZE as usize + r_n as usize * 2;
        if rs < ptr_end {
            return Err(Error::corruption(
                "schema root repin: interior cells overlap page-0 header area",
            ));
        }
    }

    // ---- Build the allocated page's new content: page 0's old LEAF
    // layout, re-based to a normal page (btree header at 0).
    let mut new_r = vec![0u8; psz];
    {
        let p_n = u16::from_be_bytes(p0_bytes[hdr + 4..hdr + 6].try_into().unwrap());
        let p_start = u16::from_be_bytes(p0_bytes[hdr + 6..hdr + 8].try_into().unwrap());
        new_r[0] = p0_bytes[hdr];
        new_r[4..6].copy_from_slice(&p0_bytes[hdr + 4..hdr + 6]);
        new_r[6..8].copy_from_slice(&p0_bytes[hdr + 6..hdr + 8]);
        // Leaf: right-most pointer stays 0.
        for i in 0..p_n as usize {
            let src = hdr + PAGE_HEADER_SIZE as usize + i * 2;
            let dst = PAGE_HEADER_SIZE as usize + i * 2;
            new_r[dst..dst + 2].copy_from_slice(&p0_bytes[src..src + 2]);
        }
        let ps = p_start as usize;
        if ps > hdr + PAGE_HEADER_SIZE as usize && ps < psz {
            new_r[ps..psz].copy_from_slice(&p0_bytes[ps..psz]);
        }
    }

    // ---- Write both pages back.
    {
        let p0 = pager.get_page(0)?;
        let mut guard = p0.lock();
        guard.data.copy_from_slice(&new_p0);
        remap_zero_children(&mut guard.data, hdr, new_root);
        guard.dirty = true;
    }
    pager.note_dirty(0);
    {
        let rp = pager.get_page(new_root)?;
        let mut guard = rp.lock();
        guard.data.copy_from_slice(&new_r);
        guard.dirty = true;
    }
    pager.note_dirty(new_root);
    Ok(())
}

/// Rewrite every interior cell whose 4-byte left child is 0 to `to` in a
/// page buffer whose btree header starts at `hdr` (100 for page 0, 0 for
/// normal pages). Interior table cells: [4B left_child][varint key].
fn remap_zero_children(data: &mut [u8], hdr: usize, to: u32) {
    let psz = data.len();
    let ptr_base = hdr + PAGE_HEADER_SIZE as usize;
    if ptr_base + 2 > psz {
        return;
    }
    let Ok(pt) = PageType::from_byte(data[hdr]) else {
        return;
    };
    if !matches!(pt, PageType::InteriorTable) {
        return;
    }
    let n = u16::from_be_bytes(data[hdr + 4..hdr + 6].try_into().unwrap()) as usize;
    for i in 0..n {
        let off = ptr_base + i * 2;
        if off + 2 > psz {
            break;
        }
        let cell_ptr = u16::from_be_bytes(data[off..off + 2].try_into().unwrap()) as usize;
        if cell_ptr + 4 <= psz {
            let child = u32::from_be_bytes(data[cell_ptr..cell_ptr + 4].try_into().unwrap());
            if child == 0 {
                data[cell_ptr..cell_ptr + 4].copy_from_slice(&to.to_be_bytes());
            }
        }
    }
}

// ============================================================================
// Page-level compact copy (VACUUM's fast path)
// ============================================================================

use std::collections::HashMap;

/// Copy every live page of the given object trees (table + index roots)
/// into `tmp` — a FRESH in-memory pager — remapping every page reference:
/// interior children, right-most pointers, leaf overflow chain heads, and
/// overflow next links. Pages land in post-order so a parent is written
/// only after its children have their new ids. The freelist is NOT
/// walked: free pages simply vanish (that is the compaction).
///
/// Returns the old->new page id map (roots included).
pub(crate) fn compact_page_copy(
    src: &Pager,
    tmp: &Pager,
    roots: &[u32],
) -> Result<HashMap<u32, u32>> {
    let mut map = HashMap::new();
    for &root in roots {
        if root == 0 {
            continue;
        }
        copy_tree(src, tmp, root, &mut map)?;
    }
    Ok(map)
}

/// Read a source page's bytes (lock held only for the clone).
fn src_page_bytes(src: &Pager, id: u32) -> Result<Vec<u8>> {
    let pr = src.get_page(id)?;
    let g = pr.lock();
    Ok(g.data.clone())
}

/// Write `bytes` into a freshly allocated `tmp` page; returns its id.
fn write_new_page(tmp: &Pager, bytes: &[u8]) -> Result<u32> {
    let mut alloc = tmp.allocate_pages(1)?;
    let (id, pr) = alloc.remove(0);
    {
        let mut g = pr.lock();
        g.data.copy_from_slice(bytes);
        g.dirty = true;
    }
    tmp.note_dirty(id);
    Ok(id)
}

/// Post-order tree copy (see `compact_page_copy`).
fn copy_tree(src: &Pager, tmp: &Pager, page_id: u32, map: &mut HashMap<u32, u32>) -> Result<u32> {
    if let Some(&n) = map.get(&page_id) {
        return Ok(n);
    }
    let psz = src.page_size() as usize;
    let bytes = src_page_bytes(src, page_id)?;
    let pt = PageType::from_byte(bytes[0])
        .map_err(|_| Error::corruption(format!("vacuum copy: bad page type at {page_id}")))?;
    match pt {
        PageType::LeafTable => {
            let mut new_bytes = bytes.clone();
            copy_leaf_overflow_chains(src, tmp, &mut new_bytes, 0, psz, map)?;
            let new_id = write_new_page(tmp, &new_bytes)?;
            map.insert(page_id, new_id);
            Ok(new_id)
        }
        PageType::LeafIndex => {
            // Index cells are fully in-page (no overflow chains in this
            // format — see Cell::decode) and reference no children.
            let new_id = write_new_page(tmp, &bytes)?;
            map.insert(page_id, new_id);
            Ok(new_id)
        }
        PageType::InteriorTable | PageType::InteriorIndex => {
            let n = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
            let right = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
            let ptr_base = PAGE_HEADER_SIZE as usize;
            // Children: each interior cell's first 4 bytes (both table and
            // index layouts) + the right-most pointer.
            let mut children = Vec::with_capacity(n + 1);
            for i in 0..n {
                let off = ptr_base + i * 2;
                if off + 2 > psz {
                    break;
                }
                let cell_ptr = u16::from_be_bytes(bytes[off..off + 2].try_into().unwrap()) as usize;
                if cell_ptr + 4 <= psz {
                    children.push(u32::from_be_bytes(
                        bytes[cell_ptr..cell_ptr + 4].try_into().unwrap(),
                    ));
                }
            }
            if right != 0 {
                children.push(right);
            }
            // Recurse FIRST (post-order): children get their new ids.
            let mut new_children = Vec::with_capacity(children.len());
            for c in children {
                new_children.push(copy_tree(src, tmp, c, map)?);
            }
            let mut new_bytes = bytes.clone();
            let mut ni = 0usize;
            for i in 0..n {
                let off = ptr_base + i * 2;
                if off + 2 > psz {
                    break;
                }
                let cell_ptr =
                    u16::from_be_bytes(new_bytes[off..off + 2].try_into().unwrap()) as usize;
                if cell_ptr + 4 <= psz {
                    new_bytes[cell_ptr..cell_ptr + 4]
                        .copy_from_slice(&new_children[ni].to_be_bytes());
                    ni += 1;
                }
            }
            if right != 0 {
                let rm = new_children[ni];
                new_bytes[8..12].copy_from_slice(&rm.to_be_bytes());
            }
            let new_id = write_new_page(tmp, &new_bytes)?;
            map.insert(page_id, new_id);
            Ok(new_id)
        }
        PageType::Overflow => Err(Error::corruption(format!(
            "vacuum copy: overflow page {page_id} reached as a btree node"
        ))),
    }
}

/// Walk a table leaf's cells, copying overflow chains and patching the
/// 4-byte chain heads in the (soon-to-be-written) page bytes. Cell
/// layout: [varint rowid][varint total][local bytes][4B chain] — the
/// chain pointer exists exactly when total exceeds the local size.
fn copy_leaf_overflow_chains(
    src: &Pager,
    tmp: &Pager,
    page_bytes: &mut [u8],
    hdr: usize,
    psz: usize,
    map: &mut HashMap<u32, u32>,
) -> Result<()> {
    let Ok(pt) = PageType::from_byte(page_bytes[hdr]) else {
        return Ok(());
    };
    if pt != PageType::LeafTable {
        return Ok(());
    }
    let n = u16::from_be_bytes(page_bytes[hdr + 4..hdr + 6].try_into().unwrap()) as usize;
    let ptr_base = hdr + PAGE_HEADER_SIZE as usize;
    for i in 0..n {
        let off = ptr_base + i * 2;
        if off + 2 > psz {
            break;
        }
        let cell_ptr = u16::from_be_bytes(page_bytes[off..off + 2].try_into().unwrap()) as usize;
        // [varint rowid][varint plen]
        let Some((_, n_rid)) =
            crate::storage::btree::varint::decode_signed(&page_bytes[cell_ptr..])
        else {
            continue;
        };
        let p = cell_ptr + n_rid;
        if p >= psz {
            continue;
        }
        let Some((plen, n_plen)) = crate::storage::btree::varint::decode(&page_bytes[p..]) else {
            continue;
        };
        let local_len = crate::storage::btree::overflow_local_len_for(plen as usize, psz);
        if local_len >= plen as usize {
            continue; // fully in-page
        }
        let chain_off = p + n_plen + local_len;
        if chain_off + 4 > psz {
            continue;
        }
        let chain = u32::from_be_bytes(page_bytes[chain_off..chain_off + 4].try_into().unwrap());
        if chain == 0 {
            continue;
        }
        let new_chain = copy_overflow_chain(src, tmp, chain, map)?;
        page_bytes[chain_off..chain_off + 4].copy_from_slice(&new_chain.to_be_bytes());
    }
    Ok(())
}

/// Post-order copy of an overflow chain: the tail first, then this page
/// (its next-pointer patches to the tail's new id).
fn copy_overflow_chain(
    src: &Pager,
    tmp: &Pager,
    page_id: u32,
    map: &mut HashMap<u32, u32>,
) -> Result<u32> {
    if let Some(&n) = map.get(&page_id) {
        return Ok(n);
    }
    let bytes = src_page_bytes(src, page_id)?;
    if bytes.len() < 16 || bytes[0] != PageType::Overflow as u8 {
        return Err(Error::corruption(format!(
            "vacuum copy: overflow chain hit non-overflow page {page_id}"
        )));
    }
    let next = u32::from_be_bytes(bytes[12..16].try_into().unwrap());
    let new_next = if next != 0 {
        copy_overflow_chain(src, tmp, next, map)?
    } else {
        0
    };
    let mut new_bytes = bytes.clone();
    new_bytes[12..16].copy_from_slice(&new_next.to_be_bytes());
    let new_id = write_new_page(tmp, &new_bytes)?;
    map.insert(page_id, new_id);
    Ok(new_id)
}
