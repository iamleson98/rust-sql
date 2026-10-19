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
use crate::storage::btree::{index_leaf_cell_size, table_leaf_cell_size_paged};
use crate::storage::page::{PageType, DB_HEADER_SIZE, PAGE_HEADER_SIZE};
use crate::storage::pager::{PageIdHashBuild, PageIdSet, Pager};

/// Pull the schema btree's root back into page 0 after it moved to
/// `new_root` (an INTERIOR page produced by a split).
///
/// Callers: after any mutation through `Btree::new(pager, 0, ..)`, check
/// `bt.root != 0` and invoke this. Safe under the engine's single-writer
/// discipline.
/// Decline-reason instrumentation for the in-place planner's own
/// decision points (the vacuum's timing env — same flag as the
/// eligibility walk's `decline!` macro above). Zero cost when off.
macro_rules! vac_plan_dbg {
    ($($arg:tt)*) => {
        if std::env::var_os("RSQL_VAC_TIMING").is_some() {
            eprintln!("[vac] in-place plan decline: {}", format!($($arg)*));
        }
    };
}

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
        guard.touch();
    }
    pager.note_dirty(0);
    {
        let rp = pager.get_page(new_root)?;
        let mut guard = rp.lock();
        guard.data.copy_from_slice(&new_r);
        guard.touch();
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

/// Rebuild a schema-tree LEAF in place with re-encoded cells: the
/// patches replace whole cell payloads (rootpage re-encode — the new
/// record may differ in width from the old); every other cell keeps its
/// bytes verbatim. The leaf is re-laid compactly: cells placed
/// sequentially from the page end, pointer array rewritten in the SAME
/// order (rowids and cell index stability are the patch contract),
/// freeblock list and fragment counter reset. Byte-level by design —
/// the in-place install runs below the btree machinery (one flush at
/// the end); schema leaves are small (tens of cells, no overflow —
/// overflowing schema rows decline the plan upstream).
///
/// `hdr` is the btree header offset (100 for page 0's hybrid layout, 0
/// otherwise). Fails loudly (Error::corruption) on malformed cells —
/// the eligibility walk has already size-validated every leaf, so a
/// failure here means on-disk corruption, not a shape decision.
pub(crate) fn rebuild_schema_leaf(
    data: &mut [u8],
    hdr: usize,
    patches: &[(u16, Vec<u8>)],
) -> Result<()> {
    // Page layout (see Page's accessors and compact_leaf_if_fragmented —
    // the engine's own leaf relayout): [type:1][freeblock region:3]
    // [n_cells:2 BE @+4][content_start:2 BE @+6, 0 = page_size]
    // [right-most ptr:4 @+8, reserved on leaves][ptr array @+12].
    let psz = data.len();
    let ptr_base = hdr + PAGE_HEADER_SIZE as usize;
    if ptr_base + 2 > psz {
        return Err(Error::corruption(
            "schema leaf rebuild: no room for pointer array",
        ));
    }
    let n = u16::from_be_bytes(data[hdr + 4..hdr + 6].try_into().unwrap()) as usize;
    if ptr_base + 2 * n > psz {
        return Err(Error::corruption(
            "schema leaf rebuild: pointer array overruns page",
        ));
    }
    // Pass 1: materialize every cell's new bytes (patched payloads
    // re-prefixed with their ORIGINAL [rowid varint][length varint]
    // headers, untouched cells copied verbatim).
    let mut new_cells: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut total = 0usize;
    for i in 0..n {
        let off = ptr_base + i * 2;
        let cell_ptr = u16::from_be_bytes(data[off..off + 2].try_into().unwrap()) as usize;
        if cell_ptr >= psz {
            return Err(Error::corruption(
                "schema leaf rebuild: cell pointer out of range",
            ));
        }
        let Some((rowid, n_rid)) = crate::storage::btree::varint::decode_signed(&data[cell_ptr..])
        else {
            return Err(Error::corruption("schema leaf rebuild: undecodable rowid"));
        };
        let payload: &[u8] =
            if let Some((_, new_payload)) = patches.iter().find(|(idx, _)| *idx as usize == i) {
                new_payload.as_slice()
            } else {
                let p = cell_ptr + n_rid;
                let Some((plen, n_plen)) = crate::storage::btree::varint::decode(&data[p..]) else {
                    return Err(Error::corruption("schema leaf rebuild: undecodable length"));
                };
                let plen = plen as usize;
                let local = crate::storage::btree::overflow_local_len_for(plen, psz);
                if local < plen {
                    return Err(Error::corruption(
                        "schema leaf rebuild: overflowing schema cell (plan must decline)",
                    ));
                }
                let start = p + n_plen;
                let end = start + plen;
                if end > psz {
                    return Err(Error::corruption(
                        "schema leaf rebuild: payload overruns page",
                    ));
                }
                &data[start..end]
            };
        let mut cell = Vec::with_capacity(n_rid + 3 + payload.len());
        let mut vbuf = [0u8; 9];
        let n_enc = crate::storage::btree::varint::encode(rowid as u64, &mut vbuf);
        cell.extend_from_slice(&vbuf[..n_enc]);
        // Schema rowids are engine-assigned sequence numbers (>= 1);
        // the signed varint round-trip only matters for negatives, which
        // cannot occur here.
        let n_enc = crate::storage::btree::varint::encode(payload.len() as u64, &mut vbuf);
        cell.extend_from_slice(&vbuf[..n_enc]);
        cell.extend_from_slice(payload);
        total += cell.len();
        new_cells.push(cell);
    }
    // Pass 2: re-lay compactly from the page end (the engine's own
    // convention — compact_leaf_if_fragmented writes cells tail-down in
    // pointer order; ascending-from-content_start is the same layout).
    let content_start = psz
        .checked_sub(total)
        .filter(|cs| *cs >= ptr_base + 2 * n)
        .ok_or_else(|| Error::corruption("schema leaf rebuild: cells do not fit page"))?;
    let mut cursor = content_start;
    for (i, cell) in new_cells.iter().enumerate() {
        let end = cursor + cell.len();
        data[cursor..end].copy_from_slice(cell);
        let off = ptr_base + i * 2;
        data[off] = (cursor >> 8) as u8;
        data[off + 1] = (cursor & 0xFF) as u8;
        cursor = end;
    }
    // Header: freeblock region zeroed (the engine keeps no chain —
    // fragmentation compacts instead), n_cells unchanged (same cells,
    // same order), content start stored with the 0 = page_size
    // convention of set_cell_content_start.
    data[hdr + 1..hdr + 4].fill(0);
    let cs_stored = if content_start >= psz {
        0u16
    } else {
        content_start as u16
    };
    data[hdr + 6..hdr + 8].copy_from_slice(&cs_stored.to_be_bytes());
    Ok(())
}

// ============================================================================
// Page-level compact copy (VACUUM's fast path)
// ============================================================================

use std::collections::HashMap;

/// A schema-row cell patch: the schema tree's leaf cells embed each
/// object's root page id as column 3 of the row record. When VACUUM moves
/// a root, the row is RE-ENCODED with the new rootpage (the value may
/// change size class — e.g. a root promoted to a 2-byte id during bulk
/// load that compacts back into the 1-byte range) and the affected
/// schema leaf is rebuilt compactly at its new slot (see
/// [`rebuild_schema_leaf`]). The old in-place byte patch could only
/// write same-width bodies — and looked the RAW (SQLite 1-based)
/// rootpage up in the INTERNAL (0-based) id map, so any object whose
/// root+1 page died spuriously declined the whole in-place path (the
/// mass-delete-then-VACUUM shape — torture S14 — always fell to the
/// temp-file image rebuild).
#[derive(Clone)]
pub(crate) struct SchemaCellPatch {
    /// OLD id of the schema leaf holding the row (the patch applies at
    /// the leaf's NEW slot — resolved via the map at install time).
    pub leaf_old: u32,
    /// Index of the cell in the leaf's pointer array (order-preserving
    /// across the rebuild).
    pub cell_idx: u16,
    /// The re-encoded row record payload (WITHOUT the cell's
    /// [rowid varint][payload-length varint] prefix — the rebuild
    /// re-adds those from the original cell).
    pub payload: Vec<u8>,
}

/// One schema-tree leaf cell as collected by the walk: enough to
/// re-encode the row at plan time. `ok = false` marks shapes the
/// in-place path cannot patch (the row spans an overflow chain, or the
/// record is undecodable) — the plan declines on those.
pub(crate) struct SchemaCellRaw {
    /// OLD id of the schema leaf holding the row.
    pub page: u32,
    /// Cell index in the leaf's pointer array.
    pub cell_idx: u16,
    /// The row record payload bytes ([type][name][tbl_name][rootpage]
    /// [sql] — see Value::encode_into).
    pub payload: Vec<u8>,
    /// False: overflow-spanning or undecodable — decline the plan.
    pub ok: bool,
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
    /// Schema-row rootpage re-encodes (see SchemaCellPatch).
    pub schema_patches: Vec<SchemaCellPatch>,
    /// OLD ids of every schema LEAF to re-lay at install time: patched
    /// leaves plus any leaf that is not already packed (the eligibility
    /// gate exempts schema leaves from the hollow/empty/waste guards
    /// precisely because the rebuild re-lays them densely — matching
    /// the image paths, which re-insert the schema rows into a fresh
    /// tree). A split schema tree's half-filled leaf would otherwise
    /// force the whole file onto the row-level rebuild.
    pub schema_rebuild_leaves: Vec<u32>,
}

/// VACUUM fast-path eligibility walk: every leaf of every tree must be
/// dense enough that a page-level path (the in-place WAL install or the
/// verbatim image copy) reclaims all the waste there is. Policy per
/// leaf, O(cells) — the LIVE bytes are the pointer-array cell sum with
/// the overflow-aware size helpers (the same accounting
/// `compact_leaf_if_fragmented` uses); the content area alone is NOT a
/// live-bytes bound, because `remove_cell_at` drops the pointer but
/// leaves the dead cell bytes inside the content area (inserts reclaim
/// them lazily):
///   - a 0-cell leaf (this engine's mass-delete signature — DELETE
///     never detaches pages, emptied leaves stay wired in) is PRUNABLE
///     by the image copier; `allow_empty = false` (the in-place plan,
///     which cannot drop pages) declines on it.
///   - a leaf whose content area is under half the page is HOLLOW —
///     survivor leaves rebuilt at a low fill fraction by mass deletes.
///     Only the row-level rebuild re-flows those densely; both fast
///     paths decline.
///   - WASTE BUDGET (file-wide, the third line): a leaf's TRUE slack is
///     (page bytes minus header/pointer array minus live cells) —
///     exactly what a row-level re-flow of that page would free, gap
///     AND dead content bytes both. A file can sit ABOVE the hollow
///     line on every leaf and still waste a large slice of its
///     footprint: a dense bulk load minus an INTERLEAVED mass delete
///     (id % 10 < 4) leaves every page ~60% live with a zero freelist
///     and no 0-cell leaf — the macOS M6 draw of run 37597765597
///     ("freelist=0 pages (of 217) | VACUUM 2ms -> 888864 bytes
///     (reclaimed 0.0%)"). Both fast paths decline when the collected
///     slack exceeds two frontier pages per tree plus 10% of the leaf
///     footprint; the row-level rebuild runs and re-flows densely.
pub(crate) fn trees_eligible_for_page_copy(
    src: &Pager,
    roots: &[u32],
    allow_empty: bool,
) -> Result<bool> {
    let psz = src.page_size() as usize;
    // Waste-budget accumulators: the leaf footprint (page count) and
    // the total TRUE free slack a row-level re-flow would reclaim (see
    // the policy note above).
    let mut leaf_pages: usize = 0;
    let mut leaf_slack_bytes: usize = 0;
    // Decline-reason instrumentation (the vacuum's own timing env):
    // which guard fired, with the walk's running totals. A macro (not
    // a closure) so the accumulators stay unborrowed and mutable.
    let vac_dbg = std::env::var_os("RSQL_VAC_TIMING").is_some();
    macro_rules! decline {
        ($why:expr, $n:expr) => {
            if vac_dbg {
                eprintln!(
                    "[vac] page-copy eligibility decline: {} \
                     (n_cells={}, leaf_pages={}, leaf_slack_bytes={})",
                    $why, $n, leaf_pages, leaf_slack_bytes
                );
            }
        };
    }
    let mut visited: PageIdSet = std::collections::HashSet::default();
    let mut stack: Vec<u32> = roots.iter().copied().filter(|&r| r != 0).collect();
    while let Some(page_id) = stack.pop() {
        if !visited.insert(page_id) {
            continue;
        }
        let pr = src.get_page(page_id)?;
        let pt = {
            let b = pr.lock();
            b.page_type()?
        };
        let hdr = if page_id == 0 {
            DB_HEADER_SIZE as usize
        } else {
            0
        };
        match pt {
            PageType::LeafTable | PageType::LeafIndex => {
                // (decline_reason, true_slack, n_cells): the
                // slack is what a row-level re-flow of THIS page would
                // free — the gap under the content start PLUS the dead
                // cell bytes inside the content area (the live-byte sum
                // is the compaction path's own accounting).
                let (reason, slack, n) = {
                    let b = pr.lock();
                    let n = b.n_cells() as usize;
                    if n == 0 {
                        (
                            if allow_empty {
                                None
                            } else {
                                Some("empty leaf")
                            },
                            0,
                            n,
                        )
                    } else {
                        let content_start = b.cell_content_start() as usize;
                        let ptr_end = hdr + PAGE_HEADER_SIZE as usize + n * 2;
                        if content_start < ptr_end || content_start > psz {
                            (Some("malformed leaf"), 0, n)
                            // not a shape we fast-path
                        } else {
                            let mut live = 0usize;
                            let mut live_ok = true;
                            for i in 0..n as u16 {
                                let p = b.cell_pointer(i) as usize;
                                let size = match pt {
                                    PageType::LeafIndex => {
                                        index_leaf_cell_size(&b.data[p..], psz as u32)
                                    }
                                    _ => table_leaf_cell_size_paged(&b.data[p..], psz as u32),
                                };
                                match size {
                                    Some(s) if s > 0 && p + s <= psz => live += s,
                                    _ => {
                                        live_ok = false;
                                        break;
                                    }
                                }
                            }
                            if !live_ok {
                                (Some("unreadable cells"), 0, n)
                            } else {
                                let content_area = psz - content_start;
                                let usable = psz - ptr_end;
                                let slack = (psz - ptr_end).saturating_sub(live);
                                let hollow = content_area * 2 < usable;
                                (if hollow { Some("hollow leaf") } else { None }, slack, n)
                            }
                        }
                    }
                };
                drop(pr);
                if let Some(why) = reason {
                    decline!(why, n);
                    return Ok(false);
                }
                leaf_pages += 1;
                leaf_slack_bytes += slack;
            }
            PageType::InteriorTable | PageType::InteriorIndex => {
                let mut children = Vec::new();
                {
                    let b = pr.lock();
                    let n = b.n_cells() as usize;
                    let right = b.right_most_pointer();
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
    // WASTE BUDGET: the page-level result keeps every non-empty leaf
    // whole, so the collected TRUE slack (gap + dead content bytes)
    // stays wasted in it. VACUUM's contract is a fully compact image —
    // decline when the slack is a significant slice of the leaf
    // footprint and let the row-level rebuild re-flow. Allowance: two
    // pages per tree (a dense rebuild's own frontier pages are
    // partially filled by construction) plus a 10% relative margin for
    // steady-state fill variance.
    let slack_allowance =
        (roots.len().saturating_mul(2) + 2).saturating_mul(psz) + leaf_pages * psz / 10;
    if leaf_slack_bytes > slack_allowance {
        if vac_dbg {
            eprintln!(
                "[vac] page-copy eligibility decline: waste budget \
                 (leaf_pages={leaf_pages}, leaf_slack_bytes={leaf_slack_bytes}, \
                 allowance={slack_allowance})"
            );
        }
        return Ok(false);
    }
    if vac_dbg {
        eprintln!(
            "[vac] page-copy eligibility ok: leaf_pages={leaf_pages}, \
             leaf_slack_bytes={leaf_slack_bytes}, allowance={slack_allowance}"
        );
    }
    Ok(true)
}

/// Walk the SCHEMA tree (rooted at hybrid page 0) and collect its LEAF
/// page ids, verifying the one hard precondition of the install's
/// schema-leaf rebuild: every cell of every leaf must be size-parseable
/// (rebuild_schema_leaf re-prefixes and re-places each cell). Returns
/// None on any unparseable cell — the in-place plan declines to the
/// image path. Shape properties (empty / hollow / slack) are NOT
/// checked: the rebuild re-lays every schema leaf compactly, so a
/// split schema tree's half-filled frontier leaf does not disqualify
/// the file (the object trees' strict gate stays in
/// trees_eligible_for_page_copy).
fn schema_tree_leaves_rebuildable(src: &Pager) -> Result<Option<Vec<u32>>> {
    let psz = src.page_size() as usize;
    let mut leaves: Vec<u32> = Vec::new();
    let mut visited: PageIdSet = std::collections::HashSet::default();
    let mut stack: Vec<u32> = vec![0];
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
                let ok = {
                    let b = pr.lock();
                    let n = b.n_cells() as usize;
                    let mut ok = true;
                    for i in 0..n as u16 {
                        let p = b.cell_pointer(i) as usize;
                        if p >= psz {
                            ok = false;
                            break;
                        }
                        match table_leaf_cell_size_paged(&b.data[p..], psz as u32) {
                            Some(sz) if sz > 0 && p + sz <= psz => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    ok
                };
                drop(pr);
                if !ok {
                    return Ok(None);
                }
                leaves.push(page_id);
            }
            PageType::InteriorTable => {
                let mut children = Vec::new();
                {
                    let b = pr.lock();
                    let n = b.n_cells() as usize;
                    let right = b.right_most_pointer();
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
            _ => {}
        }
    }
    Ok(Some(leaves))
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
    let psz = src.page_size() as usize;
    // Eligibility: the in-place plan moves pages but cannot drop or
    // re-pack any. Mass-delete shapes (empty leaves, hollow survivors)
    // must take an image/row-level path — see
    // trees_eligible_for_page_copy. The schema tree (page 0) is
    // included: the in-place path patches it in place, while the image
    // paths rebuild it densely from the rows.
    // Two-tier gate. The SCHEMA tree is checked by a dedicated walk
    // (schema_tree_leaves_rebuildable): its leaves are RE-LAID compactly
    // by the install (schema_rebuild_leaves), so the shape guards (empty
    // / hollow / waste budget) do not apply to it — only the structural
    // requirement that every cell be parseable (the rebuild's own hard
    // precondition). A split schema tree's half-filled frontier leaf
    // must not push the whole file onto the row-level rebuild. The
    // OBJECT trees keep the strict policy: in-place moves pages but
    // cannot re-flow them (the walker's historical 0-filter means the
    // old combined call never actually gated the schema tree — the
    // schema walk below is the first real schema gate).
    let schema_rebuild_leaves = match schema_tree_leaves_rebuildable(src)? {
        Some(leaves) => leaves,
        None => {
            vac_plan_dbg!("schema tree not rebuildable (unparseable cell)");
            return Ok(None);
        }
    };
    if !trees_eligible_for_page_copy(src, roots, false)? {
        return Ok(None);
    }
    let mut seen: PageIdSet = std::collections::HashSet::default();
    let mut refs: HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild> = HashMap::default();
    // Raw schema-row cells — collected during the schema-tree walk,
    // re-encoded against the map below.
    let mut schema_cells: Vec<SchemaCellRaw> = Vec::new();
    // Pass 1: walk every tree. INTERIORS are fully parsed (their child
    // references are the patch surface). OBJECT-tree leaves are NOT
    // cell-parsed yet — the live set comes from the freelist complement
    // below, and chain-head patches are only collected when overflow
    // pages actually exist (pass 2). Schema-tree leaves are always
    // cell-parsed: their cells embed the rootpage columns.
    collect_tree(src, 0, true, &mut seen, &mut refs, &mut schema_cells, true)?;
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
            &mut schema_cells,
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
        vac_plan_dbg!("btree page on freelist");
        return Ok(None);
    }
    let sorted: Vec<u32> = (0..old_n).filter(|id| !free.contains(id)).collect();
    // A live set smaller than the walked btree set is contradictory.
    if sorted.len() < seen.len() {
        vac_plan_dbg!("live set smaller than btree set");
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
    // Resolve schema rows into re-encoded cells. Every referenced root
    // must be live (reachable — it IS in the map); the new value is the
    // new INTERNAL id translated back to the SQLite 1-based convention
    // the column carries. Size-class changes are fine here — the cell
    // is fully re-encoded, not byte-patched.
    let mut schema_patches: Vec<SchemaCellPatch> = Vec::with_capacity(schema_cells.len());
    for cell in &schema_cells {
        if !cell.ok {
            vac_plan_dbg!(
                "schema row overflow/undecodable (page {} cell {})",
                cell.page,
                cell.cell_idx
            );
            return Ok(None);
        }
        let row = match crate::storage::row_codec::decode_row(&cell.payload, 5, 0, None) {
            Ok(r) => r,
            Err(_) => {
                vac_plan_dbg!(
                    "schema row record undecodable (page {} cell {})",
                    cell.page,
                    cell.cell_idx
                );
                return Ok(None);
            }
        };
        // Column 3 is the rootpage, SQLite 1-based convention. 0 (views,
        // triggers) means no tree — nothing to patch.
        let old_root = match row.get(3) {
            Some(crate::types::value::Value::Integer(r)) => *r,
            _ => {
                vac_plan_dbg!(
                    "schema row rootpage column not an integer (page {} cell {})",
                    cell.page,
                    cell.cell_idx
                );
                return Ok(None);
            }
        };
        if old_root <= 0 {
            continue; // views/triggers: no tree, no patch
        }
        let old_internal = (old_root - 1) as u32;
        let Some(&new_internal) = map.get(&old_internal) else {
            vac_plan_dbg!(
                "schema rootpage unreachable (raw {} -> internal {} not in the live map)",
                old_root,
                old_internal
            );
            return Ok(None); // root unreachable: not a shape we patch
        };
        let new_root = new_internal as i64 + 1; // back to SQLite convention
        if new_root == old_root {
            continue; // identity: the root did not move
        }
        let mut row = row;
        row[3] = crate::types::value::Value::Integer(new_root);
        let payload = crate::storage::row_codec::encode_row(&row);
        // The re-encoded cell must stay whole in the page (schema leaves
        // are rebuilt byte-level — an overflowing row would need a chain
        // the rebuild does not build). Decline to the image path.
        if crate::storage::btree::overflow_local_len_for(payload.len(), psz) < payload.len() {
            vac_plan_dbg!(
                "re-encoded schema row would overflow (page {} cell {})",
                cell.page,
                cell.cell_idx
            );
            return Ok(None);
        }
        schema_patches.push(SchemaCellPatch {
            leaf_old: cell.page,
            cell_idx: cell.cell_idx,
            payload,
        });
        if std::env::var_os("RSQL_VAC_TIMING").is_some() {
            eprintln!(
                "[vac] schema cell patch: page {} cell {} rootpage {} -> {}",
                cell.page, cell.cell_idx, old_root, new_root
            );
        }
    }
    if std::env::var_os("RSQL_VAC_TIMING").is_some() {
        eprintln!(
            "[vac] in-place plan: {} schema leaves to re-lay, {} cell patches",
            schema_rebuild_leaves.len(),
            schema_patches.len()
        );
    }
    Ok(Some(InPlaceCompaction {
        sorted,
        map,
        refs,
        schema_patches,
        schema_rebuild_leaves,
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
            PageType::LeafIndex => {
                // Index keys spill to overflow chains exactly like table
                // payloads ([varint rowid][varint TOTAL][local][4B
                // chain] — same spill math). Record the chain-head sites
                // so the install's patcher rewires them when pages move.
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
                        let Some((klen_raw, n_klen)) =
                            crate::storage::btree::varint::decode(&b.data[p..])
                        else {
                            continue;
                        };
                        let key_start = p + n_klen;
                        let (klen, local_len, _) =
                            crate::storage::btree::index_cell_split(klen_raw, psz);
                        let klen = klen as u64;
                        if local_len < klen as usize {
                            let chain_off = key_start + local_len;
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

/// Read-only tree walk collecting page ids (btree pages + leaf overflow
/// chains). Already-seen pages short-circuit (guards cycles). Page 0's
/// hybrid header is handled by the Page accessors' header_offset.
fn collect_tree(
    src: &Pager,
    page_id: u32,
    is_schema_tree: bool,
    seen: &mut PageIdSet,
    refs: &mut HashMap<u32, Vec<(u32, u32)>, PageIdHashBuild>,
    schema_cells: &mut Vec<SchemaCellRaw>,
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
                            // no in-page record bytes to re-encode —
                            // decline the in-place path for that shape.
                            schema_cells.push(SchemaCellRaw {
                                page: page_id,
                                cell_idx: i as u16,
                                payload: Vec::new(),
                                ok: false,
                            });
                        }
                        continue;
                    }
                    // Schema-tree leaf: collect the cell for plan-time
                    // re-encode (the row record is [type TEXT][name TEXT]
                    // [tbl_name (TEXT|NULL)][rootpage INT][sql TEXT] —
                    // each value [tag][body], see Value::encode_into).
                    if is_schema_tree {
                        let plen = plen as usize;
                        let end = payload_start + plen;
                        if end <= b.data.len() {
                            schema_cells.push(SchemaCellRaw {
                                page: page_id,
                                cell_idx: i as u16,
                                payload: b.data[payload_start..end].to_vec(),
                                ok: true,
                            });
                        } else {
                            // Record runs past the page: corrupt — decline.
                            schema_cells.push(SchemaCellRaw {
                                page: page_id,
                                cell_idx: i as u16,
                                payload: Vec::new(),
                                ok: false,
                            });
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
            // pointer at hdr + 8. Index interiors ALSO carry full-key
            // separator copies whose payloads can spill to overflow
            // chains — those chain-head sites are recorded too (the
            // patcher rewrites them alongside the child pointers), but
            // kept OUT of the child list: recursing collect_tree into a
            // chain page would mark it seen and silently strip the
            // next-link refs collect_overflow collects.
            let mut page_refs: Vec<(u32, u32)> = Vec::new();
            let mut sep_chain_refs: Vec<(u32, u32)> = Vec::new();
            let mut sep_chains: Vec<u32> = Vec::new();
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
                    if pt == PageType::InteriorIndex && cell_ptr + 4 < b.data.len() {
                        if let Some((_, n_rid)) =
                            crate::storage::btree::varint::decode_signed(&b.data[cell_ptr + 4..])
                        {
                            let p = cell_ptr + 4 + n_rid;
                            if p < b.data.len() {
                                if let Some((klen_raw, n_klen)) =
                                    crate::storage::btree::varint::decode(&b.data[p..])
                                {
                                    let (klen, local_len, _) =
                                        crate::storage::btree::index_cell_split(klen_raw, psz);
                                    let klen = klen as u64;
                                    if local_len < klen as usize {
                                        let chain_off = p + n_klen + local_len;
                                        if chain_off + 4 <= b.data.len() {
                                            let chain = u32::from_be_bytes(
                                                b.data[chain_off..chain_off + 4]
                                                    .try_into()
                                                    .unwrap(),
                                            );
                                            if chain != 0 {
                                                sep_chain_refs.push((chain_off as u32, chain));
                                                sep_chains.push(chain);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            drop(pr);
            if right != 0 {
                page_refs.push(((hdr + 8) as u32, right));
            }
            let children: Vec<u32> = page_refs.iter().map(|&(_, r)| r).collect();
            page_refs.extend(sep_chain_refs);
            refs.insert(page_id, page_refs);
            for chain in sep_chains {
                collect_overflow(src, chain, seen, refs)?;
            }
            for c in children {
                collect_tree(
                    src,
                    c,
                    is_schema_tree,
                    seen,
                    refs,
                    schema_cells,
                    parse_object_leaf_chains,
                )?;
            }
            Ok(())
        }
        PageType::Overflow => Ok(()), // reached only via collect_overflow
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
        if copy_tree(src, tmp, root, &mut map)?.is_none() {
            // The entire tree is empty (mass DELETE kept the pages
            // attached — this engine never recycles interior children).
            // Every object must keep a root: reserve a fresh EMPTY leaf
            // of the original root's family (table / index).
            let pt = src
                .get_page(root)
                .ok()
                .and_then(|p| p.lock().page_type().ok());
            let leaf_pt = if matches!(pt, Some(PageType::LeafIndex | PageType::InteriorIndex)) {
                PageType::LeafIndex
            } else {
                PageType::LeafTable
            };
            let psz = src.page_size() as usize;
            let mut bytes = vec![0u8; psz];
            bytes[0] = leaf_pt as u8;
            // n_cells = 0, cell_content_start = 0 (= page size), no
            // right pointer, no freeblocks — the canonical empty leaf.
            let new_id = write_new_page(tmp, &bytes)?;
            map.insert(root, new_id);
        }
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
        g.touch();
    }
    tmp.note_dirty(id);
    Ok(id)
}

/// Post-order tree copy (see `compact_page_copy`), with EMPTY-LEAF
/// PRUNING: this engine's DELETE never detaches an emptied leaf (the
/// page stays wired into its parent), so a mass delete leaves the tree
/// littered with 0-cell leaves that only VACUUM can reclaim. A leaf
/// with zero cells returns `None` — no page is allocated for it — and
/// the parent rebuild drops its cell. The interior descent already
/// tolerates every resulting state (a missing right-most pointer falls
/// back to the last cell's child; a 0-cell interior falls through to
/// its right child), which the delete/rebalance paths exercise in
/// production. An interior whose children ALL vanish collapses to its
/// single survivor (or returns `None` itself), so pruned trees also
/// lose their empty spines.
fn copy_tree(
    src: &Pager,
    tmp: &Pager,
    page_id: u32,
    map: &mut HashMap<u32, u32>,
) -> Result<Option<u32>> {
    if let Some(&n) = map.get(&page_id) {
        return Ok(Some(n));
    }
    let psz = src.page_size() as usize;
    let bytes = src_page_bytes(src, page_id)?;
    let pt = PageType::from_byte(bytes[0])
        .map_err(|_| Error::corruption(format!("vacuum copy: bad page type at {page_id}")))?;
    match pt {
        PageType::LeafTable => {
            let n = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
            if n == 0 {
                return Ok(None); // pruned: an emptied leaf reclaims its page
            }
            let mut new_bytes = bytes.clone();
            copy_leaf_overflow_chains(src, tmp, &mut new_bytes, 0, psz, map)?;
            let new_id = write_new_page(tmp, &new_bytes)?;
            map.insert(page_id, new_id);
            Ok(Some(new_id))
        }
        PageType::LeafIndex => {
            let n = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
            if n == 0 {
                return Ok(None); // pruned
            }
            // Index keys larger than max-local spill to overflow chains
            // (SQLite parity — tests/index_overflow.rs). The chains must
            // be copied and the 4-byte heads inside the cells patched,
            // exactly like table payloads.
            let mut new_bytes = bytes.clone();
            copy_index_leaf_overflow_chains(src, tmp, &mut new_bytes, 0, psz, map)?;
            let new_id = write_new_page(tmp, &new_bytes)?;
            map.insert(page_id, new_id);
            Ok(Some(new_id))
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
            // Recurse FIRST (post-order): children get their new ids (or
            // None — pruned subtrees).
            let mut new_children = Vec::with_capacity(children.len());
            for c in children {
                new_children.push(copy_tree(src, tmp, c, map)?);
            }
            // The right-most pointer: only an interior that HAD one
            // keeps one. right = 0 is a legal, documented state (the
            // delete/rebalance paths zero the slot when the right-most
            // subtree empties; the descent falls back to the last
            // cell's child as the de-facto right edge) — promoting that
            // cell's child into the right slot HERE would duplicate the
            // subtree (walked once as the cell's child, again as
            // right-most: phantom rows + out-of-order walks).
            let new_right = if right != 0 {
                new_children.last().copied().flatten()
            } else {
                None
            };
            let survivors: Vec<(usize, u32)> = new_children[..n.min(new_children.len())]
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.map(|id| (i, id)))
                .collect();
            if survivors.is_empty() {
                return Ok(match new_right {
                    // Everything pruned except (at most) the right-most
                    // subtree: collapse — the parent links straight to it.
                    Some(id) => {
                        map.insert(page_id, id);
                        Some(id)
                    }
                    None => None, // the whole subtree is empty
                });
            }
            // Rebuild the interior over the surviving cells: surviving
            // cell bytes verbatim (child pointer remapped), dropped
            // cells omitted from the pointer array, content re-flowed to
            // the tail, right-most pointer zeroed if its subtree died
            // (the descent's documented right=0 fallback covers it).
            let mut new_bytes = vec![0u8; psz];
            new_bytes[0] = pt as u8;
            new_bytes[8..12].copy_from_slice(&new_right.unwrap_or(0).to_be_bytes());
            let mut sizes: Vec<usize> = Vec::with_capacity(survivors.len());
            {
                // Scratch-copy the surviving cells (a tail-first rewrite
                // can overlap the sources).
                let mut scratch: Vec<u8> = Vec::new();
                for &(i, _) in &survivors {
                    let off = ptr_base + i * 2;
                    let cell_ptr =
                        u16::from_be_bytes(bytes[off..off + 2].try_into().unwrap()) as usize;
                    // Interior table cell: [4B child][varint key].
                    // Interior index cell: [4B child][varint rowid]
                    // [varint total][local][4B chain?]. Both sized via
                    // the paged helpers (overflow-aware for index).
                    let size = if pt == PageType::InteriorIndex {
                        interior_index_cell_size(&bytes[cell_ptr..], psz as u32).ok_or_else(
                            || Error::corruption("vacuum copy: truncated interior index cell"),
                        )?
                    } else {
                        interior_table_cell_size(&bytes[cell_ptr..])?
                    };
                    sizes.push(size);
                    scratch.extend_from_slice(&bytes[cell_ptr..cell_ptr + size]);
                }
                let mut src_off = scratch.len();
                let mut write_off = psz;
                // Tail-first, slot-REVERSED: scratch holds the survivors
                // in slot order, so the LAST chunk belongs to the LAST
                // slot — write it first (top of page), the FIRST slot
                // last (bottom, at the new content_start). Pointer
                // offsets end up descending with the slot index, the
                // engine's native rebuilt-page layout.
                for k in (0..survivors.len()).rev() {
                    let size = sizes[k];
                    src_off -= size;
                    write_off -= size;
                    new_bytes[write_off..write_off + size]
                        .copy_from_slice(&scratch[src_off..src_off + size]);
                    // Remap the child pointer (first 4 bytes).
                    let new_child = survivors[k].1;
                    new_bytes[write_off..write_off + 4].copy_from_slice(&new_child.to_be_bytes());
                    // Pointer-array slot k (big-endian u16 offset).
                    let dst = ptr_base + k * 2;
                    new_bytes[dst..dst + 2].copy_from_slice(&(write_off as u16).to_be_bytes());
                }
                debug_assert_eq!(src_off, 0);
                // cell_content_start: 0 encodes page_size (empty gap).
                let content = write_off as u32;
                let enc = if content >= psz as u32 {
                    0u16
                } else {
                    content as u16
                };
                new_bytes[6..8].copy_from_slice(&enc.to_be_bytes());
            }
            let n_new = survivors.len() as u16;
            new_bytes[4..6].copy_from_slice(&n_new.to_be_bytes());
            // Spilled index separator keys: copy their chains and patch
            // the in-cell heads (the rebuild moved the cells).
            if pt == PageType::InteriorIndex {
                copy_interior_index_separator_chains(src, tmp, &mut new_bytes, 0, psz, map)?;
            }
            let new_id = write_new_page(tmp, &new_bytes)?;
            map.insert(page_id, new_id);
            Ok(Some(new_id))
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

/// Walk an INDEX leaf's cells, copying overflow chains and patching the
/// 4-byte chain heads. Cell layout: [varint rowid][varint TOTAL key_len]
/// [local bytes][4B chain] — the chain exists exactly when the key
/// exceeds the local size (same spill math as table payloads).
fn copy_index_leaf_overflow_chains(
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
    if pt != PageType::LeafIndex {
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
        // [varint rowid][varint klen]
        let Some((_, n_rid)) =
            crate::storage::btree::varint::decode_signed(&page_bytes[cell_ptr..])
        else {
            continue;
        };
        let p = cell_ptr + n_rid;
        if p >= psz {
            continue;
        }
        let Some((klen_raw, n_klen)) = crate::storage::btree::varint::decode(&page_bytes[p..])
        else {
            continue;
        };
        let (klen, local_len, _) = crate::storage::btree::index_cell_split(klen_raw, psz);
        let klen = klen as u64;
        if local_len >= klen as usize {
            continue; // fully in-page key
        }
        let chain_off = p + n_klen + local_len;
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

/// Walk an index INTERIOR's cells, copying the overflow chains of spilled
/// SEPARATOR keys and patching their in-cell heads. Cell layout:
/// [be_u32 child][varint rowid][varint TOTAL][local][4B chain]. (The
/// separators are full copies of boundary keys, and a boundary key big
/// enough to spill in a leaf spills here too.)
fn copy_interior_index_separator_chains(
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
    if pt != PageType::InteriorIndex {
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
        // [be_u32 child][varint rowid][varint klen]
        let Some((_, n_rid)) =
            crate::storage::btree::varint::decode_signed(&page_bytes[cell_ptr + 4..])
        else {
            continue;
        };
        let p = cell_ptr + 4 + n_rid;
        if p >= psz {
            continue;
        }
        let Some((klen_raw, n_klen)) = crate::storage::btree::varint::decode(&page_bytes[p..])
        else {
            continue;
        };
        let (klen, local_len, _) = crate::storage::btree::index_cell_split(klen_raw, psz);
        let klen = klen as u64;
        if local_len >= klen as usize {
            continue; // fully in-page separator
        }
        let chain_off = p + n_klen + local_len;
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

/// Byte length of an interior TABLE cell `[be_u32 child][varint key]`.
fn interior_table_cell_size(buf: &[u8]) -> Result<usize> {
    if buf.len() < 4 {
        return Err(Error::corruption("vacuum copy: truncated interior cell"));
    }
    let (_, n) = crate::storage::btree::varint::decode_signed(&buf[4..])
        .ok_or_else(|| Error::corruption("vacuum copy: truncated interior key"))?;
    Ok(4 + n)
}

/// Byte length of an interior INDEX cell `[be_u32 child][varint rowid]
/// [varint TOTAL][local][4B chain when the key spills]`, overflow-aware.
fn interior_index_cell_size(buf: &[u8], page_size: u32) -> Option<usize> {
    let (_, n_rid) = crate::storage::btree::varint::decode_signed(&buf[4..])?;
    let p = 4 + n_rid;
    let (klen_raw, n_klen) = crate::storage::btree::varint::decode(&buf[p..])?;
    let (klen, local, _) = crate::storage::btree::index_cell_split(klen_raw, page_size as usize);
    let klen = klen as u64;
    if local >= klen as usize {
        Some(p + n_klen + klen as usize)
    } else {
        Some(p + n_klen + local + 4)
    }
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
