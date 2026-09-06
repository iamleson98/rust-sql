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
use crate::storage::pager::{PageIdHashBuild, PageIdSet, Pager};

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

/// A schema-row rootpage patch: the schema tree's leaf cells embed each
/// object's root page id as column 3 of the row record. When VACUUM moves
/// a root, the value is rewritten IN PLACE at the recorded offset — same
/// tag, same byte width (the size class must match, verified at plan
/// time; a mismatch declines the in-place path).
#[derive(Clone, Copy)]
pub(crate) struct SchemaRootPatch {
    /// OLD id of the schema leaf holding the row (the patch applies at
    /// the leaf's NEW slot — resolved via the map at install time).
    pub leaf_old: u32,
    /// Absolute in-page offset of the rootpage value's BODY (after the
    /// tag byte) inside that leaf.
    pub body_off: u32,
    /// Byte width of the size class (1, 2, 4, or 8) — old and new match.
    pub width: u8,
    /// The new root id (little-endian body).
    pub new_root: u32,
}

/// The in-place compaction plan: everything needed to compact without
/// building an image. The map is MONOTONE — pages sorted by old id get
/// new ids 0..n-1 — so `map[o] <= o` for every page, and an ascending
/// new-id move loop can never overwrite a source page before it is read
/// (slot o is only written at step o, and every read of slot o happens
/// at step k <= o).
#[derive(Default)]
pub(crate) struct InPlaceCompaction {
    /// sorted[k] = the OLD id of the page that lands at slot k.
    /// sorted[0] is always 0 (the schema root never moves).
    pub sorted: Vec<u32>,
    /// old id -> new id (identity entries included).
    pub map: HashMap<u32, u32, PageIdHashBuild>,
    /// old id -> [(in-page byte offset, referenced old id)] for every
    /// 4-byte page reference: interior child cells, right-most pointers,
    /// leaf overflow-chain heads, overflow next links. Only pages that
    /// HAVE references carry an entry (most leaves have none — empty
    /// Vecs never allocate).
    pub refs: HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild>,
    /// Schema-row rootpage rewrites (see SchemaRootPatch).
    pub schema_patches: Vec<SchemaRootPatch>,
}

/// Walk every live page — the schema tree (hybrid root at page 0) plus
/// every object tree and overflow chain — and build the in-place
/// compaction plan. Returns None when the plan is not eligible: a
/// schema row's rootpage would change size class (its cell would need
/// re-encoding), or a schema row spans an overflow chain (the column's
/// offset is not stable). The caller falls back to the image path.
/// Read-only: pages are fetched (cache hits for anything warm) but never
/// mutated.
pub(crate) fn plan_in_place_compaction(
    src: &Pager,
    roots: &[u32],
) -> Result<Option<InPlaceCompaction>> {
    let old_n = src.n_pages();
    let mut seen: PageIdSet = std::collections::HashSet::default();
    let mut refs: HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild> = HashMap::default();
    // Raw schema-row rootpage sites: (schema leaf old id, body offset,
    // old root value) — collected during the schema-tree walk, resolved
    // against the map below.
    let mut schema_sites: Vec<(u32, u32, i64)> = Vec::new();
    // Pass 1: walk every tree. INTERIORS are fully parsed (their child
    // references are the patch surface). OBJECT-tree leaves are NOT
    // cell-parsed yet — the live set comes from the freelist complement
    // below, and chain-head patches are only collected when overflow
    // pages actually exist (pass 2). Schema-tree leaves are always
    // cell-parsed: their cells embed the rootpage columns.
    collect_tree(src, 0, true, &mut seen, &mut refs, &mut schema_sites, true)?;
    for &root in roots {
        if root == 0 {
            continue;
        }
        collect_tree(
            src,
            root,
            false,
            &mut seen,
            &mut refs,
            &mut schema_sites,
            false,
        )?;
    }
    // Pass 2: the LIVE SET = every page id below old_n that is NOT on
    // the freelist (the engine's invariant: allocated pages are either
    // in live use or on the freelist — btree pages, overflow chains,
    // schema pages, or leaked-but-allocated). Walking the freelist is
    // O(#free pages) — for churn shapes a handful of trunk reads —
    // versus cell-parsing every leaf of every table.
    let free = read_freelist(src)?;
    // Sanity: no btree page may be on the freelist (corruption or a
    // stale freelist — decline to the image path).
    if seen.iter().any(|id| free.contains(id)) {
        return Ok(None);
    }
    let sorted: Vec<u32> = (0..old_n).filter(|id| !free.contains(id)).collect();
    // A live set smaller than the walked btree set is contradictory.
    if sorted.len() < seen.len() {
        return Ok(None);
    }
    let map: HashMap<u32, u32, PageIdHashBuild> = sorted
        .iter()
        .enumerate()
        .map(|(i, &o)| (o, i as u32))
        .collect();
    // Overflow pages exist iff the live set exceeds the walked btree
    // set. Only then are object-tree leaves cell-parsed (chain-head
    // patches) and overflow next-links recorded — and only when
    // something actually moves (an identity map needs no patches).
    let identity = sorted.iter().enumerate().all(|(i, &o)| i as u32 == o);
    let has_overflow = sorted.len() > seen.len();
    if has_overflow && !identity {
        // Re-walk object-tree leaves for chain heads (pass 1 skipped
        // them). Overflow next-links: recorded for every live page the
        // btree walk did not visit.
        for &root in roots {
            if root == 0 {
                continue;
            }
            parse_leaf_chains(src, root, &mut refs)?;
        }
        for &id in &sorted {
            if !seen.contains(&id) {
                // An unvisited live page: an overflow chain member (or a
                // benign leak — recording a (12, next) ref for a
                // non-overflow page is a harmless no-op patch).
                let pr = src.get_page(id)?;
                let next = {
                    let b = pr.lock();
                    if b.data.len() >= 16 {
                        u32::from_be_bytes(b.data[12..16].try_into().unwrap())
                    } else {
                        0
                    }
                };
                drop(pr);
                if next != 0 && !refs.contains_key(&id) {
                    refs.insert(id, vec![(12, next)]);
                }
            }
        }
    }
    // Resolve schema sites into patches. Every referenced root must be
    // live (reachable — it IS in the map) and the new id must fit the
    // old value's size class.
    let mut schema_patches: Vec<SchemaRootPatch> = Vec::with_capacity(schema_sites.len());
    for &(leaf_old, body_off, old_root) in &schema_sites {
        let Some(&new_root) = map.get(&(old_root as u32)) else {
            return Ok(None); // root unreachable: not a shape we patch
        };
        if new_root as i64 == old_root {
            continue; // identity: nothing to rewrite
        }
        if int_class(old_root) != int_class(new_root as i64) {
            return Ok(None); // size-class change: re-encode territory
        }
        schema_patches.push(SchemaRootPatch {
            leaf_old,
            body_off,
            width: int_class(new_root as i64) as u8,
            new_root,
        });
    }
    Ok(Some(InPlaceCompaction {
        sorted,
        map,
        refs,
        schema_patches,
    }))
}

/// Read every page id on the freelist (trunk-chain walk with a cycle
/// budget). The freelist's own trunk pages are themselves free.
fn read_freelist(src: &Pager) -> Result<PageIdSet> {
    let mut free: PageIdSet = std::collections::HashSet::default();
    let mut cur = src.freelist_head();
    let n = src.n_pages();
    let mut budget = src.freelist_count() as u64 + n as u64 + 1;
    while cur != 0 && budget > 0 {
        budget -= 1;
        let pr = src.get_page(cur)?;
        let (next, kn, entries) = {
            let b = pr.lock();
            let next = u32::from_le_bytes(b.data[..4].try_into().unwrap_or([0; 4]));
            let kn = u32::from_le_bytes(b.data[4..8].try_into().unwrap_or([0; 4])) as usize;
            let kn = kn.min((b.data.len().saturating_sub(8)) / 4);
            let entries: Vec<u32> = (0..kn)
                .map(|i| {
                    u32::from_le_bytes(b.data[8 + i * 4..12 + i * 4].try_into().unwrap_or([0; 4]))
                })
                .collect();
            (next, kn, entries)
        };
        drop(pr);
        free.insert(cur);
        for e in entries {
            free.insert(e);
        }
        let _ = kn;
        cur = next;
    }
    Ok(free)
}

/// Pass-2 leaf chain scan for OBJECT trees: cell-parse every leaf of the
/// tree rooted at `root`, recording (chain_off, chain) patches. Uses its
/// OWN visited set — pass 1 already marked the btree pages in `seen`
/// (without chain parsing), so reusing it here would skip every leaf and
/// leave overflow chains unpatched (dangling pointers after moves).
fn parse_leaf_chains(
    src: &Pager,
    root: u32,
    refs: &mut HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild>,
) -> Result<()> {
    let mut visited: PageIdSet = std::collections::HashSet::default();
    let mut stack = vec![root];
    let psz = src.page_size() as usize;
    while let Some(page_id) = stack.pop() {
        if !visited.insert(page_id) {
            continue;
        }
        let pr = src.get_page(page_id)?;
        let pt = {
            let b = pr.lock();
            b.page_type()?
        };
        match pt {
            PageType::LeafTable => {
                let mut chains: Vec<u32> = Vec::new();
                {
                    let b = pr.lock();
                    let n = b.n_cells() as usize;
                    for i in 0..n {
                        let cell_ptr = b.cell_pointer(i as u16) as usize;
                        if cell_ptr >= b.data.len() {
                            continue;
                        }
                        let Some((_, n_rid)) =
                            crate::storage::btree::varint::decode_signed(&b.data[cell_ptr..])
                        else {
                            continue;
                        };
                        let p = cell_ptr + n_rid;
                        if p >= b.data.len() {
                            continue;
                        }
                        let Some((plen, n_plen)) =
                            crate::storage::btree::varint::decode(&b.data[p..])
                        else {
                            continue;
                        };
                        let payload_start = p + n_plen;
                        let local_len =
                            crate::storage::btree::overflow_local_len_for(plen as usize, psz);
                        if local_len < plen as usize {
                            let chain_off = payload_start + local_len;
                            if chain_off + 4 > b.data.len() {
                                continue;
                            }
                            let chain = u32::from_be_bytes(
                                b.data[chain_off..chain_off + 4].try_into().unwrap(),
                            );
                            if chain != 0 {
                                chains.push(chain);
                                refs.entry(page_id)
                                    .or_default()
                                    .push((chain_off as u32, chain));
                            }
                        }
                    }
                }
                drop(pr);
                for chain in chains {
                    // Walk the chain recording next-links (the install's
                    // patcher rewrites them when the target moved).
                    let mut cur = chain;
                    let mut budget = src.n_pages() as u64 + 1;
                    while cur != 0 && budget > 0 {
                        budget -= 1;
                        if !visited.insert(cur) {
                            break;
                        }
                        let p = src.get_page(cur)?;
                        let next = {
                            let b = p.lock();
                            if b.data.len() >= 16 {
                                u32::from_be_bytes(b.data[12..16].try_into().unwrap())
                            } else {
                                0
                            }
                        };
                        drop(p);
                        if next != 0 {
                            refs.entry(cur).or_default().push((12, next));
                        }
                        cur = next;
                    }
                }
            }
            PageType::LeafIndex => {}
            PageType::InteriorTable | PageType::InteriorIndex => {
                let mut children: Vec<u32> = Vec::new();
                {
                    let b = pr.lock();
                    let right = b.right_most_pointer();
                    let n = b.n_cells() as usize;
                    for i in 0..n {
                        let cell_ptr = b.cell_pointer(i as u16) as usize;
                        if cell_ptr + 4 <= b.data.len() {
                            children.push(u32::from_be_bytes(
                                b.data[cell_ptr..cell_ptr + 4].try_into().unwrap(),
                            ));
                        }
                    }
                    if right != 0 {
                        children.push(right);
                    }
                }
                drop(pr);
                stack.extend(children);
            }
            PageType::Overflow => {}
        }
    }
    Ok(())
}

/// Value::encode_into's integer size classes: byte width of the BODY
/// (0x01 zero -> 0, 0x02 i8 -> 1, 0x03 i16 -> 2, 0x04 i32 -> 4,
/// 0x05 i64 -> 8; see types/value.rs).
fn int_class(v: i64) -> usize {
    if v == 0 {
        0
    } else if v >= i8::MIN as i64 && v <= i8::MAX as i64 {
        1
    } else if v >= i16::MIN as i64 && v <= i16::MAX as i64 {
        2
    } else if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
        4
    } else {
        8
    }
}

/// Read-only tree walk collecting page ids (btree pages + leaf overflow
/// chains). Already-seen pages short-circuit (guards cycles). Page 0's
/// hybrid header is handled by the Page accessors' header_offset.
fn collect_tree(
    src: &Pager,
    page_id: u32,
    is_schema_tree: bool,
    seen: &mut PageIdSet,
    refs: &mut HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild>,
    schema_sites: &mut Vec<(u32, u32, i64)>,
    parse_object_leaf_chains: bool,
) -> Result<()> {
    if !seen.insert(page_id) {
        return Ok(());
    }
    let psz = src.page_size() as usize;
    let hdr = if page_id == 0 {
        DB_HEADER_SIZE as usize
    } else {
        0
    };
    let pr = src.get_page(page_id)?;
    let pt = {
        let b = pr.lock();
        b.page_type()?
    };
    match pt {
        PageType::LeafTable => {
            // Cell parsing happens for the SCHEMA tree (rootpage sites +
            // chain refs — the schema tree is small) and for OBJECT
            // leaves only when chain patches may be needed (pass 2 after
            // overflow pages were detected). Otherwise the leaf is just
            // a live page id — no per-cell work (the live set comes from
            // the freelist complement, not the walk).
            if !is_schema_tree && !parse_object_leaf_chains {
                return Ok(());
            }
            // Overflow chains hang off leaf cells: [varint rowid]
            // [varint plen][local][4B chain]. Record (chain_off, chain)
            // for the patcher.
            let mut chains: Vec<u32> = Vec::new();
            let mut page_refs: Vec<(u32, u32)> = Vec::new();
            {
                let b = pr.lock();
                let n = b.n_cells() as usize;
                for i in 0..n {
                    let cell_ptr = b.cell_pointer(i as u16) as usize;
                    if cell_ptr >= b.data.len() {
                        continue;
                    }
                    let Some((_, n_rid)) =
                        crate::storage::btree::varint::decode_signed(&b.data[cell_ptr..])
                    else {
                        continue;
                    };
                    let p = cell_ptr + n_rid;
                    if p >= b.data.len() {
                        continue;
                    }
                    let Some((plen, n_plen)) = crate::storage::btree::varint::decode(&b.data[p..])
                    else {
                        continue;
                    };
                    let payload_start = p + n_plen;
                    let local_len =
                        crate::storage::btree::overflow_local_len_for(plen as usize, psz);
                    if local_len < plen as usize {
                        let chain_off = payload_start + local_len;
                        if chain_off + 4 > b.data.len() {
                            continue;
                        }
                        let chain = u32::from_be_bytes(
                            b.data[chain_off..chain_off + 4].try_into().unwrap(),
                        );
                        if chain != 0 {
                            chains.push(chain);
                            page_refs.push((chain_off as u32, chain));
                        }
                        if is_schema_tree {
                            // A schema row spanning an overflow chain has
                            // no stable in-page column offsets — decline
                            // the in-place path for that shape.
                            schema_sites.push((page_id, u32::MAX, -1));
                        }
                        continue;
                    }
                    // Schema-tree leaf: locate column 3's (rootpage) body
                    // offset for the in-place root patch. The row record
                    // is [type TEXT][name TEXT][tbl_name (TEXT|NULL)]
                    // [rootpage INT][sql TEXT] — each value is
                    // [tag][body] (see Value::encode_into).
                    if is_schema_tree {
                        if let Some((body_off, root)) =
                            schema_rootpage_site(&b.data[payload_start..])
                        {
                            schema_sites.push((page_id, (payload_start + body_off) as u32, root));
                        } else {
                            // Unparseable row: decline.
                            schema_sites.push((page_id, u32::MAX, -1));
                        }
                    }
                }
            }
            drop(pr);
            if !page_refs.is_empty() {
                refs.insert(page_id, page_refs);
            }
            for chain in chains {
                collect_overflow(src, chain, seen, refs)?;
            }
            Ok(())
        }
        PageType::LeafIndex => Ok(()), // fully in-page, no children
        PageType::InteriorTable | PageType::InteriorIndex => {
            // References: each cell's 4-byte left child + the right-most
            // pointer at hdr + 8.
            let mut page_refs: Vec<(u32, u32)> = Vec::new();
            let right;
            {
                let b = pr.lock();
                let n = b.n_cells() as usize;
                right = b.right_most_pointer();
                for i in 0..n {
                    let cell_ptr = b.cell_pointer(i as u16) as usize;
                    if cell_ptr + 4 <= b.data.len() {
                        page_refs.push((
                            cell_ptr as u32,
                            u32::from_be_bytes(b.data[cell_ptr..cell_ptr + 4].try_into().unwrap()),
                        ));
                    }
                }
            }
            drop(pr);
            if right != 0 {
                page_refs.push(((hdr + 8) as u32, right));
            }
            let children: Vec<u32> = page_refs.iter().map(|&(_, r)| r).collect();
            refs.insert(page_id, page_refs);
            for c in children {
                collect_tree(
                    src,
                    c,
                    is_schema_tree,
                    seen,
                    refs,
                    schema_sites,
                    parse_object_leaf_chains,
                )?;
            }
            Ok(())
        }
        PageType::Overflow => Ok(()), // reached only via collect_overflow
    }
}

/// Locate column 3's (rootpage) BODY offset inside a schema row's
/// record payload, plus its current integer value. Skips the three
/// leading columns generically (any tag layout Value::encode_into can
/// produce). Returns None on any parse failure (caller declines).
fn schema_rootpage_site(payload: &[u8]) -> Option<(usize, i64)> {
    let mut off = 0usize;
    for _ in 0..3 {
        off += value_span(&payload[off..])?;
    }
    let tag = *payload.get(off)?;
    let body = off + 1;
    let read_body = |n: usize, signed: bool| -> Option<i64> {
        let raw = payload.get(body..body + n)?;
        let mut v: i64 = 0;
        for (i, &b) in raw.iter().enumerate() {
            v |= (b as i64) << (8 * i); // little-endian bodies
        }
        if signed {
            // Sign-extend from the top bit of the last byte.
            let bits = 8 * n;
            if bits < 64 && (v >> (bits - 1)) & 1 == 1 {
                v -= 1i64 << bits;
            }
        }
        Some(v)
    };
    let (val, width) = match tag {
        0x01 => (0, 0),
        0x02 => (read_body(1, true)?, 1),
        0x03 => (read_body(2, true)?, 2),
        0x04 => (read_body(4, true)?, 4),
        0x05 => (read_body(8, true)?, 8),
        _ => return None, // column 3 is not an integer: unexpected shape
    };
    let _ = width;
    Some((body, val))
}

/// Total encoded byte length of the value starting at `bytes[0]` (tag
/// first — see Value::encode_into).
fn value_span(bytes: &[u8]) -> Option<usize> {
    let tag = *bytes.first()?;
    match tag {
        0x00 | 0x01 | 0x09 => Some(1),
        0x02 => Some(2),
        0x03 => Some(3),
        0x06 => Some(9),
        0x04 => Some(5),
        0x05 => Some(9),
        0x07 | 0x08 => {
            let (len, n) = crate::storage::btree::varint::decode(&bytes[1..])?;
            Some(1 + n + len as usize)
        }
        0x0A => {
            let (_, n) = crate::storage::btree::varint::decode(&bytes[1..])?;
            Some(1 + n)
        }
        _ => None,
    }
}

/// ITERATIVE overflow-chain walk (chains can be thousands of pages deep —
/// recursion would risk the stack). Next-pointer at [12..16] (see
/// copy_overflow_chain).
fn collect_overflow(
    src: &Pager,
    head: u32,
    seen: &mut PageIdSet,
    refs: &mut HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild>,
) -> Result<()> {
    let mut cur = head;
    // Cycle guard: a chain longer than the page count is corrupt.
    let mut budget = src.n_pages() as u64 + 1;
    while cur != 0 && budget > 0 {
        budget -= 1;
        if !seen.insert(cur) {
            return Ok(());
        }
        let pr = src.get_page(cur)?;
        let next = {
            let b = pr.lock();
            if b.data.len() >= 16 {
                u32::from_be_bytes(b.data[12..16].try_into().unwrap())
            } else {
                0
            }
        };
        drop(pr);
        if next != 0 {
            refs.entry(cur).or_default().push((12, next));
        }
        cur = next;
    }
    Ok(())
}

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
