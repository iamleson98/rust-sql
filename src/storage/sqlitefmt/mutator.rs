//! PAGE-LEVEL SPLICING — the container-side b-tree mutator.
//!
//! The incremental page-diff architecture (see `container`) made the
//! SQLite-format container's commits O(changed OBJECT): per-root change
//! epochs pick the dirty b-trees, and each commit splices only those
//! onto stable page identities. The measured residual was object
//! GRANULARITY — a dirty object was re-collected (every row!) and its
//! whole logical tree re-encoded, even when a handful of pages changed
//! (the probe pinned it: 79 of 82 ms of a 10k-row table's commit
//! re-encodes all 3147 pages when 4–262 actually differ).
//!
//! This module closes that residual: it mutates the object's EXISTING
//! foreign-format pages in place — descend, verify the expected old
//! bytes, insert/delete the cell, re-pack only the TOUCHED pages
//! (densely — no freeblock bookkeeping), split with the engine's own
//! byte-aware 2-way discipline and its greedy-runs 3+-way fallback,
//! propagate separators up, and prune pages that empty out. Per-commit
//! CPU and I/O scale with the CHANGED PAGES, never with the object.
//!
//! Rootpage stability: a root split keeps the root's page IDENTITY
//! (the old content moves to fresh children, the root page becomes the
//! new interior — SQLite's own `balance()` discipline). DML commits
//! therefore never move rootpages, and the sqlite_schema tree stops
//! rebuilding on data-only commits.
//!
//! Correctness contract: every operation carries the bytes (or the
//! absence) it EXPECTS to find; any mismatch — a stale delta journal,
//! another session's intervening commit, anything — fails the WHOLE
//! mutation session for that object with [`MutateFallback`], and the
//! caller falls back to the whole-object splice (never corruption).
//! Mutations run into a scratch page map: until the session succeeds,
//! nothing is installed and no coordinator state moves.

use crate::types::Value;
use std::collections::{BTreeSet, HashMap};

use super::record::{decode_record_all_enc, encode_record_enc, TextEnc};
use super::varint::{read_varint, write_varint};
use super::writer::{compare_index_keys, make_index_cell, make_table_leaf_cell, Collation};

// ---------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------

/// One rowid-table row change, with the verification payload.
#[derive(Debug, Clone)]
pub struct TableRowOp {
    pub rowid: i64,
    /// The record bytes expected at `rowid` (`None` = the rowid must be
    /// absent). Byte-compared against the assembled cell payload — any
    /// mismatch fails the session (stale journal → splice fallback).
    pub expect: Option<Vec<u8>>,
    /// The record bytes to install (`None` = delete the row).
    pub set: Option<Vec<u8>>,
}

/// One entry change for an index-shaped tree (indexes and WITHOUT
/// ROWID tables). Entries are VALUES: ordering goes through the
/// tree's collation semantics, not raw bytes.
#[derive(Debug, Clone)]
pub struct EntryOp {
    /// The entry expected to exist (encoded and byte-compared; `None` =
    /// the key must be absent).
    pub expect: Option<Vec<Value>>,
    /// The entry to install (`None` = delete the key).
    pub set: Option<Vec<Value>>,
}

/// Ordering parameters for an entry tree — the same vectors the
/// whole-object builders take.
#[derive(Debug, Clone, Default)]
pub struct EntryOrder {
    pub desc: Vec<bool>,
    pub collations: Vec<Collation>,
}

/// The mutation program for one object.
#[derive(Debug, Clone)]
pub enum MutateOps {
    /// Rowid table b-tree (leaf 0x0d / interior 0x05).
    Table(Vec<TableRowOp>),
    /// Index b-tree or WITHOUT ROWID table (leaf 0x0a / interior 0x02).
    Entries(EntryOrder, Vec<EntryOp>),
}

/// A successful mutation session's result — the same shape the caller
/// consumes from the whole-object splice, plus the exact freelist
/// accounting the container needs.
pub struct MutateOutcome {
    pub root: u32,
    /// `(pgno, bytes)` of every page the session rewrote. The caller
    /// byte-compares against the committed view — identical rewrites
    /// (a no-op session) commit nothing, exactly like the splice path.
    pub pages: Vec<(u32, Vec<u8>)>,
    /// Pages that JOINED the object's span (fresh splits, moved-in
    /// overflow chains) — the span is maintained incrementally, so a
    /// commit's span bookkeeping is O(changes), never O(object pages).
    pub added: Vec<u32>,
    /// Pages that LEFT the span (pruned empties, dead chains,
    /// collapsed children).
    pub removed: Vec<u32>,
    /// Highest page number handed out (container tail growth).
    pub max_page: u32,
    /// Container-freelist pages the session consumed (still in use).
    pub took_from_free: Vec<u32>,
    /// Pages to add to the container freelist (pruned empties, dead
    /// overflow chains, collapsed children, discarded tail pages).
    pub freed: Vec<u32>,
}

/// The mutation cannot proceed — the caller falls back to the
/// whole-object splice for this object. `why` is diagnostics-only.
#[derive(Debug)]
pub struct MutateFallback(pub String);

impl From<String> for MutateFallback {
    fn from(why: String) -> Self {
        MutateFallback(why)
    }
}

impl From<&str> for MutateFallback {
    fn from(why: &str) -> Self {
        MutateFallback(why.to_string())
    }
}

type MResult<T> = Result<T, MutateFallback>;

// ---------------------------------------------------------------------------
// Mutator instrumentation (per-commit cost anatomy): counters at the
// fast-path decision points, the src reads and the model fallbacks.
// Read by the per-commit probes; always-on (one atomic add each).
// ---------------------------------------------------------------------------
pub static MUT_SRC_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_SRC_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_RAW_TABLE_OK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_RAW_TABLE_FALLBACK: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static MUT_RAW_INDEX_OK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_RAW_INDEX_FALLBACK: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static MUT_MODEL_LEAF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_SPLITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_TABLE_OP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_TABLE_LEAF_RAW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static MUT_TABLE_LEAF_MODEL_MAP: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static MUT_DESC_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

/// One in-flight mutation session over one object's tree. Pages load
/// lazily from `src` (the committed view: main file + WAL overlay),
/// decode into a cell model, mutate there, and re-encode densely at
/// finish. `src` must serve any page of the object's span and any
/// committed freelist page — but the session's own scratch pages are
/// consulted FIRST (cells inserted earlier this session verify fine).
pub struct Mutator<'a> {
    page_size: u32,
    usable: usize,
    enc: TextEnc,
    src: &'a mut dyn FnMut(u32) -> Option<Vec<u8>>,
    /// The container freelist (ascending) — fresh pages pop in order;
    /// only pages still in use at the end count as consumed.
    free: Vec<u32>,
    free_pos: usize,
    /// Pages popped from the container freelist (any fate).
    popped_from_free: BTreeSet<u32>,
    /// The pending-byte page (fileformat2) — never allocated.
    pending_page: u32,
    tail: u32,
    /// Pages freed mid-session (prunes, dead chains, collapses) —
    /// reusable by later allocations before anything is committed.
    session_free: BTreeSet<u32>,
    /// Decoded b-tree pages loaded (and possibly mutated) this session.
    pages: HashMap<u32, PageCells>,
    /// Session-written RAW pages (overflow chains — no cell model).
    raw_pages: HashMap<u32, Vec<u8>>,
    /// Committed RAW pages fetched for descents (memoized per session —
    /// every op probes the same interior pages).
    desc_raw: HashMap<u32, Vec<u8>>,
    /// Pages whose bytes the session rewrote.
    touched: BTreeSet<u32>,
    /// Every page number the allocator handed out (any source).
    allocated: BTreeSet<u32>,
    /// Pages removed from the object (pruned / chains / collapses).
    freed: BTreeSet<u32>,
    /// The object's original span (ascending) — BORROWED from the
    /// coordinator's span record; a session never mutates it.
    span0: &'a [u32],
    max_page: u32,
}

/// A decoded b-tree page: header facts + the ordered cell list.
struct PageCells {
    ptype: u8,
    /// Interior only: the right-most child pointer.
    right_most: u32,
    /// Cells in key order.
    cells: Vec<MutCell>,
}

#[derive(Clone)]
struct MutCell {
    /// The complete on-page cell bytes (head, including any 4-byte
    /// overflow pointer and, for interior cells, the leading child
    /// pointer).
    bytes: Vec<u8>,
    /// Table trees: leaf key / interior separator rowid. 0 for index
    /// cells (unused).
    rowid: i64,
    /// Interior only: the cell's left-child pointer.
    left_child: u32,
    /// Index trees: the cell's decoded entry values (filled lazily on
    /// first comparison; interior cells carry the separator entry,
    /// leaf cells the row's entry).
    idx: Option<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// Local/overflow split math — fileformat2 §1.6 ("the formulas")
// ---------------------------------------------------------------------------

/// Table-leaf maximum all-local payload.
#[inline]
fn table_x(u: usize) -> usize {
    u - 35
}

/// Index-leaf/interior maximum all-local payload.
#[inline]
fn index_x(u: usize) -> usize {
    ((u - 12) * 64 / 255) - 23
}

/// The overflow fallback minimum.
#[inline]
fn min_local(u: usize) -> usize {
    ((u - 12) * 32 / 255) - 23
}

/// The local payload size for a total of `total` bytes on a page of
/// the given kind (`index_page` selects the x formula).
#[inline]
fn local_size(u: usize, total: usize, index_page: bool) -> usize {
    let x = if index_page { index_x(u) } else { table_x(u) };
    if total <= x {
        return total;
    }
    let m = min_local(u);
    let k = m + ((total - m) % (u - 4));
    if k <= x {
        k
    } else {
        m
    }
}

// ---------------------------------------------------------------------------
// Cell head parsing (extent + key facts). Every parser takes the page's
// usable size for the local/overflow math.
// ---------------------------------------------------------------------------

/// Byte extent of the on-page part of one cell starting at `off`.
fn cell_extent(page: &[u8], off: usize, usable: usize) -> Result<usize, String> {
    let ptype = page[0];
    let u = usable;
    match ptype {
        0x0d => {
            // [varint P][varint rowid][local][u32?]
            let (total, l1) = read_varint(page, off).ok_or("bad payload varint")?;
            let (_, l2) = read_varint(page, off + l1).ok_or("bad rowid varint")?;
            let local = local_size(u, total as usize, false);
            let mut n = l1 + l2 + local;
            if total as usize > local {
                n += 4;
            }
            Ok(n)
        }
        0x05 => {
            // [u32 child][varint sep]
            let (_, l1) = read_varint(page, off + 4).ok_or("bad separator varint")?;
            Ok(4 + l1)
        }
        0x0a => {
            // [varint P][local record][u32?]
            let (total, l1) = read_varint(page, off).ok_or("bad payload varint")?;
            let local = local_size(u, total as usize, true);
            let mut n = l1 + local;
            if total as usize > local {
                n += 4;
            }
            Ok(n)
        }
        0x02 => {
            // [u32 child][varint P][local record][u32?]
            let (total, l1) = read_varint(page, off + 4).ok_or("bad payload varint")?;
            let local = local_size(u, total as usize, true);
            let mut n = 4 + l1 + local;
            if total as usize > local {
                n += 4;
            }
            Ok(n)
        }
        other => Err(format!("unexpected page type 0x{other:02x}")),
    }
}

/// The offset of a table-leaf cell's overflow pointer (inside the
/// cell's own bytes), or `None` when the payload is all-local.
fn table_overflow_at(cell: &[u8], usable: usize) -> Option<usize> {
    // Cell layout: [varint P][varint rowid][local][u32 overflow?]
    let (total, l1) = read_varint(cell, 0)?;
    let (_, l2) = read_varint(cell, l1)?;
    let local = local_size(usable, total as usize, false);
    if total as usize <= local {
        return None;
    }
    let at = l1 + l2 + local;
    cell.get(at..at + 4).is_some().then_some(at)
}

/// The offset of an index cell's overflow pointer (the cell may carry a
/// 4-byte child prefix first — `interior`), or `None` when all-local.
fn index_overflow_at(cell: &[u8], usable: usize, interior: bool) -> Option<usize> {
    let base = if interior { 4 } else { 0 };
    let (total, l1) = read_varint(cell, base)?;
    let local = local_size(usable, total as usize, true);
    if total as usize <= local {
        return None;
    }
    let at = base + l1 + local;
    cell.get(at..at + 4).is_some().then_some(at)
}

/// Decode one committed (or session-scratch) page into the cell model.
fn decode_page(pgno: u32, raw: &[u8], usable: usize) -> Result<PageCells, String> {
    if raw.len() < usable {
        return Err(format!("page {pgno} shorter than the page size"));
    }
    let ptype = raw[0];
    let interior = matches!(ptype, 0x05 | 0x02);
    let hdr: usize = if interior { 12 } else { 8 };
    let n = u16::from_be_bytes([raw[3], raw[4]]) as usize;
    let right_most = if interior {
        u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]])
    } else {
        0
    };
    let mut cells = Vec::with_capacity(n);
    for i in 0..n {
        let cp_off = hdr + i * 2;
        if cp_off + 2 > usable {
            return Err(format!("page {pgno}: cell pointer {i} out of page"));
        }
        let cp = u16::from_be_bytes([raw[cp_off], raw[cp_off + 1]]) as usize;
        if cp < hdr || cp >= usable {
            return Err(format!("page {pgno}: cell offset {cp} out of range"));
        }
        let extent = cell_extent(raw, cp, usable)?;
        if cp + extent > usable {
            return Err(format!("page {pgno}: cell {i} truncated"));
        }
        let bytes = raw[cp..cp + extent].to_vec();
        let (rowid, left_child) = match ptype {
            0x0d => {
                let (_, l1) = read_varint(&bytes, 0).ok_or("bad rowid varint")?;
                let (rowid, _) = read_varint(&bytes, l1).ok_or("bad rowid varint")?;
                (rowid, 0)
            }
            0x05 => {
                let child = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let (sep, _) = read_varint(&bytes, 4).ok_or("bad separator varint")?;
                (sep, child)
            }
            0x02 => {
                let child = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                (0, child)
            }
            _ => (0, 0), // 0x0a: leaf, no child prefix
        };
        cells.push(MutCell {
            bytes,
            rowid,
            left_child,
            idx: None,
        });
    }
    Ok(PageCells {
        ptype,
        right_most,
        cells,
    })
}

/// The child pointer of a RAW table-interior page (type 0x05) covering
/// `rowid` — a binary search over the cell pointer array, parsing only
/// the ~log2(cells) probed separators. This is the descent fast path:
/// interior pages never enter the cell model unless a split propagates
/// into them (the cell-model decode costs one allocation per cell,
/// which a descent never needs).
fn raw_table_child(raw: &[u8], rowid: i64) -> MResult<u32> {
    let n = u16::from_be_bytes([raw[3], raw[4]]) as usize;
    let right = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]);
    let cell_at = |i: usize| -> MResult<(u32, i64)> {
        let cp = u16::from_be_bytes([raw[12 + i * 2], raw[13 + i * 2]]) as usize;
        if cp + 5 > raw.len() {
            return Err("interior cell pointer out of page".into());
        }
        let child = u32::from_be_bytes([raw[cp], raw[cp + 1], raw[cp + 2], raw[cp + 3]]);
        let (sep, _) = read_varint(raw, cp + 4).ok_or("bad separator varint")?;
        Ok((child, sep))
    };
    // First cell with sep >= rowid (the original linear scan's rule).
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (_, sep) = cell_at(mid)?;
        if rowid <= sep {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    if lo < n {
        let (child, _) = cell_at(lo)?;
        Ok(child)
    } else {
        Ok(right)
    }
}

/// Dense re-encode of a page from the cell model — exactly the layout
/// the whole-object writers produce (content packed from the end, the
/// pointer array ascending, no freeblocks, no fragments).
fn encode_page(pc: &PageCells, page_size: u32) -> Vec<u8> {
    let ps = page_size as usize;
    let interior = matches!(pc.ptype, 0x05 | 0x02);
    let hdr: usize = if interior { 12 } else { 8 };
    let n = pc.cells.len();
    let mut offsets = Vec::with_capacity(n);
    let mut content = ps;
    for cell in &pc.cells {
        content -= cell.bytes.len();
        offsets.push(content);
    }
    let mut page = vec![0u8; ps];
    page[0] = pc.ptype;
    page[1..3].copy_from_slice(&0u16.to_be_bytes()); // no freeblocks
    page[3..5].copy_from_slice(&(n as u16).to_be_bytes());
    let cs = if content >= 65536 {
        0u16
    } else {
        content as u16
    };
    page[5..7].copy_from_slice(&cs.to_be_bytes());
    page[7] = 0; // no fragments
    if interior {
        page[8..12].copy_from_slice(&pc.right_most.to_be_bytes());
    }
    for (i, off) in offsets.iter().enumerate() {
        let cp = hdr + i * 2;
        page[cp..cp + 2].copy_from_slice(&(*off as u16).to_be_bytes());
    }
    for (i, cell) in pc.cells.iter().enumerate() {
        page[offsets[i]..offsets[i] + cell.bytes.len()].copy_from_slice(&cell.bytes);
    }
    page
}

/// Page budget: does the cell list fit a page of this kind?
#[inline]
fn fits(ptype: u8, cells: &[MutCell], usable: usize) -> bool {
    let hdr: usize = if matches!(ptype, 0x05 | 0x02) { 12 } else { 8 };
    let sum: usize = cells.iter().map(|c| c.bytes.len()).sum();
    sum + hdr + 2 * cells.len() <= usable
}

// ---------------------------------------------------------------------------
// RAW LEAF FAST PATHS
//
// The common autocommit commit touches ONE leaf of each tree with a
// cell that fits — but the cell-model path pays decode_page +
// encode_page, ONE ALLOCATION PER CELL, on that leaf (an index leaf
// packs ~300 entries at 4 KiB). These paths patch the leaf's RAW
// bytes instead: an insert grows the content area down (one small
// memmove for the pointer array + the new cell's bytes — SQLite's own
// insertCell discipline), a delete/replace compacts the surviving
// cells in one pass (one page-sized allocation, no per-cell allocs).
// Anything structurally richer — a split needed, an overflow chain on
// the NEW cell, a verify that needs chain assembly — returns None and
// the caller takes the cell-model path unchanged.
//
// A full leaf plus one new cell (the canonical first-touch shape after
// a dense build) takes the RAW 2-WAY SPLIT below before falling back:
// the same arithmetic `plan_leaf_runs` runs, over extents instead of
// decoded cells.
// ---------------------------------------------------------------------------

/// A/B kill switch: `RSQL_NO_RAW_FAST=1` disables the raw-leaf fast
/// paths (every op takes the cell-model path) for benchmarking.
fn raw_fast_disabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("RSQL_NO_RAW_FAST").is_some())
}

/// A/B kill switch for the RAW 2-way leaf SPLIT alone (the other raw
/// fast paths stay on): `RSQL_NO_RAW_SPLIT=1`.
fn raw_split_disabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("RSQL_NO_RAW_SPLIT").is_some())
}

/// A leaf page's raw geometry (parsed once; validated).
struct RawLeaf {
    /// Cell count (header bytes 3..5).
    n: usize,
    /// Cell content start (bytes 5..7; 0 means 65536).
    content: usize,
    /// The cell-pointer array's first byte (8 for both leaf kinds).
    array: usize,
}

/// Parse a leaf's geometry; `None` on anything malformed.
fn raw_leaf(raw: &[u8]) -> Option<RawLeaf> {
    if raw.len() < 8 {
        return None;
    }
    let ptype = raw[0];
    if ptype != 0x0d && ptype != 0x0a {
        return None;
    }
    let n = u16::from_be_bytes([raw[3], raw[4]]) as usize;
    let mut content = u16::from_be_bytes([raw[5], raw[6]]) as usize;
    if content == 0 {
        content = 65536;
    }
    let array = 8usize;
    if array + n * 2 > content || content > raw.len() {
        return None;
    }
    Some(RawLeaf { n, content, array })
}

/// Insert `cell` at pointer position `pos` on a CLEAN leaf (no
/// freeblocks, no fragments): existing cells never move — the content
/// area grows down by the cell's length. `None` when the leaf is
/// dirty (freeblock list present — the space math would be wrong) or
/// the cell does not fit (the caller takes the split path).
fn raw_leaf_grow_insert(raw: &[u8], leaf: &RawLeaf, pos: usize, cell: &[u8]) -> Option<Vec<u8>> {
    if u16::from_be_bytes([raw[1], raw[2]]) != 0 || raw[7] != 0 {
        return None; // freeblocks/fragments: not a clean leaf
    }
    let l = cell.len();
    let new_content = leaf.content.checked_sub(l)?;
    if new_content < leaf.array + (leaf.n + 1) * 2 {
        return None; // does not fit — split
    }
    let mut page = raw.to_vec();
    // Pointer array: shift [pos, n) up one slot, install the new entry.
    let (dst, src) = (leaf.array + (pos + 1) * 2, leaf.array + pos * 2);
    page.copy_within(src..src + (leaf.n - pos) * 2, dst);
    page[leaf.array + pos * 2..leaf.array + pos * 2 + 2]
        .copy_from_slice(&(new_content as u16).to_be_bytes());
    // Header: count +1, content start moved down.
    page[3..5].copy_from_slice(&((leaf.n + 1) as u16).to_be_bytes());
    let cs = if new_content >= 65536 {
        0u16
    } else {
        new_content as u16
    };
    page[5..7].copy_from_slice(&cs.to_be_bytes());
    page[new_content..new_content + l].copy_from_slice(cell);
    Some(page)
}

/// Compact rebuild of a leaf: `skip` one pointer position, insert
/// `cell` at `ins`, or both (a replace). Surviving cells are copied
/// in one pass into a fresh dense page — bottom-up packing (cell 0 at
/// the lowest content offset; a valid layout to every SQLite reader),
/// freeblocks and fragments dropped — with no per-cell allocation.
/// `None` when the result would not fit a page.
fn raw_leaf_rebuild(
    raw: &[u8],
    leaf: &RawLeaf,
    skip: Option<usize>,
    ins: Option<(usize, &[u8])>,
    usable: usize,
) -> Option<Vec<u8>> {
    if raw.len() < usable {
        return None;
    }
    let new_n = leaf.n + ins.is_some() as usize - skip.is_some() as usize;
    // Extent of every surviving cell (one varint parse each; no alloc).
    let extent_of = |i: usize| -> Option<usize> {
        let cp =
            u16::from_be_bytes([raw[leaf.array + i * 2], raw[leaf.array + i * 2 + 1]]) as usize;
        cell_extent(raw, cp, usable).ok()
    };
    let mut total = 0usize;
    for i in 0..leaf.n {
        if Some(i) == skip {
            continue;
        }
        total += extent_of(i)?;
    }
    let ins_len = ins.as_ref().map(|(_, c)| c.len()).unwrap_or(0);
    if leaf.array + new_n * 2 + total + ins_len > usable {
        return None; // does not fit — split
    }
    let mut page = vec![0u8; usable];
    page[0] = raw[0];
    page[3..5].copy_from_slice(&(new_n as u16).to_be_bytes());
    let content = usable - total - ins_len;
    let cs = if content >= 65536 {
        0u16
    } else {
        content as u16
    };
    page[5..7].copy_from_slice(&cs.to_be_bytes());
    // Pointer slots + content offsets in one ascending walk. The
    // insert fires when the walk REACHES its slot — checked BEFORE the
    // cell copy (an insert at slot 0, or at the slot a same-position
    // skip vacated, must land before the first copied cell; a
    // check-after-copy could never fire for slot 0 and would silently
    // drop the cell, leaving the top pointer zeroed).
    let ins_at = ins.map(|(at, _)| at);
    let mut off = content;
    let mut slot = 0usize;
    for i in 0..leaf.n {
        if Some(i) == skip {
            continue;
        }
        if Some(slot) == ins_at {
            let (_, cell) = ins.expect("ins_at implies ins");
            page[leaf.array + slot * 2..leaf.array + slot * 2 + 2]
                .copy_from_slice(&(off as u16).to_be_bytes());
            page[off..off + cell.len()].copy_from_slice(cell);
            off += cell.len();
            slot += 1;
        }
        let cp =
            u16::from_be_bytes([raw[leaf.array + i * 2], raw[leaf.array + i * 2 + 1]]) as usize;
        let extent = extent_of(i)?;
        page[leaf.array + slot * 2..leaf.array + slot * 2 + 2]
            .copy_from_slice(&(off as u16).to_be_bytes());
        page[off..off + extent].copy_from_slice(&raw[cp..cp + extent]);
        off += extent;
        slot += 1;
    }
    // Insert at the END (a trailing insert or a replace-at-tail).
    if Some(slot) == ins_at {
        let (_, cell) = ins.expect("ins_at implies ins");
        page[leaf.array + slot * 2..leaf.array + slot * 2 + 2]
            .copy_from_slice(&(off as u16).to_be_bytes());
        page[off..off + cell.len()].copy_from_slice(cell);
        off += cell.len();
        slot += 1;
    }
    debug_assert_eq!(slot, new_n, "rebuild slot accounting");
    debug_assert_eq!(off, usable, "rebuild content accounting");
    Some(page)
}

/// Build a dense page of the given type from `cells` (the same
/// bottom-up packing discipline as `raw_leaf_rebuild`; valid to every
/// SQLite reader). `right_most` lands in bytes 8..12 for interior
/// types. `None` when the cells do not fit.
fn dense_page(ptype: u8, cells: &[&[u8]], usable: usize, right_most: u32) -> Option<Vec<u8>> {
    let hdr: usize = if matches!(ptype, 0x05 | 0x02) { 12 } else { 8 };
    let total: usize = cells.iter().map(|c| c.len()).sum();
    if hdr + 2 * cells.len() + total > usable {
        return None;
    }
    let mut page = vec![0u8; usable];
    page[0] = ptype;
    page[3..5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let content = usable - total;
    let cs = if content >= 65536 {
        0u16
    } else {
        content as u16
    };
    page[5..7].copy_from_slice(&cs.to_be_bytes());
    if hdr == 12 {
        page[8..12].copy_from_slice(&right_most.to_be_bytes());
    }
    let mut off = content;
    for (i, c) in cells.iter().enumerate() {
        page[hdr + i * 2..hdr + i * 2 + 2].copy_from_slice(&(off as u16).to_be_bytes());
        page[off..off + c.len()].copy_from_slice(c);
        off += c.len();
    }
    Some(page)
}

/// The payload slice of an IN-PAGE table-leaf cell at pointer
/// position `pos` (`None` when the cell overflows — chain assembly
/// is the model path's job).
fn inpage_table_payload<'r>(
    raw: &'r [u8],
    leaf: &RawLeaf,
    pos: usize,
    usable: usize,
) -> Option<&'r [u8]> {
    let cp =
        u16::from_be_bytes([raw[leaf.array + pos * 2], raw[leaf.array + pos * 2 + 1]]) as usize;
    let (total, l1) = read_varint(raw, cp)?;
    let (_, l2) = read_varint(raw, cp + l1)?;
    let at = cp + l1 + l2;
    let total = total as usize;
    (total <= table_x(usable)).then(|| &raw[at..at + total])
}

// ---------------------------------------------------------------------------
// Mutator core
// ---------------------------------------------------------------------------

impl<'a> Mutator<'a> {
    /// Start a session over the object whose span is `span0` with root
    /// `root`. `src` serves the committed view of ANY page (the
    /// container's reader + overlay); `free` is the container freelist
    /// (ascending); `tail` is the first fresh page number.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        page_size: u32,
        enc: TextEnc,
        src: &'a mut dyn FnMut(u32) -> Option<Vec<u8>>,
        free: Vec<u32>,
        tail: u32,
        span0: &'a [u32],
    ) -> Self {
        let pending_page = 0x4000_0000 / page_size.max(1) + 1;
        Mutator {
            page_size,
            usable: page_size as usize,
            enc,
            src,
            free,
            free_pos: 0,
            popped_from_free: BTreeSet::new(),
            pending_page,
            tail,
            session_free: BTreeSet::new(),
            pages: HashMap::new(),
            raw_pages: HashMap::new(),
            desc_raw: HashMap::new(),
            touched: BTreeSet::new(),
            allocated: BTreeSet::new(),
            freed: BTreeSet::new(),
            span0,
            max_page: 0,
        }
    }

    /// Fetch raw page bytes: session scratch (overflow chains) first,
    /// then the descent cache, then the committed view. The descent
    /// cache memoizes committed pages fetched for raw binary-search
    /// descents — the same interior pages are probed by every op of
    /// the session, and re-cloning them per op is the mutator's single
    /// biggest fixed cost.
    fn raw(&mut self, pgno: u32) -> MResult<Vec<u8>> {
        use std::sync::atomic::Ordering::Relaxed;
        if let Some(bytes) = self.raw_pages.get(&pgno) {
            MUT_DESC_READS.fetch_add(1, Relaxed);
            return Ok(bytes.clone());
        }
        if let Some(bytes) = self.desc_raw.get(&pgno) {
            MUT_DESC_READS.fetch_add(1, Relaxed);
            return Ok(bytes.clone());
        }
        if let Some(bytes) = (self.src)(pgno) {
            MUT_SRC_CALLS.fetch_add(1, Relaxed);
            MUT_SRC_BYTES.fetch_add(bytes.len() as u64, Relaxed);
            self.desc_raw.insert(pgno, bytes.clone());
            return Ok(bytes);
        }
        Err(format!("page {pgno} unreadable").into())
    }

    // ---- Allocation ----

    /// One fresh page number: mid-session frees first, then the
    /// container freelist (ascending), then the tail (skipping the
    /// pending-byte page).
    fn alloc(&mut self) -> u32 {
        if let Some(&p) = self.session_free.iter().next() {
            self.session_free.remove(&p);
            self.allocated.insert(p);
            self.max_page = self.max_page.max(p);
            return p;
        }
        if self.free_pos < self.free.len() {
            let p = self.free[self.free_pos];
            self.free_pos += 1;
            self.popped_from_free.insert(p);
            self.allocated.insert(p);
            self.max_page = self.max_page.max(p);
            return p;
        }
        let mut n = self.tail;
        while n == self.pending_page {
            n += 1;
        }
        self.tail = n + 1;
        self.allocated.insert(n);
        self.max_page = self.max_page.max(n);
        n
    }

    /// Return a page to the session's free pool (reusable before the
    /// session ends; the remainder goes back to the container).
    fn release(&mut self, pgno: u32) {
        self.pages.remove(&pgno);
        self.raw_pages.remove(&pgno);
        self.desc_raw.remove(&pgno);
        self.touched.remove(&pgno);
        self.freed.insert(pgno);
        self.session_free.insert(pgno);
    }

    // ---- Overflow chains ----

    /// Read a cell's full payload (local bytes + the overflow chain).
    /// `local_at`/`local_len` locate the local span inside `cell`;
    /// `ovf_at` (when `Some`) locates the 4-byte chain head pointer.
    fn assemble(
        &mut self,
        cell: &[u8],
        local_at: usize,
        local_len: usize,
        ovf_at: Option<usize>,
    ) -> MResult<Vec<u8>> {
        let mut payload = cell[local_at..local_at + local_len].to_vec();
        let Some(at) = ovf_at else {
            return Ok(payload);
        };
        let mut next = u32::from_be_bytes([cell[at], cell[at + 1], cell[at + 2], cell[at + 3]]);
        let mut guard = 0u32;
        while next != 0 {
            guard += 1;
            if guard > 1_000_000 {
                return Err("overflow chain too long (loop?)".into());
            }
            let page = self.raw(next)?;
            if page.len() < 4 {
                return Err(format!("overflow page {next} too small").into());
            }
            let following = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            payload.extend_from_slice(&page[4..]);
            next = following;
        }
        Ok(payload)
    }

    /// A table-leaf cell's full payload.
    fn table_payload(&mut self, cell: &[u8]) -> MResult<Vec<u8>> {
        // Cell layout: [varint P][varint rowid][local][u32 overflow?] —
        // the FIRST varint is the payload total, the second the rowid.
        let (total, l1) =
            read_varint(cell, 0).ok_or_else(|| MutateFallback("bad cell varint".into()))?;
        let (_, l2) =
            read_varint(cell, l1).ok_or_else(|| MutateFallback("bad cell varint".into()))?;
        let local = local_size(self.usable, total as usize, false);
        let ovf = table_overflow_at(cell, self.usable);
        self.assemble(cell, l1 + l2, local, ovf)
    }

    /// An index cell's full payload (record bytes).
    fn index_payload(&mut self, cell: &[u8], interior: bool) -> MResult<Vec<u8>> {
        let base = if interior { 4 } else { 0 };
        let (total, l1) =
            read_varint(cell, base).ok_or_else(|| MutateFallback("bad cell varint".into()))?;
        let local = local_size(self.usable, total as usize, true);
        let ovf = index_overflow_at(cell, self.usable, interior);
        self.assemble(cell, base + l1, local, ovf)
    }

    /// Free every page of a cell's overflow chain (the cell is being
    /// removed or replaced).
    fn free_chain_of(&mut self, cell: &[u8], table_leaf: bool, interior: bool) {
        let ovf = if table_leaf {
            table_overflow_at(cell, self.usable)
        } else {
            index_overflow_at(cell, self.usable, interior)
        };
        let Some(at) = ovf else { return };
        if at + 4 > cell.len() {
            return;
        }
        let mut next = u32::from_be_bytes([cell[at], cell[at + 1], cell[at + 2], cell[at + 3]]);
        let mut guard = 0u32;
        while next != 0 {
            guard += 1;
            if guard > 1_000_000 {
                return;
            }
            let Ok(page) = self.raw(next) else {
                return;
            };
            if page.len() < 4 {
                return;
            }
            let following = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            self.release(next);
            next = following;
        }
    }

    /// Write a new cell's overflow chain (if it has one), patching the
    /// head's pointer. Returns the final on-page cell bytes.
    fn attach_chain(&mut self, mut head: Vec<u8>, tail: &[u8], ovf_at: usize) -> Vec<u8> {
        if tail.is_empty() {
            return head;
        }
        let cap = self.usable - 4;
        let n_chain = tail.len().div_ceil(cap);
        let mut chain: Vec<u32> = Vec::with_capacity(n_chain);
        for _ in 0..n_chain {
            chain.push(self.alloc());
        }
        head[ovf_at..ovf_at + 4].copy_from_slice(&chain[0].to_be_bytes());
        for (i, &pgno) in chain.iter().enumerate() {
            let start = i * cap;
            let end = (start + cap).min(tail.len());
            let next = chain.get(i + 1).copied().unwrap_or(0);
            let mut page = vec![0u8; self.usable];
            page[0..4].copy_from_slice(&next.to_be_bytes());
            page[4..4 + (end - start)].copy_from_slice(&tail[start..end]);
            self.raw_pages.insert(pgno, page);
            self.touched.insert(pgno);
        }
        head
    }
}

// ---------------------------------------------------------------------------
// Page ownership helpers
// ---------------------------------------------------------------------------

impl<'a> Mutator<'a> {
    /// Load a page into the cache WITHOUT marking it rewritten.
    fn cache_page(&mut self, pgno: u32) -> MResult<()> {
        if self.pages.contains_key(&pgno) {
            return Ok(());
        }
        // Through `raw`: the descent cache may already hold the page.
        let raw = self.raw(pgno)?;
        let pc = decode_page(pgno, &raw, self.usable).map_err(MutateFallback)?;
        self.pages.insert(pgno, pc);
        Ok(())
    }

    /// Take a page out of the cache for mutation (loading it first).
    fn take_page(&mut self, pgno: u32) -> MResult<PageCells> {
        self.cache_page(pgno)?;
        Ok(self.pages.remove(&pgno).unwrap())
    }

    /// Put a mutated page back, marking it rewritten.
    fn put_page(&mut self, pgno: u32, pc: PageCells) {
        self.pages.insert(pgno, pc);
        self.touched.insert(pgno);
    }

    /// Put a page back WITHOUT marking it rewritten (read-only loads).
    fn stash_page(&mut self, pgno: u32, pc: PageCells) {
        self.pages.insert(pgno, pc);
    }

    // ---- Cell construction ----

    /// A table-leaf cell for `(rowid, payload)` — the writer's exact
    /// head math, with the overflow chain written and attached.
    fn make_table_cell(&mut self, rowid: i64, payload: &[u8]) -> MutCell {
        let c = make_table_leaf_cell(self.usable, rowid, payload);
        let head = match c.ptr_pos {
            Some(at) if !c.tail.is_empty() => self.attach_chain(c.head, &c.tail, at),
            _ => c.head,
        };
        MutCell {
            bytes: head,
            rowid,
            left_child: 0,
            idx: None,
        }
    }

    /// An index-leaf cell for one entry.
    fn make_entry_cell(&mut self, values: &[Value]) -> MutCell {
        let c = make_index_cell(self.usable, values, self.enc);
        let head = match c.ptr_pos {
            Some(at) if !c.tail.is_empty() => self.attach_chain(c.head, &c.tail, at),
            _ => c.head,
        };
        MutCell {
            bytes: head,
            rowid: 0,
            left_child: 0,
            idx: Some(values.to_vec()),
        }
    }

    /// An interior pair `(left_child, separator-rowid)` (table trees).
    fn table_pair(&self, child: u32, sep: i64) -> MutCell {
        let mut bytes = Vec::with_capacity(4 + 10);
        bytes.extend_from_slice(&child.to_be_bytes());
        write_varint(&mut bytes, sep);
        MutCell {
            bytes,
            rowid: sep,
            left_child: child,
            idx: None,
        }
    }

    /// An interior pair `(left_child, separator-entry)` (index trees) —
    /// the entry's overflow chain (if any) moves with the bytes.
    fn entry_pair(&self, child: u32, entry: &MutCell) -> MutCell {
        let mut bytes = Vec::with_capacity(4 + entry.bytes.len());
        bytes.extend_from_slice(&child.to_be_bytes());
        bytes.extend_from_slice(&entry.bytes);
        MutCell {
            bytes,
            rowid: 0,
            left_child: child,
            idx: entry.idx.clone(),
        }
    }

    /// Decode an index cell's entry values (lazily, memoized).
    fn entry_values(&mut self, cell: &mut MutCell, interior: bool) -> MResult<Vec<Value>> {
        if let Some(v) = &cell.idx {
            return Ok(v.clone());
        }
        let payload = self.index_payload(&cell.bytes, interior)?;
        let values = decode_record_all_enc(&payload, self.enc).map_err(MutateFallback)?;
        cell.idx = Some(values.clone());
        Ok(values)
    }
}

// ---------------------------------------------------------------------------
// Table-tree operations
// ---------------------------------------------------------------------------

impl<'a> Mutator<'a> {
    /// One rowid-table op: descend, verify, apply, rebalance, propagate.
    fn table_op(&mut self, root: u32, op: TableRowOp) -> MResult<()> {
        if op.expect.is_none() && op.set.is_none() {
            return Ok(());
        }
        MUT_TABLE_OP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // ---- Descent ----
        // Interior pages take the RAW binary-search path (no cell-model
        // decode — one allocation per cell is a cost a descent never
        // needs to pay); a page the session already decoded (an earlier
        // op's split rewrote it) stays authoritative in the model map.
        // Only the touched LEAF enters the cell model.
        let mut path: Vec<u32> = Vec::new(); // root..parent (page ids)
        let mut pgno = root;
        let (mut leaf, leaf_ptype) = loop {
            if let Some(pc) = self.pages.get(&pgno) {
                let ptype = pc.ptype;
                match ptype {
                    0x0d => {
                        MUT_TABLE_LEAF_MODEL_MAP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        break (self.pages.remove(&pgno).expect("just checked"), 0x0du8);
                    }
                    0x05 => {
                        let mut child = pc.right_most;
                        for c in &pc.cells {
                            if op.rowid <= c.rowid {
                                child = c.left_child;
                                break;
                            }
                        }
                        path.push(pgno);
                        pgno = child;
                        continue;
                    }
                    other => {
                        return Err(
                            format!("table descent: page {pgno} has type 0x{other:02x}").into()
                        );
                    }
                }
            }
            let raw = self.raw(pgno)?;
            match raw[0] {
                0x0d => {
                    // RAW fast path first: the common one-cell patch
                    // never enters the cell model. `None` = structurally
                    // richer — the model path below handles it unchanged.
                    MUT_TABLE_LEAF_RAW.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if !raw_fast_disabled() {
                        if let Some(r) =
                            self.try_raw_table_leaf(pgno, &raw, &op, path.last().copied())
                        {
                            MUT_RAW_TABLE_OK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return r;
                        }
                        MUT_RAW_TABLE_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    MUT_MODEL_LEAF.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let pc = decode_page(pgno, &raw, self.usable).map_err(MutateFallback)?;
                    break (pc, 0x0du8);
                }
                0x05 => {
                    let child = raw_table_child(&raw, op.rowid)?;
                    path.push(pgno);
                    pgno = child;
                }
                other => {
                    return Err(format!("table descent: page {pgno} has type 0x{other:02x}").into());
                }
            }
        };

        // ---- Verify + apply on the leaf ----
        let pos = leaf.cells.partition_point(|c| c.rowid < op.rowid);
        let present = leaf.cells.get(pos).is_some_and(|c| c.rowid == op.rowid);
        match (&op.expect, &op.set) {
            (None, None) => {}
            (None, Some(rec)) => {
                if present {
                    return Err(format!(
                        "insert: rowid {} already present (stale journal?)",
                        op.rowid
                    )
                    .into());
                }
                let cell = self.make_table_cell(op.rowid, rec);
                leaf.cells.insert(pos, cell);
            }
            (Some(old), new) => {
                if !present {
                    return Err(format!(
                        "update/delete: rowid {} absent (stale journal?)",
                        op.rowid
                    )
                    .into());
                }
                let found = self.table_payload(&leaf.cells[pos].bytes)?;
                if found != *old {
                    return Err(format!(
                        "update/delete: rowid {} record mismatch (stale journal?)",
                        op.rowid
                    )
                    .into());
                }
                let old_bytes = leaf.cells.remove(pos);
                self.free_chain_of(&old_bytes.bytes, true, false);
                if let Some(rec) = new {
                    let cell = self.make_table_cell(op.rowid, rec);
                    leaf.cells.insert(pos, cell);
                }
            }
        }

        // ---- Rebalance + propagate ----
        if leaf.cells.is_empty() && !path.is_empty() {
            // Prune the empty leaf; the parent loses its reference.
            self.release(pgno);
            self.remove_table_child(&mut path, pgno)?;
            return Ok(());
        }
        if !fits(leaf_ptype, &leaf.cells, self.usable) {
            self.split_node(&mut path, pgno, leaf, leaf_ptype)?;
            return Ok(());
        }
        self.put_page(pgno, leaf);
        Ok(())
    }

    /// Remove `child` from its table parent (the path's top), pruning
    /// upward while pages empty. Table separators are copies — no
    /// entry preservation needed.
    fn remove_table_child(&mut self, path: &mut Vec<u32>, child: u32) -> MResult<()> {
        let Some(parent_pgno) = path.pop() else {
            return Ok(());
        };
        let mut parent = self.take_page(parent_pgno)?;
        if parent.right_most == child {
            match parent.cells.pop() {
                Some(last) => parent.right_most = last.left_child,
                None => {
                    // The parent's ONLY child vanished.
                    if path.is_empty() {
                        // Root: an empty tree — the root page becomes an
                        // empty leaf (identity kept).
                        parent = PageCells {
                            ptype: 0x0d,
                            right_most: 0,
                            cells: Vec::new(),
                        };
                        self.put_page(parent_pgno, parent);
                        return Ok(());
                    }
                    self.release(parent_pgno);
                    return self.remove_table_child(path, parent_pgno);
                }
            }
        } else {
            let Some(idx) = parent.cells.iter().position(|c| c.left_child == child) else {
                return Err("prune: child pointer not found in parent".into());
            };
            parent.cells.remove(idx);
        }
        if parent.cells.is_empty() {
            if path.is_empty() {
                // Root interior with a single (right-most) child:
                // collapse — the child's content moves INTO the root
                // page, identity kept.
                let sole = parent.right_most;
                if sole != 0 {
                    let content = self.take_page(sole)?;
                    self.release(sole);
                    self.put_page(parent_pgno, content);
                }
                return Ok(());
            }
            self.release(parent_pgno);
            return self.remove_table_child(path, parent_pgno);
        }
        self.put_page(parent_pgno, parent);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Splits (both tree kinds)
// ---------------------------------------------------------------------------

/// One planned run of a split: the cells it keeps and (for index trees)
/// the boundary entry pushed to the parent.
struct SplitRun {
    /// Cells the run's page KEEPS.
    keep: Vec<MutCell>,
    /// Table trees: the copied-up separator (max rowid of the run).
    sep: i64,
    /// Index trees: the boundary ENTRY pushed up (the run's last cell,
    /// removed from `keep`).
    pushed: Option<MutCell>,
}

/// Plan a leaf split's runs. `index_tree` selects the push-last
/// discipline (each non-final run keeps >= 1 cell after its push); the
/// greedy fallback mirrors the engine's native multi-way split for
/// pages packed past every feasible 2-way point.
fn plan_leaf_runs(cells: Vec<MutCell>, index_tree: bool, usable: usize) -> Vec<SplitRun> {
    let hdr = 8usize;
    let n = cells.len();
    let run_bytes = |cs: &[MutCell]| cs.iter().map(|c| c.bytes.len()).sum::<usize>();
    let fits_run = |cs: &[MutCell]| run_bytes(cs) + hdr + 2 * cs.len() <= usable;

    if index_tree {
        // Balanced 2-way: boundary cell b (pushed up), left keeps
        // 0..b-1, right keeps b+1.. — both sides non-empty.
        let mut best: Option<(usize, usize)> = None; // (b, imbalance)
        for b in 1..n.saturating_sub(1) {
            let left = &cells[..b];
            let right = &cells[b + 1..];
            if fits_run(left) && fits_run(right) {
                let imb =
                    ((run_bytes(left) as i64 - run_bytes(right) as i64).unsigned_abs()) as usize;
                if !best.is_some_and(|(_, cur)| imb >= cur) {
                    best = Some((b, imb));
                }
            }
        }
        if let Some((b, _)) = best {
            return vec![
                SplitRun {
                    keep: cells[..b].to_vec(),
                    sep: 0,
                    pushed: Some(cells[b].clone()),
                },
                SplitRun {
                    keep: cells[b + 1..].to_vec(),
                    sep: 0,
                    pushed: None,
                },
            ];
        }
        // Greedy multi-way: every non-final run keeps >= 1 cell after
        // pushing its boundary, so non-final runs hold >= 2 (two index
        // cells always fit: max on-page cell ~ usable/4). The final
        // run may hold 1.
        let mut runs: Vec<Vec<MutCell>> = Vec::new();
        let mut cur: Vec<MutCell> = Vec::new();
        for cell in cells {
            if !cur.is_empty() && !fits_run(&cur) {
                runs.push(std::mem::take(&mut cur));
            }
            cur.push(cell);
        }
        if !cur.is_empty() {
            runs.push(cur);
        }
        let total = runs.len();
        let mut out: Vec<SplitRun> = Vec::new();
        for (i, mut run) in runs.into_iter().enumerate() {
            if i + 1 == total {
                out.push(SplitRun {
                    keep: run,
                    sep: 0,
                    pushed: None,
                });
            } else {
                let pushed = run.pop().expect("non-final index run holds >= 2 cells");
                out.push(SplitRun {
                    keep: run,
                    sep: 0,
                    pushed: Some(pushed),
                });
            }
        }
        return out;
    }

    // Table trees: separators are copied rowids.
    let mut best: Option<(usize, usize)> = None;
    for k in 1..n {
        let left = &cells[..k];
        let right = &cells[k..];
        if fits_run(left) && fits_run(right) {
            let imb = ((run_bytes(left) as i64 - run_bytes(right) as i64).unsigned_abs()) as usize;
            if !best.is_some_and(|(_, cur)| imb >= cur) {
                best = Some((k, imb));
            }
        }
    }
    if let Some((k, _)) = best {
        let left = cells[..k].to_vec();
        let right = cells[k..].to_vec();
        let sep = left.last().map(|c| c.rowid).unwrap_or(0);
        return vec![
            SplitRun {
                keep: left,
                sep,
                pushed: None,
            },
            SplitRun {
                keep: right,
                sep: 0,
                pushed: None,
            },
        ];
    }
    // Greedy multi-way (a page packed past every feasible 2-way point —
    // giant cells; every legal cell fits alone).
    let mut runs: Vec<Vec<MutCell>> = Vec::new();
    let mut cur: Vec<MutCell> = Vec::new();
    for cell in cells {
        let sz = cell.bytes.len();
        let cur_bytes: usize = cur.iter().map(|c| c.bytes.len()).sum();
        if !cur.is_empty() && cur_bytes + sz + hdr + 2 * (cur.len() + 1) > usable {
            runs.push(std::mem::take(&mut cur));
        }
        cur.push(cell);
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs.into_iter()
        .map(|run| {
            let sep = run.last().map(|c| c.rowid).unwrap_or(0);
            SplitRun {
                keep: run,
                sep,
                pushed: None,
            }
        })
        .collect()
}

/// One planned group of an INTERIOR split: the pairs its page keeps,
/// its right-most child, and the boundary pushed to the parent.
struct InteriorRun {
    pairs: Vec<MutCell>,
    right_most: u32,
    /// Table trees: the copied-up separator rowid.
    sep: i64,
    /// Index trees: the pushed-up boundary ENTRY.
    pushed: Option<MutCell>,
}

/// Plan an interior split's groups over `pairs` + `right_most`
/// (see the packer's group semantics: a group covering children
/// a..=b keeps pairs a..b-1 with child b as its right-most; the
/// boundary pair b is consumed — its child closes group 1, its
/// separator pushes up). Every group keeps >= 1 pair.
fn plan_interior_runs(
    pairs: Vec<MutCell>,
    right_most: u32,
    index_tree: bool,
    usable: usize,
) -> Result<Vec<InteriorRun>, String> {
    let hdr = 12usize;
    let m = pairs.len();
    let sum = |ps: &[MutCell]| ps.iter().map(|c| c.bytes.len()).sum::<usize>();
    let fits_p = |ps: &[MutCell]| sum(ps) + hdr + 2 * ps.len() <= usable;

    let make_runs = |bounds: &[usize]| -> Vec<InteriorRun> {
        // bounds = child-indices closing each group except the last.
        let mut out = Vec::with_capacity(bounds.len() + 1);
        let mut start = 0usize; // first child index of the current group
        for &b in bounds {
            // Group [start..=b]: pairs start..b-1, right = pairs[b]'s child.
            let group_pairs = pairs[start..b].to_vec();
            let (sep, pushed) = if index_tree {
                // Pair b consumed: its ENTRY (child prefix stripped) pushes.
                let mut entry = pairs[b].clone();
                entry.bytes = entry.bytes[4..].to_vec();
                entry.left_child = 0;
                (0i64, Some(entry))
            } else {
                (pairs[b].rowid, None)
            };
            out.push(InteriorRun {
                pairs: group_pairs,
                right_most: pairs[b].left_child,
                sep,
                pushed,
            });
            start = b + 1;
        }
        // Final group [start..=m]: pairs start..m-1 + right = right_most.
        out.push(InteriorRun {
            pairs: pairs[start..].to_vec(),
            right_most,
            sep: 0,
            pushed: None,
        });
        out
    };

    // Balanced 2-way: boundary b in [1, m-2].
    let mut best: Option<(usize, usize)> = None;
    for b in 1..m.saturating_sub(1) {
        let (left_pairs, right_pairs) = if index_tree {
            (&pairs[..b], &pairs[b + 1..])
        } else {
            (&pairs[..b], &pairs[b..])
        };
        if !right_pairs.is_empty() && fits_p(left_pairs) && fits_p(right_pairs) {
            let imb = ((sum(left_pairs) as i64 - sum(right_pairs) as i64).unsigned_abs()) as usize;
            if !best.is_some_and(|(_, cur)| imb >= cur) {
                best = Some((b, imb));
            }
        }
    }
    if let Some((b, _)) = best {
        return Ok(make_runs(&[b]));
    }

    // Greedy multi-way: pack groups while they fit, boundaries between.
    let mut bounds: Vec<usize> = Vec::new();
    let mut cur_bytes = 0usize;
    let mut cur_n = 0usize;
    for (i, pair) in pairs.iter().enumerate().take(m) {
        let sz = pair.bytes.len();
        if cur_n > 0 && cur_bytes + sz + hdr + 2 * (cur_n + 1) > usable {
            // close the group at child i (pair i is the boundary)
            bounds.push(i);
            cur_bytes = 0;
            cur_n = 0;
        }
        cur_bytes += sz;
        cur_n += 1;
    }
    let runs = make_runs(&bounds);
    // Feasibility: every group keeps >= 1 pair and fits.
    for r in &runs {
        if r.pairs.is_empty() || !fits_p(&r.pairs) {
            return Err(format!(
                "interior split infeasible ({} pairs, group with {} cells)",
                m,
                r.pairs.len()
            ));
        }
    }
    Ok(runs)
}

impl<'a> Mutator<'a> {
    /// Split an over-full page and propagate: run 0 keeps `pgno`'s
    /// identity (non-root), every further run takes a fresh page, and
    /// the parent's reference to `pgno` is REPLACED by the run
    /// sequence — the LAST run re-pairs with the old separator (or
    /// becomes the new right-most). The root keeps its page identity
    /// by having its CONTENT distributed to fresh children.
    #[allow(clippy::too_many_arguments)]
    fn split_node(
        &mut self,
        path: &mut Vec<u32>,
        pgno: u32,
        node: PageCells,
        ptype: u8,
    ) -> MResult<()> {
        MUT_SPLITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let index_tree = matches!(ptype, 0x0a | 0x02);
        let is_root = path.is_empty();

        // ---- Plan ----
        enum Plan {
            Leaf(Vec<SplitRun>),
            Interior(Vec<InteriorRun>),
        }
        let plan = if matches!(ptype, 0x0d | 0x0a) {
            Plan::Leaf(plan_leaf_runs(node.cells, index_tree, self.usable))
        } else {
            Plan::Interior(
                plan_interior_runs(node.cells, node.right_most, index_tree, self.usable)
                    .map_err(MutateFallback)?,
            )
        };
        if let Plan::Leaf(runs) = &plan {
            if runs.len() < 2 {
                return Err("split produced a single run (page fits?)".into());
            }
        }

        // ---- Materialize run pages ----
        // (run page, its parent-boundary cell) per run; the last run's
        // boundary is filled by the caller context (old sep / right-most).
        let mut run_pages: Vec<u32> = Vec::new();
        let mut boundaries: Vec<Option<MutCell>> = Vec::new(); // pairs for the parent
        match plan {
            Plan::Leaf(runs) => {
                let n_runs = runs.len();
                for (i, run) in runs.into_iter().enumerate() {
                    let page = if i == 0 && !is_root {
                        pgno // run 0 keeps the identity
                    } else {
                        self.alloc()
                    };
                    self.put_page(
                        page,
                        PageCells {
                            ptype,
                            right_most: 0,
                            cells: run.keep,
                        },
                    );
                    run_pages.push(page);
                    if i + 1 < n_runs {
                        let boundary = if index_tree {
                            run.pushed.expect("index leaf run carries its pushed entry")
                        } else {
                            self.table_pair(page, run.sep)
                        };
                        boundaries.push(Some(boundary));
                    } else {
                        boundaries.push(None);
                    }
                }
            }
            Plan::Interior(runs) => {
                let n_runs = runs.len();
                for (i, run) in runs.into_iter().enumerate() {
                    let page = if i == 0 && !is_root {
                        pgno
                    } else {
                        self.alloc()
                    };
                    self.put_page(
                        page,
                        PageCells {
                            ptype,
                            right_most: run.right_most,
                            cells: run.pairs,
                        },
                    );
                    run_pages.push(page);
                    if i + 1 < n_runs {
                        // The boundary is the RAW separator (a rowid pair
                        // for tables, the stripped entry for indexes) —
                        // the parent update wraps it exactly once.
                        let boundary = if index_tree {
                            run.pushed
                                .expect("index interior run carries its pushed entry")
                        } else {
                            self.table_pair(page, run.sep)
                        };
                        boundaries.push(Some(boundary));
                    } else {
                        boundaries.push(None);
                    }
                }
            }
        }

        // ---- Parent update ----
        if is_root {
            // The root page is rewritten as the new parent: pairs
            // (run_i, boundary_i) for every run but the last, right-most
            // = the last run. Identity kept — rootpages never move.
            let cells: Vec<MutCell> = run_pages
                .iter()
                .zip(boundaries.iter())
                .filter_map(|(&p, b)| {
                    b.as_ref().map(|entry| {
                        if index_tree {
                            self.entry_pair(p, entry)
                        } else {
                            self.table_pair(p, entry.rowid)
                        }
                    })
                })
                .collect();
            let right = *run_pages.last().expect("split produced runs");
            let parent_type = if index_tree { 0x02 } else { 0x05 };
            self.put_page(
                pgno,
                PageCells {
                    ptype: parent_type,
                    right_most: right,
                    cells,
                },
            );
            return Ok(());
        }

        let parent_pgno = path.pop().expect("non-root split has a parent");
        let mut parent = self.take_page(parent_pgno)?;
        // The replacement sequence: pair_i = (run_i, boundary_i); the
        // LAST run re-pairs with the old separator K (when the split
        // page had a pair) or becomes the right-most.
        let last = *run_pages.last().expect("split produced runs");
        if parent.right_most == pgno {
            for (p, b) in run_pages.iter().zip(boundaries.iter()) {
                if let Some(entry) = b {
                    let pair = if index_tree {
                        self.entry_pair(*p, entry)
                    } else {
                        self.table_pair(*p, entry.rowid)
                    };
                    parent.cells.push(pair);
                }
            }
            parent.right_most = last;
        } else {
            let idx = parent
                .cells
                .iter()
                .position(|c| c.left_child == pgno)
                .ok_or_else(|| MutateFallback("split: page not referenced by parent".into()))?;
            let old_pair = parent.cells.remove(idx);
            for (i, (p, b)) in run_pages.iter().zip(boundaries.iter()).enumerate() {
                let is_last = i + 1 == run_pages.len();
                let pair = if is_last {
                    // The last run takes the OLD separator.
                    if index_tree {
                        // Re-pair: the old pair's entry (minus child
                        // prefix) with the new last page.
                        let mut entry = old_pair.clone();
                        entry.bytes = entry.bytes[4..].to_vec();
                        entry.left_child = 0;
                        self.entry_pair(*p, &entry)
                    } else {
                        self.table_pair(*p, old_pair.rowid)
                    }
                } else {
                    let entry = b.as_ref().expect("non-last runs carry boundaries");
                    if index_tree {
                        self.entry_pair(*p, entry)
                    } else {
                        self.table_pair(*p, entry.rowid)
                    }
                };
                parent.cells.insert(idx + i, pair);
            }
        }
        if fits(parent.ptype, &parent.cells, self.usable) {
            self.put_page(parent_pgno, parent);
            return Ok(());
        }
        // The parent overflowed: split it too (recursive propagation).
        let parent_type = parent.ptype;
        self.split_node(path, parent_pgno, parent, parent_type)
    }
}

// ---------------------------------------------------------------------------
// Entry-tree operations (indexes and WITHOUT ROWID tables)
// ---------------------------------------------------------------------------

impl<'a> Mutator<'a> {
    fn cmp_entries(&self, order: &EntryOrder, a: &[Value], b: &[Value]) -> std::cmp::Ordering {
        compare_index_keys(a, b, &order.desc, &order.collations, self.enc)
    }

    /// Binary-search a RAW index-interior page (type 0x02) for `key`:
    /// `(first-greater-or-equal cell index, exact-match index, the child
    /// covering key)`. Decodes only the ~log2(cells) probed separators —
    /// the interior pages never enter the cell model on the descent
    /// path (a full decode costs one allocation per cell).
    #[allow(clippy::type_complexity)]
    fn raw_index_probe(
        &mut self,
        raw: &[u8],
        order: &EntryOrder,
        key: &[Value],
    ) -> MResult<(usize, Option<usize>, u32)> {
        let n = u16::from_be_bytes([raw[3], raw[4]]) as usize;
        let right = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]);
        let child_of = |i: usize| -> u32 {
            let cp = u16::from_be_bytes([raw[12 + i * 2], raw[13 + i * 2]]) as usize;
            u32::from_be_bytes([raw[cp], raw[cp + 1], raw[cp + 2], raw[cp + 3]])
        };
        let mut lo = 0usize;
        let mut hi = n;
        let mut exact = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cp = u16::from_be_bytes([raw[12 + mid * 2], raw[13 + mid * 2]]) as usize;
            if cp + 4 > raw.len() {
                return Err("index interior cell pointer out of page".into());
            }
            let cell = &raw[cp..];
            let payload = self.index_payload(cell, true)?;
            let mv = decode_record_all_enc(&payload, self.enc).map_err(MutateFallback)?;
            match self.cmp_entries(order, &mv, key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Equal => {
                    exact = Some(mid);
                    break;
                }
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        let child = if let Some(i) = exact {
            child_of(i)
        } else if lo < n {
            child_of(lo)
        } else {
            right
        };
        Ok((lo, exact, child))
    }

    /// Binary-search a raw table leaf for `rowid`: the first pointer
    /// position whose rowid >= it (probes parse only the two leading
    /// varints). `Err` propagates a malformed cell; overflow cells are
    /// fine (the rowid precedes the payload).
    fn raw_table_leaf_pos(raw: &[u8], leaf: &RawLeaf, rowid: i64) -> MResult<usize> {
        let cell_rowid = |i: usize| -> MResult<i64> {
            let cp =
                u16::from_be_bytes([raw[leaf.array + i * 2], raw[leaf.array + i * 2 + 1]]) as usize;
            let (_, l1) =
                read_varint(raw, cp).ok_or_else(|| MutateFallback("bad cell varint".into()))?;
            let (rid, _) = read_varint(raw, cp + l1)
                .ok_or_else(|| MutateFallback("bad rowid varint".into()))?;
            Ok(rid)
        };
        let (mut lo, mut hi) = (0usize, leaf.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if rowid <= cell_rowid(mid)? {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok(lo)
    }

    /// The raw table-leaf op fast path (see the section comment).
    /// `None` = structurally inapplicable (fall back to the cell
    /// model); `Some(Err)` = a JOURNAL verify failure (stale journal —
    /// propagate, never fall back); `Some(Ok)` = the leaf is patched
    /// into `raw_pages`.
    fn try_raw_table_leaf(
        &mut self,
        pgno: u32,
        raw: &[u8],
        op: &TableRowOp,
        parent: Option<u32>,
    ) -> Option<MResult<()>> {
        let leaf = raw_leaf(raw)?;
        let pos = match Self::raw_table_leaf_pos(raw, &leaf, op.rowid) {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        let present = pos < leaf.n && {
            let cp = u16::from_be_bytes([raw[leaf.array + pos * 2], raw[leaf.array + pos * 2 + 1]])
                as usize;
            read_varint(raw, cp).and_then(|(_, l1)| read_varint(raw, cp + l1).map(|(r, _)| r))
                == Some(op.rowid)
        };
        match (&op.expect, &op.set) {
            (None, None) => Some(Ok(())),
            (None, Some(rec)) => {
                if present {
                    return Some(Err(format!(
                        "insert: rowid {} already present (stale journal?)",
                        op.rowid
                    )
                    .into()));
                }
                // New cells needing an overflow chain take the model
                // path (chain pages must be allocated).
                let c = make_table_leaf_cell(self.usable, op.rowid, rec);
                if c.ptr_pos.is_some() {
                    return None;
                }
                let patched = match raw_leaf_grow_insert(raw, &leaf, pos, &c.head) {
                    Some(p) => p,
                    // Full leaf: the RAW 2-way split (the canonical
                    // first-touch shape after a dense build) before the
                    // model fallback.
                    None => {
                        if !raw_split_disabled() {
                            return self.try_raw_leaf_split(
                                pgno, raw, &leaf, pos, &c.head, false, parent, op.rowid,
                            );
                        }
                        return None;
                    }
                };
                self.raw_pages.insert(pgno, patched);
                self.touched.insert(pgno);
                self.max_page = self.max_page.max(pgno);
                Some(Ok(()))
            }
            (Some(old), new) => {
                if !present {
                    return Some(Err(format!(
                        "update/delete: rowid {} absent (stale journal?)",
                        op.rowid
                    )
                    .into()));
                }
                let found = inpage_table_payload(raw, &leaf, pos, self.usable)?;
                if found != old.as_slice() {
                    return Some(Err(format!(
                        "update/delete: rowid {} record mismatch (stale journal?)",
                        op.rowid
                    )
                    .into()));
                }
                // A delete that would EMPTY the leaf takes the model
                // path: the model prunes empty table leaves (releasing
                // the page + patching the parent) — an un-pruned empty
                // leaf left mid-tree violates the discipline later ops
                // and the whole-object rebuilds rely on.
                if new.is_none() && leaf.n == 1 {
                    return None;
                }
                let old_extent = {
                    let cp = u16::from_be_bytes([
                        raw[leaf.array + pos * 2],
                        raw[leaf.array + pos * 2 + 1],
                    ]) as usize;
                    match cell_extent(raw, cp, self.usable) {
                        Ok(e) => e,
                        Err(e) => return Some(Err(e.into())),
                    }
                };
                match new {
                    None => {
                        // In-page verify means no chain to free.
                        let patched = raw_leaf_rebuild(raw, &leaf, Some(pos), None, self.usable)?;
                        self.raw_pages.insert(pgno, patched);
                        self.touched.insert(pgno);
                        self.max_page = self.max_page.max(pgno);
                        Some(Ok(()))
                    }
                    Some(rec) => {
                        let c = make_table_leaf_cell(self.usable, op.rowid, rec);
                        if c.ptr_pos.is_some() {
                            return None;
                        }
                        if c.head.len() == old_extent {
                            // Same-extent replace: patch the cell's
                            // bytes in place; nothing moves.
                            let cp = u16::from_be_bytes([
                                raw[leaf.array + pos * 2],
                                raw[leaf.array + pos * 2 + 1],
                            ]) as usize;
                            let mut page = raw.to_vec();
                            page[cp..cp + c.head.len()].copy_from_slice(&c.head);
                            self.raw_pages.insert(pgno, page);
                            self.touched.insert(pgno);
                            self.max_page = self.max_page.max(pgno);
                            Some(Ok(()))
                        } else {
                            let patched = raw_leaf_rebuild(
                                raw,
                                &leaf,
                                Some(pos),
                                Some((pos, &c.head)),
                                self.usable,
                            )?;
                            self.raw_pages.insert(pgno, patched);
                            self.touched.insert(pgno);
                            self.max_page = self.max_page.max(pgno);
                            Some(Ok(()))
                        }
                    }
                }
            }
        }
    }

    /// Binary-search a raw index leaf for `key`: the first pointer
    /// position whose entry >= it (each probe decodes one record;
    /// overflow entries assemble through `raw`, exactly like
    /// `raw_index_probe`). Returns `(pos, exact)`.
    fn raw_entry_leaf_pos(
        &mut self,
        raw: &[u8],
        leaf: &RawLeaf,
        order: &EntryOrder,
        key: &[Value],
    ) -> MResult<(usize, bool)> {
        let mut lo = 0usize;
        let mut hi = leaf.n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let cp = u16::from_be_bytes([raw[leaf.array + mid * 2], raw[leaf.array + mid * 2 + 1]])
                as usize;
            let cell = &raw[cp..];
            let payload = self.index_payload(cell, false)?;
            let mv = decode_record_all_enc(&payload, self.enc).map_err(MutateFallback)?;
            match self.cmp_entries(order, &mv, key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Equal => return Ok((mid, true)),
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok((lo, false))
    }

    /// The raw index-leaf insert fast path (see the section comment).
    fn try_raw_entry_leaf_insert(
        &mut self,
        pgno: u32,
        raw: &[u8],
        order: &EntryOrder,
        key: &[Value],
        parent: Option<u32>,
    ) -> Option<MResult<()>> {
        let leaf = raw_leaf(raw)?;
        let (pos, exact) = match self.raw_entry_leaf_pos(raw, &leaf, order, key) {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        if exact {
            return Some(Err(
                "index insert: key already present (leaf, stale journal?)".into(),
            ));
        }
        // Chain-bearing new entries take the model path.
        let c = make_index_cell(self.usable, key, self.enc);
        if c.ptr_pos.is_some() {
            return None;
        }
        let patched = match raw_leaf_grow_insert(raw, &leaf, pos, &c.head) {
            Some(p) => p,
            // Full leaf: the RAW 2-way split before the model fallback
            // (same rationale as the table path).
            None => {
                if !raw_split_disabled() {
                    return self
                        .try_raw_leaf_split(pgno, raw, &leaf, pos, &c.head, true, parent, 0);
                }
                return None;
            }
        };
        self.raw_pages.insert(pgno, patched);
        self.touched.insert(pgno);
        self.max_page = self.max_page.max(pgno);
        Some(Ok(()))
    }

    /// The raw index-leaf delete fast path (see the section comment).
    /// Index trees NEVER prune empty leaves (the model's own
    /// discipline — the parent's separator must survive), so an
    /// emptying delete stays on the raw path.
    fn try_raw_entry_leaf_delete(
        &mut self,
        pgno: u32,
        raw: &[u8],
        order: &EntryOrder,
        key: &[Value],
        expect_enc: &[u8],
    ) -> Option<MResult<()>> {
        let leaf = raw_leaf(raw)?;
        let (pos, exact) = match self.raw_entry_leaf_pos(raw, &leaf, order, key) {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        if !exact {
            return Some(Err("index delete: key absent (stale journal?)".into()));
        }
        // In-page verify only: an overflowing found entry needs chain
        // assembly AND chain freeing — the model path's job.
        let cp =
            u16::from_be_bytes([raw[leaf.array + pos * 2], raw[leaf.array + pos * 2 + 1]]) as usize;
        let (total, l1) = match read_varint(raw, cp) {
            Some((t, l)) => (t as usize, l),
            None => return Some(Err(MutateFallback("bad cell varint".into()))),
        };
        if total > index_x(self.usable) {
            return None; // overflow entry — model path
        }
        if &raw[cp + l1..cp + l1 + total] != expect_enc {
            return Some(Err(
                "index delete: entry bytes mismatch (stale journal?)".into()
            ));
        }
        let patched = raw_leaf_rebuild(raw, &leaf, Some(pos), None, self.usable)?;
        self.raw_pages.insert(pgno, patched);
        self.touched.insert(pgno);
        self.max_page = self.max_page.max(pgno);
        Some(Ok(()))
    }

    /// The RAW 2-way leaf split (see the section comment). A full leaf
    /// plus one new in-page cell is the canonical first-touch shape
    /// after a dense build; the model path pays decode_page (one
    /// allocation per cell — a leaf packs hundreds) to run the very
    /// same arithmetic this path runs over extents. The plan replicates
    /// `plan_leaf_runs`' 2-way discipline EXACTLY (first strictly
    /// minimal imbalance wins; both sides must fit; index trees push
    /// the boundary cell up, table trees copy the left side's max
    /// rowid), and the parent patch replicates `split_node`'s
    /// propagation for the 2-run case: the old pair is replaced by
    /// [left pair, right pair] (the last run re-pairs with the old
    /// separator), or — when the split page is the parent's right-most
    /// — the left pair appends and the right page becomes right-most.
    /// `None` = anything richer (root split, no 2-way point, parent
    /// full, session-decoded parent, overflow on the new cell): the
    /// model path handles it unchanged.
    #[allow(clippy::too_many_arguments)]
    fn try_raw_leaf_split(
        &mut self,
        pgno: u32,
        raw: &[u8],
        leaf: &RawLeaf,
        pos: usize,
        cell: &[u8],
        index_tree: bool,
        parent_pgno: Option<u32>,
        new_rowid: i64,
    ) -> Option<MResult<()>> {
        // ---- Parent preconditions (checked before anything allocates) ----
        let parent = parent_pgno?;
        if self.pages.contains_key(&parent) {
            return None; // session-decoded parent: the model map is authoritative
        }
        let praw = match self.raw(parent) {
            Ok(p) => p,
            Err(e) => return Some(Err(e)),
        };
        let want: u8 = if index_tree { 0x02 } else { 0x05 };
        if praw.first() != Some(&want) || praw.len() < 12 {
            return None;
        }
        let pn = u16::from_be_bytes([praw[3], praw[4]]) as usize;
        let mut pcontent = u16::from_be_bytes([praw[5], praw[6]]) as usize;
        if pcontent == 0 {
            pcontent = 65536;
        }
        if 12 + pn * 2 > pcontent || pcontent > praw.len() {
            return None;
        }
        let p_right_most = u32::from_be_bytes([praw[8], praw[9], praw[10], praw[11]]);
        // The parent cell referencing pgno (interior cells open with
        // the left-child page number).
        let ptr = |i: usize| -> usize {
            u16::from_be_bytes([praw[12 + i * 2], praw[13 + i * 2]]) as usize
        };
        let usable = self.usable;
        let pext = |i: usize| -> Option<usize> { cell_extent(&praw, ptr(i), usable).ok() };
        let mut idx = None;
        for i in 0..pn {
            let cp = ptr(i);
            if praw.get(cp..cp + 4) == Some(&pgno.to_be_bytes()[..]) {
                idx = Some(i);
                break;
            }
        }
        let idx = idx?;
        let rightmost_case = p_right_most == pgno;
        if !rightmost_case && idx + 1 == pn {
            return None; // the pair past the last cell: inconsistent
        }

        // ---- Combined cell list (old cells + the new one at `pos`) ----
        let n = leaf.n;
        let big = n + 1;
        let cp_of = |i: usize| -> usize {
            u16::from_be_bytes([raw[leaf.array + i * 2], raw[leaf.array + i * 2 + 1]]) as usize
        };
        // Per-position extent; `pos` is the new cell.
        let mut ext: Vec<usize> = Vec::with_capacity(big);
        for p in 0..big {
            if p == pos {
                ext.push(cell.len());
            } else {
                let oi = if p < pos { p } else { p - 1 };
                ext.push(cell_extent(raw, cp_of(oi), usable).ok()?);
            }
        }
        let mut prefix: Vec<usize> = Vec::with_capacity(big + 1);
        prefix.push(0);
        for p in 0..big {
            prefix.push(prefix[p] + ext[p]);
        }
        let total = prefix[big];
        // The combined cell at position `p` (borrowed from `raw` or the
        // new cell) and its rowid.
        let cell_bytes = |p: usize| -> Option<&[u8]> {
            if p == pos {
                return Some(cell);
            }
            let oi = if p < pos { p } else { p - 1 };
            let cp = cp_of(oi);
            Some(&raw[cp..cp + ext[p]])
        };
        let cell_rowid = |p: usize| -> Option<i64> {
            if p == pos {
                return Some(new_rowid);
            }
            let oi = if p < pos { p } else { p - 1 };
            let cp = cp_of(oi);
            let (_, l1) = read_varint(raw, cp)?;
            let (rid, _) = read_varint(raw, cp + l1)?;
            Some(rid)
        };

        // ---- The 2-way plan (plan_leaf_runs' exact discipline) ----
        let fits = |bytes: usize, count: usize| bytes + 8 + 2 * count <= usable;
        let (cut, table_sep, index_boundary) = if index_tree {
            // Balanced 2-way with the boundary cell pushed up: left
            // keeps [0..b), right [b+1..big); both non-empty.
            let mut best: Option<(usize, usize)> = None; // (b, imbalance)
            for b in 1..big.saturating_sub(1) {
                let left = prefix[b];
                let right = total - prefix[b + 1];
                if !fits(left, b) || !fits(right, big - b - 1) {
                    continue;
                }
                let imb = (left as i64 - right as i64).unsigned_abs() as usize;
                if !best.is_some_and(|(_, cur)| imb >= cur) {
                    best = Some((b, imb));
                }
            }
            let (b, _) = best?;
            (b, 0i64, Some(cell_bytes(b)?.to_vec()))
        } else {
            let mut best: Option<(usize, usize)> = None; // (k, imbalance)
            for (k, &left) in prefix.iter().enumerate().take(big).skip(1) {
                let right = total - left;
                if !fits(left, k) || !fits(right, big - k) {
                    continue;
                }
                let imb = (left as i64 - right as i64).unsigned_abs() as usize;
                if !best.is_some_and(|(_, cur)| imb >= cur) {
                    best = Some((k, imb));
                }
            }
            let (k, _) = best?;
            (k, cell_rowid(k - 1)?, None)
        };

        // ---- Parent byte math (still before any allocation) ----
        let old_cp = ptr(idx);
        let old_extent = pext(idx)?;
        if old_extent < 4 || old_cp + old_extent > praw.len() {
            return None;
        }
        let old_bytes = &praw[old_cp..old_cp + old_extent];
        // Table pairs: [u32 child][varint rowid]. Index pairs:
        // [u32 child][entry bytes].
        let mut pair_left: Vec<u8> = Vec::with_capacity(4 + 16);
        pair_left.extend_from_slice(&pgno.to_be_bytes());
        if index_tree {
            pair_left.extend_from_slice(index_boundary.as_ref().expect("planned above"));
        } else {
            write_varint(&mut pair_left, table_sep);
        }
        // The patch: non-right-most replaces one pair with two (the
        // right pair re-uses the old bytes with the child re-pointed —
        // same extent); right-most appends the left pair. Either way
        // the parent grows by pair_left and one cell pointer.
        let mut ptotal = 0usize;
        for i in 0..pn {
            ptotal += pext(i)?;
        }
        if ptotal + pair_left.len() + 12 + 2 * (pn + 1) > usable {
            return None; // the parent would overflow — the model path splits it upward
        }

        // ---- Materialize ----
        let right = self.alloc();
        let leaf_type: u8 = if index_tree { 0x0a } else { 0x0d };
        // Left page: combined [0..cut) — for index trees the boundary
        // cell at `cut` is pushed up, not kept.
        let mut lrefs: Vec<&[u8]> = Vec::with_capacity(cut);
        for p in 0..cut {
            lrefs.push(cell_bytes(p)?);
        }
        // Right page: table [cut..big), index [cut+1..big).
        let rstart = if index_tree { cut + 1 } else { cut };
        let mut rrefs: Vec<&[u8]> = Vec::with_capacity(big - rstart);
        for p in rstart..big {
            rrefs.push(cell_bytes(p)?);
        }
        if lrefs.is_empty() || rrefs.is_empty() {
            return None; // both sides must be non-empty (mirrors the model)
        }
        let left_page = dense_page(leaf_type, &lrefs, usable, 0)?;
        let right_page = dense_page(leaf_type, &rrefs, usable, 0)?;
        // The patched parent.
        let mut prefs: Vec<&[u8]> = Vec::with_capacity(pn + 1);
        let mut pair_right: Vec<u8> = Vec::with_capacity(old_extent);
        let new_right_most = if rightmost_case {
            for i in 0..pn {
                let cp = ptr(i);
                prefs.push(&praw[cp..cp + pext(i)?]);
            }
            prefs.push(&pair_left);
            right
        } else {
            pair_right.extend_from_slice(&right.to_be_bytes());
            pair_right.extend_from_slice(&old_bytes[4..]);
            for i in 0..pn {
                if i == idx {
                    prefs.push(&pair_left);
                    prefs.push(&pair_right);
                } else {
                    let cp = ptr(i);
                    prefs.push(&praw[cp..cp + pext(i)?]);
                }
            }
            p_right_most
        };
        let parent_page = dense_page(want, &prefs, usable, new_right_most)?;

        // ---- Install ----
        MUT_SPLITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.raw_pages.insert(pgno, left_page);
        self.raw_pages.insert(right, right_page);
        self.raw_pages.insert(parent, parent_page);
        self.touched.insert(pgno);
        self.touched.insert(right);
        self.touched.insert(parent);
        self.max_page = self.max_page.max(pgno).max(right).max(parent);
        Some(Ok(()))
    }

    /// Insert one entry (assumed absent — verified during descent).
    fn entry_insert(&mut self, root: u32, order: &EntryOrder, key: &[Value]) -> MResult<()> {
        let mut path: Vec<u32> = Vec::new();
        let mut pgno = root;
        let (mut leaf, leaf_ptype) = loop {
            // RAW descent: a page the session has not decoded takes the
            // binary-search probe (no per-cell allocations); splits put
            // their rewritten interiors into the model map, which stays
            // authoritative.
            if !self.pages.contains_key(&pgno) {
                let raw = self.raw(pgno)?;
                match raw[0] {
                    0x0a => {
                        // RAW fast path first: the common one-entry patch
                        // never enters the cell model.
                        if !raw_fast_disabled() {
                            if let Some(r) = self.try_raw_entry_leaf_insert(
                                pgno,
                                &raw,
                                order,
                                key,
                                path.last().copied(),
                            ) {
                                MUT_RAW_INDEX_OK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                return r;
                            }
                            MUT_RAW_INDEX_FALLBACK
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        MUT_MODEL_LEAF.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let pc = decode_page(pgno, &raw, self.usable).map_err(MutateFallback)?;
                        break (pc, 0x0au8);
                    }
                    0x02 => {
                        let (_lo, exact, child) = self.raw_index_probe(&raw, order, key)?;
                        if let Some(i) = exact {
                            return Err(format!(
                                "index insert: key already present (pair {i}, stale journal?)"
                            )
                            .into());
                        }
                        path.push(pgno);
                        pgno = child;
                        continue;
                    }
                    other => {
                        return Err(
                            format!("index descent: page {pgno} has type 0x{other:02x}").into()
                        );
                    }
                }
            }
            let mut page = self.take_page(pgno)?;
            match page.ptype {
                0x0a => break (page, 0x0au8),
                0x02 => {
                    // Find the first pair whose entry >= key.
                    let mut child = page.right_most;
                    let mut lo = 0usize;
                    let mut hi = page.cells.len();
                    let mut exact = None;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let mv = self.entry_values(&mut page.cells[mid], true)?;
                        match self.cmp_entries(order, &mv, key) {
                            std::cmp::Ordering::Less => lo = mid + 1,
                            std::cmp::Ordering::Equal => {
                                exact = Some(mid);
                                break;
                            }
                            std::cmp::Ordering::Greater => hi = mid,
                        }
                    }
                    if let Some(i) = exact {
                        return Err(format!(
                            "index insert: key already present (pair {i}, stale journal?)"
                        )
                        .into());
                    }
                    if lo < page.cells.len() {
                        child = page.cells[lo].left_child;
                    }
                    self.stash_page(pgno, page);
                    path.push(pgno);
                    pgno = child;
                }
                other => {
                    self.stash_page(pgno, page);
                    return Err(format!("index descent: page {pgno} has type 0x{other:02x}").into());
                }
            }
        };

        // Leaf: binary search the insert position.
        let mut lo = 0usize;
        let mut hi = leaf.cells.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mv = self.entry_values(&mut leaf.cells[mid], false)?;
            if self.cmp_entries(order, &mv, key) == std::cmp::Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < leaf.cells.len() {
            let at = self.entry_values(&mut leaf.cells[lo], false)?;
            if self.cmp_entries(order, &at, key) == std::cmp::Ordering::Equal {
                return Err("index insert: key already present (leaf, stale journal?)".into());
            }
        }
        let cell = self.make_entry_cell(key);
        leaf.cells.insert(lo, cell);
        if !fits(leaf_ptype, &leaf.cells, self.usable) {
            return self.split_node(&mut path, pgno, leaf, leaf_ptype);
        }
        self.put_page(pgno, leaf);
        Ok(())
    }

    /// Delete one entry. When the entry lives as an INTERIOR separator
    /// (index trees' pushed-up boundaries), the successor replaces it
    /// in place and is removed from its leaf — SQLite's own discipline;
    /// no entry ever vanishes.
    fn entry_delete(&mut self, root: u32, order: &EntryOrder, key: &[Value]) -> MResult<()> {
        let expect_enc = encode_record_enc(key, self.enc);
        let mut path: Vec<u32> = Vec::new();
        let mut pgno = root;
        loop {
            // RAW descent fast path: probe the committed bytes without a
            // cell-model decode. Only an EXACT separator match (the
            // entry lives in this interior — successor replacement)
            // forces the decode.
            if !self.pages.contains_key(&pgno) {
                let raw = self.raw(pgno)?;
                if raw[0] == 0x02 {
                    let (_lo, exact, child) = self.raw_index_probe(&raw, order, key)?;
                    if exact.is_none() {
                        path.push(pgno);
                        pgno = child;
                        continue;
                    }
                    // Exact: fall through to the model path below
                    // (take_page decodes, the model search re-finds the
                    // pair, the successor replacement runs).
                } else if raw[0] == 0x0a {
                    // RAW fast path first: the common one-entry delete
                    // never enters the cell model.
                    if !raw_fast_disabled() {
                        if let Some(r) =
                            self.try_raw_entry_leaf_delete(pgno, &raw, order, key, &expect_enc)
                        {
                            MUT_RAW_INDEX_OK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return r;
                        }
                        MUT_RAW_INDEX_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                } else if raw[0] != 0x0a {
                    return Err(
                        format!("index descent: page {pgno} has type 0x{:02x}", raw[0]).into(),
                    );
                }
            }
            let mut page = self.take_page(pgno)?;
            match page.ptype {
                0x0a => {
                    // Leaf: find + verify + remove.
                    let mut lo = 0usize;
                    let mut hi = page.cells.len();
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let mv = self.entry_values(&mut page.cells[mid], false)?;
                        if self.cmp_entries(order, &mv, key) == std::cmp::Ordering::Less {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    if lo >= page.cells.len() {
                        return Err("index delete: key absent (stale journal?)".into());
                    }
                    let found = lo;
                    let at = self.entry_values(&mut page.cells[found], false)?;
                    if self.cmp_entries(order, &at, key) != std::cmp::Ordering::Equal {
                        return Err("index delete: key absent (stale journal?)".into());
                    }
                    let payload = self.index_payload(&page.cells[found].bytes, false)?;
                    if payload != expect_enc {
                        return Err("index delete: entry bytes mismatch (stale journal?)".into());
                    }
                    let removed = page.cells.remove(found);
                    // The removed cell's overflow chain dies with it.
                    self.free_chain_of(&removed.bytes, false, false);
                    // Index trees never prune empty leaves (the parent's
                    // separator entry must survive); the page just sits
                    // empty until a later insert refills it or the next
                    // whole-object rebuild compacts.
                    self.put_page(pgno, page);
                    return Ok(());
                }
                0x02 => {
                    // Interior: exact match = the entry lives HERE.
                    let mut lo = 0usize;
                    let mut hi = page.cells.len();
                    let mut exact = None;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let mv = self.entry_values(&mut page.cells[mid], true)?;
                        match self.cmp_entries(order, &mv, key) {
                            std::cmp::Ordering::Less => lo = mid + 1,
                            std::cmp::Ordering::Equal => {
                                exact = Some(mid);
                                break;
                            }
                            std::cmp::Ordering::Greater => hi = mid,
                        }
                    }
                    if let Some(i) = exact {
                        // Successor-replace: the minimum entry of the
                        // subtree FOLLOWING pair i's child moves into the
                        // pair; the successor is then deleted from its
                        // leaf (its chain moving with it).
                        let next_child = if i + 1 < page.cells.len() {
                            page.cells[i + 1].left_child
                        } else {
                            page.right_most
                        };
                        let child = page.cells[i].left_child;
                        // Free the OLD pair key's chain (the entry dies).
                        let old_pair = page.cells[i].clone();
                        self.free_chain_of(&old_pair.bytes, false, true);
                        let (succ_cell, succ_leaf_pgno, mut succ_leaf) =
                            self.leftmost_leaf_take(next_child)?;
                        // Build the replacement pair: child + the
                        // successor's head (chain included).
                        let mut bytes = Vec::with_capacity(4 + succ_cell.bytes.len());
                        bytes.extend_from_slice(&child.to_be_bytes());
                        bytes.extend_from_slice(&succ_cell.bytes);
                        page.cells[i] = MutCell {
                            bytes,
                            rowid: 0,
                            left_child: child,
                            idx: succ_cell.idx.clone(),
                        };
                        // The successor's leaf loses its first cell —
                        // WITHOUT freeing the chain (it moved up).
                        if succ_leaf.cells.is_empty() {
                            return Err("leftmost leaf empty on successor take".into());
                        }
                        succ_leaf.cells.remove(0);
                        self.put_page(succ_leaf_pgno, succ_leaf);
                        // The modified interior page may now overflow
                        // (the successor head can be larger).
                        let page_type = page.ptype;
                        if fits(page_type, &page.cells, self.usable) {
                            self.put_page(pgno, page);
                            return Ok(());
                        }
                        return self.split_node(&mut path, pgno, page, page_type);
                    }
                    // Descend.
                    let child = if lo < page.cells.len() {
                        page.cells[lo].left_child
                    } else {
                        page.right_most
                    };
                    self.stash_page(pgno, page);
                    path.push(pgno);
                    pgno = child;
                }
                other => {
                    self.stash_page(pgno, page);
                    return Err(format!("index descent: page {pgno} has type 0x{other:02x}").into());
                }
            }
        }
    }

    /// The leftmost leaf of a subtree + its first cell (the subtree's
    /// minimum entry), with the leaf taken for mutation.
    fn leftmost_leaf_take(&mut self, mut pgno: u32) -> MResult<(MutCell, u32, PageCells)> {
        loop {
            let page = self.take_page(pgno)?;
            match page.ptype {
                0x0a => {
                    if page.cells.is_empty() {
                        return Err("leftmost leaf is empty".into());
                    }
                    let first = page.cells[0].clone();
                    return Ok((first, pgno, page));
                }
                0x02 => {
                    let child = if page.cells.is_empty() {
                        page.right_most
                    } else {
                        page.cells[0].left_child
                    };
                    if child == 0 {
                        return Err("interior page with no children".into());
                    }
                    self.stash_page(pgno, page);
                    pgno = child;
                }
                other => {
                    return Err(
                        format!("leftmost descent: page {pgno} has type 0x{other:02x}").into(),
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session entry point + finish
// ---------------------------------------------------------------------------

impl<'a> Mutator<'a> {
    /// Run the full mutation program against the object rooted at
    /// `root`, then produce the outcome (touched pages + exact span /
    /// freelist accounting).
    pub fn run(mut self, root: u32, ops: MutateOps) -> MResult<MutateOutcome> {
        // DEBUG (bug hunt): `RSQL_FORCE_DECLINE=ix` declines every
        // index-shaped session — reproduces mixed mutate+splice commits
        // with the raw paths disabled.
        if let Some(list) = std::env::var_os("RSQL_FORCE_DECLINE") {
            let list = list.to_string_lossy().to_string();
            let hit = match &ops {
                MutateOps::Table(_) => list.contains("tbl"),
                MutateOps::Entries(_, _) => list.contains("ix"),
            };
            if hit {
                return Err("forced decline (debug)".into());
            }
        }
        match ops {
            MutateOps::Table(ops) => {
                for op in ops {
                    self.table_op(root, op)?;
                }
            }
            MutateOps::Entries(order, ops) => {
                for op in ops {
                    match (op.expect, op.set) {
                        (None, None) => {}
                        (None, Some(set)) => self.entry_insert(root, &order, &set)?,
                        (Some(expect), None) => self.entry_delete(root, &order, &expect)?,
                        (Some(expect), Some(set)) => {
                            self.entry_delete(root, &order, &expect)?;
                            self.entry_insert(root, &order, &set)?;
                        }
                    }
                }
            }
        }
        if std::env::var_os("RSQL_MUTATE_VALIDATE").is_some() {
            self.validate(root)?;
        }
        self.finish(root)
    }

    /// Final accounting: the SPAN DELTAS (pages that joined / left
    /// the object's span — O(changes), never O(object pages): the
    /// common commit touches existing pages and moves nothing), the
    /// container-freelist deltas and the touched pages (the caller
    /// byte-compares against the committed view — identical rewrites
    /// commit nothing).
    fn finish(self, root: u32) -> MResult<MutateOutcome> {
        // Pages that left the span: session-freed pages that came from
        // the ORIGINAL span (pruned leaves, dead overflow chains,
        // collapsed children). `span0` is sorted ascending (the
        // coordinator's span bookkeeping guarantees it).
        let removed: Vec<u32> = self
            .session_free
            .iter()
            .copied()
            .filter(|p| self.span0.binary_search(p).is_ok())
            .collect();
        // The root must survive (a root collapse KEEPS the root page;
        // releasing it is a bug, not a shape).
        if removed.binary_search(&root).is_ok() {
            return Err("the root page left the span".into());
        }
        // Pages that joined: this session's allocations still in use.
        let added: Vec<u32> = self
            .allocated
            .iter()
            .copied()
            .filter(|p| !self.session_free.contains(p))
            .collect();
        let took_from_free: Vec<u32> = self
            .popped_from_free
            .difference(&self.session_free)
            .copied()
            .collect();
        let freed: Vec<u32> = self
            .session_free
            .difference(&self.popped_from_free)
            .copied()
            .collect();
        let mut pages: Vec<(u32, Vec<u8>)> = Vec::with_capacity(self.touched.len());
        for &pgno in &self.touched {
            if let Some(pc) = self.pages.get(&pgno) {
                pages.push((pgno, encode_page(pc, self.page_size)));
            } else if let Some(raw) = self.raw_pages.get(&pgno) {
                pages.push((pgno, raw.clone()));
            }
        }
        pages.sort_unstable_by_key(|(p, _)| *p);
        Ok(MutateOutcome {
            root,
            pages,
            added,
            removed,
            max_page: self.max_page,
            took_from_free,
            freed,
        })
    }

    /// Opt-in structural validation (RSQL_MUTATE_VALIDATE=1): walk the
    /// mutated tree from the root, checking child references, key
    /// order and chain ownership against the final span. O(object
    /// pages) — for tests, never the hot path.
    fn validate(&mut self, root: u32) -> MResult<()> {
        let used: BTreeSet<u32> = {
            let mut u: BTreeSet<u32> = self.span0.iter().copied().collect();
            u.extend(self.allocated.iter().copied());
            u.difference(&self.session_free).copied().collect()
        };
        self.validate_page(root, &used, &mut Vec::new())
    }

    fn validate_page(
        &mut self,
        pgno: u32,
        used: &BTreeSet<u32>,
        seen: &mut Vec<u32>,
    ) -> MResult<()> {
        if seen.contains(&pgno) {
            return Err(format!("validate: cycle at page {pgno}").into());
        }
        seen.push(pgno);
        if !used.contains(&pgno) {
            return Err(format!("validate: page {pgno} referenced but not in the span").into());
        }
        let page = self.take_page(pgno)?;
        let interior = matches!(page.ptype, 0x05 | 0x02);
        if interior {
            if page.right_most == 0 || !used.contains(&page.right_most) {
                return Err(format!(
                    "validate: page {pgno} right-most {} not in the span",
                    page.right_most
                )
                .into());
            }
            for (i, c) in page.cells.iter().enumerate() {
                if !used.contains(&c.left_child) {
                    return Err(format!(
                        "validate: page {pgno} pair {i} child {} not in the span",
                        c.left_child
                    )
                    .into());
                }
                if i + 1 < page.cells.len()
                    && c.rowid >= page.cells[i + 1].rowid
                    && page.ptype == 0x05
                {
                    return Err(format!(
                        "validate: page {pgno} table separators out of order at {i}"
                    )
                    .into());
                }
            }
        }
        if page.ptype == 0x0d {
            for w in page.cells.windows(2) {
                if w[0].rowid >= w[1].rowid {
                    return Err(format!("validate: page {pgno} leaf rowids out of order").into());
                }
            }
        }
        if !fits(page.ptype, &page.cells, self.usable) {
            return Err(format!("validate: page {pgno} overflows its budget").into());
        }
        let children: Vec<u32> = if interior {
            let mut c: Vec<u32> = page.cells.iter().map(|x| x.left_child).collect();
            c.push(page.right_most);
            c
        } else {
            Vec::new()
        };
        // Chain ownership of every cell.
        for c in &page.cells {
            let ovf = match page.ptype {
                0x0d => table_overflow_at(&c.bytes, self.usable),
                0x0a => index_overflow_at(&c.bytes, self.usable, false),
                _ => index_overflow_at(&c.bytes, self.usable, true),
            };
            if let Some(at) = ovf {
                let mut next = u32::from_be_bytes([
                    c.bytes[at],
                    c.bytes[at + 1],
                    c.bytes[at + 2],
                    c.bytes[at + 3],
                ]);
                while next != 0 {
                    if !used.contains(&next) {
                        return Err(format!(
                            "validate: page {pgno} overflow chain page {next} not in the span"
                        )
                        .into());
                    }
                    let raw = self.raw(next)?;
                    next = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                }
            }
        }
        self.stash_page(pgno, page);
        for child in children {
            self.validate_page(child, used, seen)?;
        }
        Ok(())
    }
}
