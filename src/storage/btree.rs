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
use crate::storage::concurrent::JournalKind;
use crate::storage::page::{Page, PageId, PageType, PAGE_HEADER_SIZE};
use crate::storage::pager::Pager;
use crate::types::Value;

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
            for (i, slot) in out.iter_mut().enumerate().take(n) {
                let shift = (n - 1 - i) * 7;
                let byte = ((v >> shift) & 0x7F) as u8;
                *slot = if i < n - 1 { byte | 0x80 } else { byte };
            }
            n
        } else {
            // 9 bytes: first 8 bytes hold the high 56 bits (7 bits each),
            // 9th byte holds the low 8 bits.
            let high = v >> 8;
            let low = (v & 0xFF) as u8;
            for (i, slot) in out.iter_mut().enumerate().take(8) {
                let shift = (7 - i) * 7;
                *slot = (((high >> shift) & 0x7F) as u8) | 0x80;
            }
            out[8] = low;
            9
        }
    }

    /// Decode a varint. Returns (value, bytes consumed).
    ///
    /// Inlined 1-3 byte fast path (SQLite's getVarint32 macro shape):
    /// cell lengths, small rowids and keys up to 2^21 resolve without the
    /// general loop — the b-tree searches decode one or two varints per
    /// probe, and the out-of-line loop was the top self-time symbol of a
    /// multi-index insert load.
    #[inline]
    pub fn decode(buf: &[u8]) -> Option<(u64, usize)> {
        match *buf {
            [b0, ..] if b0 < 0x80 => Some((b0 as u64, 1)),
            [b0, b1, ..] if b1 < 0x80 => Some(((((b0 & 0x7F) as u64) << 7) | b1 as u64, 2)),
            [b0, b1, b2, ..] if b2 < 0x80 => Some((
                (((b0 & 0x7F) as u64) << 14) | (((b1 & 0x7F) as u64) << 7) | b2 as u64,
                3,
            )),
            _ => decode_slow(buf),
        }
    }

    #[inline(never)]
    fn decode_slow(buf: &[u8]) -> Option<(u64, usize)> {
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

    #[cfg(test)]
    #[test]
    fn fast_path_matches_the_general_loop() {
        let check = |v: u64| {
            let mut b = [0u8; 9];
            let n = encode(v, &mut b);
            assert_eq!(decode(&b[..n]), decode_slow(&b[..n]), "{v}");
            assert_eq!(decode(&b[..n]), Some((v, n)), "{v}");
            // Trailing bytes never change the result; truncation fails.
            assert_eq!(decode(&b), decode_slow(&b), "{v} padded");
            assert_eq!(decode(&b[..n - 1]), decode_slow(&b[..n - 1]), "{v} cut");
        };
        (0..1u64 << 22).for_each(check);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200_000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            check(x >> (x % 64));
            check(x);
        }
        for v in [
            u64::MAX,
            i64::MIN as u64,
            (-1i64) as u64,
            1 << 56,
            (1 << 56) - 1,
        ] {
            check(v);
        }
    }

    /// Encode a signed i64 as a varint (using SQLite's zig-zag-like encoding).
    pub fn encode_signed(v: i64, out: &mut [u8]) -> usize {
        // SQLite uses two's complement, big-endian, but stored as varint
        // with the sign bit in the MSB of the i64. We cast to u64.
        encode(v as u64, out)
    }

    #[inline]
    pub fn decode_signed(buf: &[u8]) -> Option<(i64, usize)> {
        decode(buf).map(|(v, n)| (v as i64, n))
    }

    /// Encoded byte length of `v` under `encode`/`encode_signed` (1-9).
    #[inline]
    pub fn len_of(v: u64) -> usize {
        if v <= 0x7F {
            1
        } else if v < (1u64 << 56) {
            let mut n = 1;
            let mut max = 0x7Fu64;
            while v > max {
                n += 1;
                max = (max << 7) | 0x7F;
            }
            n
        } else {
            9
        }
    }
}

/// Deterministic LOCAL (in-cell) size for a table-leaf payload, as a
/// pure function of (total, page_size) — the SQLite overflow-cell
/// contract: readers derive the split point from the payload length
/// alone, without knowing the cell's byte span.
///
/// * `total <= page_size - 128`: the whole payload is in-page (no
///   overflow).
/// * Otherwise the tail spills to a chain of Overflow pages; the local
///   prefix is `min_local + (total - min_local) % chain_capacity`,
///   clamped so it always fits the leaf's reserved margin. The formula
///   distributes the tail so chain pages fill evenly.
pub(crate) fn overflow_local_len_for(total: usize, page_size: usize) -> usize {
    let max_local = page_size.saturating_sub(128);
    if total <= max_local {
        return total;
    }
    let chain_cap = page_size.saturating_sub(16);
    let min_local = (page_size / 4).clamp(16, max_local.saturating_sub(8));
    let surplus = min_local + (total - min_local) % chain_cap.max(1);
    if surplus <= max_local {
        surplus
    } else {
        min_local
    }
}

/// Marker bit on an INDEX cell's key-length varint (`RSQLDB06`): the cell
/// follows SQLite's index-cell local-size rule ([`index_local_len_v6`])
/// rather than the table rule. Far above any legal key length (payloads
/// cap at 1 GiB), so an unmarked length always decodes as before — files
/// written by earlier versions read unchanged.
pub(crate) const INDEX_V6_MARK: u64 = 1 << 62;

/// SQLite's index-cell local payload bounds (btreeInitPage, no reserved
/// bytes): `maxLocal = (U-12)*64/255 - 23`, `minLocal = (U-12)*32/255 - 23`.
/// Capping an index cell's in-page bytes at ~1/4 page guarantees at
/// least four cells per index page. The table rule (`page_size - 128`
/// in-page) let a ~4 KB key sit inline, one cell per page — a fanout-1
/// tree whose page count grew quadratically with long keys (3,000 rows
/// with 3 KB TEXT keys: 181k pages, 742 MB, for ~2 MB of data).
pub(crate) fn index_max_local(page_size: usize) -> usize {
    (page_size.saturating_sub(12) * 64 / 255).saturating_sub(23)
}

fn index_min_local(page_size: usize) -> usize {
    (page_size.saturating_sub(12) * 32 / 255).saturating_sub(23)
}

/// In-page prefix length of an index key under the SQLite index rule
/// (the `INDEX_V6_MARK` cells): the whole key when it fits `maxLocal`,
/// else `minLocal + (total - minLocal) % chain_capacity`, or `minLocal`
/// when that exceeds `maxLocal`.
pub(crate) fn index_local_len_v6(total: usize, page_size: usize) -> usize {
    let max_local = index_max_local(page_size);
    if total <= max_local {
        return total;
    }
    let min_local = index_min_local(page_size);
    let chain_cap = page_size.saturating_sub(16).max(1);
    let surplus = min_local + (total - min_local) % chain_cap;
    if surplus <= max_local {
        surplus
    } else {
        min_local
    }
}

/// The key-length varint an index overflow cell stores.
#[inline]
fn index_total_raw(total: u64, v6: bool) -> u64 {
    if v6 {
        total | INDEX_V6_MARK
    } else {
        total
    }
}

/// Decode a stored index cell's key-length varint: `(total key length,
/// in-page length, v6 rule)`. Marked lengths follow the SQLite index
/// rule; unmarked ones the legacy (table) rule they were written under.
pub(crate) fn index_cell_split(raw: u64, page_size: usize) -> (usize, usize, bool) {
    if raw & INDEX_V6_MARK != 0 {
        let total = (raw & !INDEX_V6_MARK) as usize;
        (total, index_local_len_v6(total, page_size), true)
    } else {
        let total = raw as usize;
        (total, overflow_local_len_for(total, page_size), false)
    }
}

/// One RAW-record entry for the cell-serving step path
/// ([`Btree::scan_table_range_cells`]): the record's bytes live in the
/// caller's arena at `[off, off+len)`; `rowid` is the b-tree cell key.
/// The statement decodes the projected columns on demand (one reused
/// serve buffer) instead of materializing a `Vec<Value>` per row.
#[derive(Clone, Copy, Debug)]
pub struct CellEntry {
    pub rowid: i64,
    pub off: u32,
    pub len: u32,
}

/// Outcome of one cells pull: `hit_end` is true when the scan ran off
/// the range end or EOF (as opposed to a budget stop); `last_rowid` is
/// the last COPIED row's rowid — the next pull resumes at
/// `last_rowid + 1`, which lands ON a budget-stopped row.
#[derive(Clone, Copy, Debug)]
pub struct CellScanOutcome {
    pub hit_end: bool,
    /// The walk stopped BEFORE an overflow cell (arena-copying a
    /// multi-page payload costs 2-3x the bytes of the materialized
    /// path's targeted span gather — the torture S09 shape: 64 KB
    /// blobs regressed 5x). The driver then declines cell serving and
    /// the statement continues on the materialized path; the resume
    /// (last_rowid + 1) lands ON the overflow row.
    pub overflow_stop: bool,
    pub last_rowid: i64,
}

/// Mutable walk state threaded through the cells-scan recursion.
struct CellsWalk<'a> {
    arena: &'a mut Vec<u8>,
    entries: &'a mut Vec<CellEntry>,
    budget: usize,
    max_bytes: usize,
    /// False once a budget stop interrupted the walk.
    hit_end: bool,
    /// True when the walk stopped at an overflow cell (see
    /// CellScanOutcome::overflow_stop).
    overflow_stop: bool,
    /// rowid of the last COPIED row (i64::MIN = none yet this pull).
    last: i64,
}

impl CellsWalk<'_> {
    #[inline]
    fn full(&self) -> bool {
        self.entries.len() >= self.budget || self.arena.len() >= self.max_bytes
    }

    #[inline]
    fn copy(&mut self, rowid: i64, bytes: &[u8]) {
        let off = self.arena.len() as u32;
        self.arena.extend_from_slice(bytes);
        self.entries.push(CellEntry {
            rowid,
            off,
            len: bytes.len() as u32,
        });
        self.last = rowid;
    }
}

/// A cell in a B+tree. This is a logical representation; on-disk format
/// is varint-encoded.
#[derive(Clone, Debug)]
pub enum Cell {
    /// Table leaf: (rowid, payload) — payload fully in-page.
    TableLeaf { rowid: i64, payload: Vec<u8> },
    /// Table leaf with an overflow chain: the cell stores a LOCAL prefix
    /// of the payload plus the first overflow page id; the tail lives in
    /// a linked chain of Overflow pages. `total` is the FULL payload
    /// length (local + chain). SQLite-overflow equivalent.
    TableLeafOverflow {
        rowid: i64,
        total: u64,
        local: Vec<u8>,
        overflow: PageId,
    },
    /// Table interior: (left_child_page, key).
    TableInterior { left_child: PageId, key: i64 },
    /// Index leaf: (key, rowid).
    IndexLeaf { key: Vec<u8>, rowid: i64 },
    /// Index leaf with an overflow chain: oversized index keys spill the
    /// tail to Overflow pages exactly like table payloads — the cell
    /// keeps a LOCAL prefix + the first chain page; `total` is the FULL
    /// key length. Split point derived from `total` via
    /// `overflow_local_len_for` (same pure function as table cells), so
    /// readers never need side tables to find it.
    IndexLeafOverflow {
        rowid: i64,
        total: u64,
        local: Vec<u8>,
        overflow: PageId,
        /// SQLite index local-size rule (stored with `INDEX_V6_MARK`);
        /// false for cells written under the legacy table rule.
        v6: bool,
    },
    /// Index interior: (left_child_page, key, rowid).
    IndexInterior {
        left_child: PageId,
        key: Vec<u8>,
        rowid: i64,
    },
    /// Index interior with an overflow-chained separator key (the
    /// separator is a COPY of the left child's max entry — when that
    /// entry's key is oversized, the copy gets its own chain).
    IndexInteriorOverflow {
        left_child: PageId,
        rowid: i64,
        total: u64,
        local: Vec<u8>,
        overflow: PageId,
        /// See `IndexLeafOverflow::v6`.
        v6: bool,
    },
}

impl Cell {
    pub fn key(&self) -> i64 {
        match self {
            Cell::TableLeaf { rowid, .. } => *rowid,
            Cell::TableLeafOverflow { rowid, .. } => *rowid,
            Cell::TableInterior { key, .. } => *key,
            Cell::IndexLeaf { rowid, .. } => *rowid,
            Cell::IndexLeafOverflow { rowid, .. } => *rowid,
            Cell::IndexInterior { rowid, .. } => *rowid,
            Cell::IndexInteriorOverflow { rowid, .. } => *rowid,
        }
    }

    /// Left child page of an interior cell (table or index).
    pub fn left_child(&self) -> PageId {
        match self {
            Cell::TableInterior { left_child, .. } => *left_child,
            Cell::IndexInterior { left_child, .. } => *left_child,
            Cell::IndexInteriorOverflow { left_child, .. } => *left_child,
            _ => 0,
        }
    }

    /// Encoded key bytes of an index cell. For OVERFLOW index cells this
    /// is the LOCAL prefix only — comparisons that must see the full key
    /// go through `Btree::cell_index_key` (chain reassembly). Every
    /// in-flight comparison site was audited to either use the full-key
    /// helper or only need the prefix by construction.
    pub fn index_key(&self) -> &[u8] {
        match self {
            Cell::IndexLeaf { key, .. } | Cell::IndexInterior { key, .. } => key,
            Cell::IndexLeafOverflow { local, .. } | Cell::IndexInteriorOverflow { local, .. } => {
                local
            }
            _ => &[],
        }
    }

    /// Overflow chain head of an index cell (0 = fully in-page key).
    pub fn index_overflow(&self) -> PageId {
        match self {
            Cell::IndexLeafOverflow { overflow, .. }
            | Cell::IndexInteriorOverflow { overflow, .. } => *overflow,
            _ => 0,
        }
    }

    /// Compare an index cell against a target (key, rowid).
    /// Index pages are sorted by (key bytes, rowid). NOTE: for overflow
    /// cells this compares the LOCAL prefix — callers needing total-order
    /// fidelity must reassemble first (`Btree::cell_index_key`).
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
            Cell::TableLeafOverflow {
                rowid,
                total,
                local,
                ..
            } => {
                let k = varint::encode_signed(*rowid, &mut buf);
                let p = varint::encode(*total, &mut buf);
                k + p + local.len() + 4
            }
            Cell::TableInterior { left_child: _, key } => 4 + varint::encode_signed(*key, &mut buf),
            Cell::IndexLeaf { key, rowid } => {
                // Format: varint(rowid) + varint(key_len) + key
                let r = varint::encode_signed(*rowid, &mut buf);
                let kl = varint::encode(key.len() as u64, &mut buf);
                r + kl + key.len()
            }
            Cell::IndexLeafOverflow {
                rowid,
                total,
                local,
                v6,
                ..
            } => {
                // Format: varint(rowid) + varint(TOTAL key_len [| V6 mark]) + local + be_u32(chain)
                let r = varint::encode_signed(*rowid, &mut buf);
                let kl = varint::encode(index_total_raw(*total, *v6), &mut buf);
                r + kl + local.len() + 4
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
            Cell::IndexInteriorOverflow {
                left_child: _,
                rowid,
                total,
                local,
                v6,
                ..
            } => {
                // Format: be_u32(left_child) + varint(rowid) + varint(TOTAL [| V6 mark]) + local + be_u32(chain)
                let r = varint::encode_signed(*rowid, &mut buf);
                let kl = varint::encode(index_total_raw(*total, *v6), &mut buf);
                4 + r + kl + local.len() + 4
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
            Cell::TableLeafOverflow {
                rowid,
                total,
                local,
                overflow,
            } => {
                let k = varint::encode_signed(*rowid, &mut buf);
                out.extend_from_slice(&buf[..k]);
                let p = varint::encode(*total, &mut buf);
                out.extend_from_slice(&buf[..p]);
                out.extend_from_slice(local);
                out.extend_from_slice(&overflow.to_be_bytes());
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
            Cell::IndexLeafOverflow {
                rowid,
                total,
                local,
                overflow,
                v6,
            } => {
                let r = varint::encode_signed(*rowid, &mut buf);
                out.extend_from_slice(&buf[..r]);
                let kl = varint::encode(index_total_raw(*total, *v6), &mut buf);
                out.extend_from_slice(&buf[..kl]);
                out.extend_from_slice(local);
                out.extend_from_slice(&overflow.to_be_bytes());
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
            Cell::IndexInteriorOverflow {
                left_child,
                rowid,
                total,
                local,
                overflow,
                v6,
            } => {
                out.extend_from_slice(&left_child.to_be_bytes());
                let r = varint::encode_signed(*rowid, &mut buf);
                out.extend_from_slice(&buf[..r]);
                let kl = varint::encode(index_total_raw(*total, *v6), &mut buf);
                out.extend_from_slice(&buf[..kl]);
                out.extend_from_slice(local);
                out.extend_from_slice(&overflow.to_be_bytes());
            }
        }
    }

    /// Encode the cell into a FIXED-SIZE stack buffer (no allocation) —
    /// the hot-path sibling of [`Self::encode`] for callers that already
    /// hold `encoded_size()` bytes of scratch. Writes exactly
    /// `self.encoded_size()` bytes; panics are impossible for a buffer of
    /// that length (every branch writes a fixed, pre-computed prefix).
    pub fn encode_into(&self, out: &mut [u8]) {
        let mut w = 0usize;
        let mut buf = [0u8; 10];
        macro_rules! put {
            ($bytes:expr) => {{
                let b: &[u8] = $bytes;
                out[w..w + b.len()].copy_from_slice(b);
                w += b.len();
            }};
        }
        match self {
            Cell::TableLeaf { rowid, payload } => {
                let k = varint::encode_signed(*rowid, &mut buf);
                put!(&buf[..k]);
                let p = varint::encode(payload.len() as u64, &mut buf);
                put!(&buf[..p]);
                put!(payload);
            }
            Cell::TableLeafOverflow {
                rowid,
                total,
                local,
                overflow,
            } => {
                let k = varint::encode_signed(*rowid, &mut buf);
                put!(&buf[..k]);
                let p = varint::encode(*total, &mut buf);
                put!(&buf[..p]);
                put!(local);
                put!(&overflow.to_be_bytes());
            }
            Cell::TableInterior { left_child, key } => {
                put!(&left_child.to_be_bytes());
                let k = varint::encode_signed(*key, &mut buf);
                put!(&buf[..k]);
            }
            Cell::IndexLeaf { key, rowid } => {
                let r = varint::encode_signed(*rowid, &mut buf);
                put!(&buf[..r]);
                let kl = varint::encode(key.len() as u64, &mut buf);
                put!(&buf[..kl]);
                put!(key);
            }
            Cell::IndexLeafOverflow {
                rowid,
                total,
                local,
                overflow,
                v6,
            } => {
                let r = varint::encode_signed(*rowid, &mut buf);
                put!(&buf[..r]);
                let kl = varint::encode(index_total_raw(*total, *v6), &mut buf);
                put!(&buf[..kl]);
                put!(local);
                put!(&overflow.to_be_bytes());
            }
            Cell::IndexInterior {
                left_child,
                key,
                rowid,
            } => {
                put!(&left_child.to_be_bytes());
                let r = varint::encode_signed(*rowid, &mut buf);
                put!(&buf[..r]);
                let kl = varint::encode(key.len() as u64, &mut buf);
                put!(&buf[..kl]);
                put!(key);
            }
            Cell::IndexInteriorOverflow {
                left_child,
                rowid,
                total,
                local,
                overflow,
                v6,
            } => {
                put!(&left_child.to_be_bytes());
                let r = varint::encode_signed(*rowid, &mut buf);
                put!(&buf[..r]);
                let kl = varint::encode(index_total_raw(*total, *v6), &mut buf);
                put!(&buf[..kl]);
                put!(local);
                put!(&overflow.to_be_bytes());
            }
        }
        debug_assert_eq!(w, self.encoded_size());
    }

    /// Decode a cell from a byte buffer at a given page type.
    /// `page_size` is needed to derive the overflow local-size for spilled
    /// payloads (the split point is a pure function of the length).
    pub fn decode(buf: &[u8], page_type: PageType, page_size: u32) -> Result<Self> {
        match page_type {
            PageType::LeafTable => {
                let (rowid, n) = varint::decode_signed(buf)
                    .ok_or_else(|| Error::corruption("truncated leaf rowid"))?;
                let rest = &buf[n..];
                let (plen, m) = varint::decode(rest)
                    .ok_or_else(|| Error::corruption("truncated leaf payload length"))?;
                let rest = &rest[m..];
                let plen_us = plen as usize;
                let local_len = overflow_local_len_for(plen_us, page_size as usize);
                if local_len == plen_us {
                    // Fully in-page payload.
                    if rest.len() < plen_us {
                        return Err(Error::corruption("truncated leaf payload"));
                    }
                    Ok(Cell::TableLeaf {
                        rowid,
                        payload: rest[..plen_us].to_vec(),
                    })
                } else {
                    // Overflow cell: local prefix + 4-byte first chain page.
                    if rest.len() < local_len + 4 {
                        return Err(Error::corruption("truncated overflow leaf cell"));
                    }
                    let overflow =
                        u32::from_be_bytes(rest[local_len..local_len + 4].try_into().unwrap());
                    Ok(Cell::TableLeafOverflow {
                        rowid,
                        total: plen,
                        local: rest[..local_len].to_vec(),
                        overflow,
                    })
                }
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
                let (raw_len, m) = varint::decode(rest)
                    .ok_or_else(|| Error::corruption("truncated index leaf key length"))?;
                let rest = &rest[m..];
                let (key_len_us, local_len, v6) = index_cell_split(raw_len, page_size as usize);
                let key_len = key_len_us as u64;
                if local_len == key_len_us {
                    if rest.len() < key_len_us {
                        return Err(Error::corruption("truncated index leaf key"));
                    }
                    Ok(Cell::IndexLeaf {
                        key: rest[..key_len_us].to_vec(),
                        rowid,
                    })
                } else {
                    // Overflow-chained index key: local prefix + chain head.
                    if rest.len() < local_len + 4 {
                        return Err(Error::corruption("truncated index leaf overflow cell"));
                    }
                    let overflow =
                        u32::from_be_bytes(rest[local_len..local_len + 4].try_into().unwrap());
                    Ok(Cell::IndexLeafOverflow {
                        rowid,
                        total: key_len,
                        local: rest[..local_len].to_vec(),
                        overflow,
                        v6,
                    })
                }
            }
            PageType::InteriorIndex => {
                if buf.len() < 4 {
                    return Err(Error::corruption("truncated index interior child"));
                }
                let left_child = u32::from_be_bytes(buf[..4].try_into().unwrap());
                let (rowid, n) = varint::decode_signed(&buf[4..])
                    .ok_or_else(|| Error::corruption("truncated index interior rowid"))?;
                let rest = &buf[4 + n..];
                let (raw_len, m) = varint::decode(rest)
                    .ok_or_else(|| Error::corruption("truncated index interior key length"))?;
                let rest = &rest[m..];
                let (key_len_us, local_len, v6) = index_cell_split(raw_len, page_size as usize);
                let key_len = key_len_us as u64;
                if local_len == key_len_us {
                    if rest.len() < key_len_us {
                        return Err(Error::corruption("truncated index interior key"));
                    }
                    Ok(Cell::IndexInterior {
                        left_child,
                        key: rest[..key_len_us].to_vec(),
                        rowid,
                    })
                } else {
                    if rest.len() < local_len + 4 {
                        return Err(Error::corruption("truncated index interior overflow cell"));
                    }
                    let overflow =
                        u32::from_be_bytes(rest[local_len..local_len + 4].try_into().unwrap());
                    Ok(Cell::IndexInteriorOverflow {
                        left_child,
                        rowid,
                        total: key_len,
                        local: rest[..local_len].to_vec(),
                        overflow,
                        v6,
                    })
                }
            }
            PageType::Overflow => Err(Error::corruption(
                "overflow page reached as a btree cell page",
            )),
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
    /// Key bytes: the FULL key for in-page cells, the LOCAL prefix when
    /// `overflow != 0` (the tail lives in the chain — reassemble via
    /// `Btree::index_view_key_into` for total-order comparisons).
    key: &'a [u8],
    rowid: i64,
    left_child: u32,
    /// Full key length (== key.len() for in-page cells).
    total: u64,
    /// Overflow chain head (0 = key fully in-page).
    overflow: PageId,
}

/// Byte length of an INDEX leaf cell at `buf` (varint rowid + varint key
/// length + key, or + local + be_u32 chain for overflow cells).
/// Non-allocating; returns None on truncation.
pub(crate) fn index_leaf_cell_size(buf: &[u8], page_size: u32) -> Option<usize> {
    let (_, n1) = varint::decode_signed(buf)?;
    let rest = &buf[n1..];
    let (raw, n2) = varint::decode(rest)?;
    let (klen, local, _) = index_cell_split(raw, page_size as usize);
    if local == klen {
        Some(n1 + n2 + klen)
    } else {
        Some(n1 + n2 + local + 4)
    }
}

/// Byte length of a TABLE leaf cell at `buf` (varint rowid + varint payload
/// length + payload). Non-allocating; returns None on truncation.
fn table_leaf_cell_size(buf: &[u8]) -> Option<usize> {
    let (_, n1) = varint::decode_signed(buf)?;
    let rest = &buf[n1..];
    let (plen, n2) = varint::decode(rest)?;
    Some(n1 + n2 + plen as usize)
}

/// Byte length of a TABLE leaf cell at `buf`, OVERFLOW-AWARE: spilled
/// cells store only the local prefix + a 4-byte chain pointer, so the
/// in-page footprint is `n1 + n2 + local + 4`, not `n1 + n2 + plen`
/// ([`table_leaf_cell_size`] overestimates those and is only safe where
/// callers bail on `size > page_size`). Used by the leaf-compaction
/// path, which must sum EXACT in-page bytes.
pub(crate) fn table_leaf_cell_size_paged(buf: &[u8], page_size: u32) -> Option<usize> {
    let (_, n1) = varint::decode_signed(buf)?;
    let rest = &buf[n1..];
    let (plen, n2) = varint::decode(rest)?;
    let plen = plen as usize;
    let local = overflow_local_len_for(plen, page_size as usize);
    if local == plen {
        Some(n1 + n2 + plen)
    } else {
        Some(n1 + n2 + local + 4)
    }
}

fn decode_index_cell(buf: &[u8], interior: bool, page_size: u32) -> Option<IndexCellView<'_>> {
    let (left_child, after_child) = if interior {
        if buf.len() < 4 {
            return None;
        }
        (u32::from_be_bytes(buf[..4].try_into().unwrap()), 4)
    } else {
        (0, 0)
    };
    let (rowid, n) = varint::decode_signed(&buf[after_child..])?;
    let rest = &buf[after_child + n..];
    let (raw_len, m) = varint::decode(rest)?;
    let rest = &rest[m..];
    let (key_len, local_len, _) = index_cell_split(raw_len, page_size as usize);
    if local_len == key_len {
        if rest.len() < key_len {
            return None;
        }
        Some(IndexCellView {
            key: &rest[..key_len],
            rowid,
            left_child,
            total: key_len as u64,
            overflow: 0,
        })
    } else {
        // Overflow cell: local prefix + be_u32 chain head.
        if rest.len() < local_len + 4 {
            return None;
        }
        Some(IndexCellView {
            key: &rest[..local_len],
            rowid,
            left_child,
            total: key_len as u64,
            overflow: u32::from_be_bytes(rest[local_len..local_len + 4].try_into().unwrap()),
        })
    }
}

/// `(cell.key, cell.rowid) < (key, rowid)` for the index cell at the front
/// of `buf`, WITHOUT decoding the cell's rowid unless the keys tie (it sits
/// before the key: only its length is walked). The hot binary searches of
/// index inserts and descents call this per probe — the full
/// `decode_index_cell` decoded a rowid varint and built a tuple for every
/// probe. `None`: a corrupt cell; `Some(None)`: an overflow cell (the
/// caller's full-key path decides).
#[inline]
fn index_cell_lt(
    buf: &[u8],
    interior: bool,
    page_size: u32,
    key: &[u8],
    rowid: i64,
) -> Option<Option<bool>> {
    let rb = if interior { buf.get(4..)? } else { buf };
    // Skip the rowid varint (1-9 bytes; the 9th carries 8 bits).
    let mut rlen = 0;
    loop {
        let b = *rb.get(rlen)?;
        rlen += 1;
        if b < 0x80 || rlen == 9 {
            break;
        }
    }
    let rest = &rb[rlen..];
    let (raw_len, m) = varint::decode(rest)?;
    let (key_len, local_len, _) = index_cell_split(raw_len, page_size as usize);
    if local_len != key_len {
        return Some(None);
    }
    let ck = rest.get(m..m + key_len)?;
    Some(Some(match ck.cmp(key) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => varint::decode_signed(rb)?.0 < rowid,
    }))
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

impl<'a> Btree<'a> {
    /// Byte-aware split point for the slow rebuild paths: the count-mid
    /// when both halves fit, else the nearest feasible point. A cell
    /// larger than half a page (oversized-key separators) makes the
    /// 50/50-by-COUNT split put more bytes on one side than the page
    /// holds — the in-page write then underflows the content start and
    /// clobbers the page header (observed: type byte overwritten by a
    /// left_child's leading 0x00 byte). Sizes include the 2-byte pointer
    /// slot; `avail` is the page's writable budget (page_size - header).
    /// Returns None only when a single cell exceeds `avail` (impossible
    /// for legal cells: each is capped at max_cell_payload < avail).
    fn byte_aware_mid(cells: &[Cell], avail: usize) -> Option<usize> {
        let total = cells.len();
        if total < 2 {
            // Match the historic `total / 2` (= 0) semantics: a lone cell
            // goes to the RIGHT half (leaf table split reads cells[mid]).
            return Some(total / 2);
        }
        let size = |c: &Cell| c.encoded_size() + 2;
        let total_bytes: usize = cells.iter().map(size).sum();
        if total_bytes <= avail {
            // Everything fits on one side (caller splits anyway): keep the
            // count-mid for balance.
            return Some(total / 2);
        }
        // Feasible mid: left(bytes) <= avail AND right(bytes) <= avail.
        // left grows monotonically with mid — scan from the count-mid
        // outward, preferring the most balanced feasible point.
        let count_mid = total / 2;
        let left_bytes = |m: usize| cells.iter().take(m).map(size).sum::<usize>();
        let feasible = |m: usize| {
            m >= 1 && m < total && left_bytes(m) <= avail && total_bytes - left_bytes(m) <= avail
        };
        if feasible(count_mid) {
            return Some(count_mid);
        }
        for d in 1..total {
            if count_mid >= d && feasible(count_mid - d) {
                return Some(count_mid - d);
            }
            if count_mid + d < total && feasible(count_mid + d) {
                return Some(count_mid + d);
            }
        }
        // Last resort: 1 | rest (each single cell fits by construction).
        if left_bytes(1) <= avail && total_bytes - left_bytes(1) <= avail {
            return Some(1);
        }
        None
    }

    /// Greedy byte-aware run partition for the no-feasible-2-way corner:
    /// distributes `cells` into the FEWEST contiguous runs such that each
    /// run's byte sum (cell sizes + 2-byte pointer slots) fits `avail`.
    /// Every legal cell is ≤ avail on its own (`max_cell_payload` sits
    /// below the page budget), so each run holds at least one cell and
    /// the partition always succeeds. Callers only reach this when the
    /// whole set does NOT fit one page, so the result has ≥ 2 runs.
    ///
    /// Bound: the caller arrives with the existing cells (which fit one
    /// page) plus ONE incoming cell, so the total is ≤ 2 × avail and the
    /// run count stays ≤ 3 in practice — but the algorithm is correct
    /// for any count.
    fn greedy_runs(cells: &[Cell], avail: usize) -> Vec<std::ops::Range<usize>> {
        let size = |c: &Cell| c.encoded_size() + 2;
        let mut runs = Vec::new();
        let mut start = 0usize;
        let mut acc = 0usize;
        for (i, c) in cells.iter().enumerate() {
            let s = size(c);
            if i > start && acc + s > avail {
                runs.push(start..i);
                start = i;
                acc = 0;
            }
            acc += s;
        }
        runs.push(start..cells.len());
        runs
    }

    /// Interior index separator cell for (left_child, key, rowid). The
    /// separator is a COPY of a leaf entry's key — when that key is
    /// oversized the copy gets its OWN fresh overflow chain (the source
    /// cell keeps the original). In-page keys take the ordinary variant.
    fn index_interior_separator(
        &mut self,
        left_child: PageId,
        key: &[u8],
        rowid: i64,
    ) -> Result<Cell> {
        let psz = self.pager.page_size() as usize;
        if key.len() <= index_max_local(psz) {
            Ok(Cell::IndexInterior {
                left_child,
                key: key.to_vec(),
                rowid,
            })
        } else {
            let local_len = index_local_len_v6(key.len(), psz);
            let chain = self.build_overflow_chain(key, local_len)?;
            self.pager.note_index_v6_cell()?;
            Ok(Cell::IndexInteriorOverflow {
                left_child,
                rowid,
                total: key.len() as u64,
                local: key[..local_len].to_vec(),
                overflow: chain,
                v6: true,
            })
        }
    }

    /// Full index key of a stored cell VIEW (in-page: the borrowed bytes
    /// cloned; overflow: chain reassembly). Total-order comparison sites
    /// call this only for overflow views — in-page cells stay on the
    /// zero-copy fast path.
    fn index_view_key(&mut self, v: &IndexCellView<'_>) -> Result<Vec<u8>> {
        if v.overflow == 0 {
            Ok(v.key.to_vec())
        } else {
            let mut out = Vec::with_capacity(v.total as usize);
            self.assemble_overflow_payload_into(v.key, v.total, v.overflow, &mut out)?;
            Ok(out)
        }
    }

    /// Full index key of an owned `Cell`. Insert/split flows pass Cells
    /// whose overflow variant carries only the local prefix — this
    /// reassembles from the cell's own chain. (Insert keeps the source
    /// key alive on the stack, but split-moved cells don't — hence the
    /// chain walk rather than a transient cache field.)
    fn cell_index_key(&mut self, cell: &Cell) -> Result<Vec<u8>> {
        match cell {
            Cell::IndexLeaf { key, .. } | Cell::IndexInterior { key, .. } => Ok(key.clone()),
            Cell::IndexLeafOverflow {
                local,
                total,
                overflow,
                ..
            }
            | Cell::IndexInteriorOverflow {
                local,
                total,
                overflow,
                ..
            } => {
                let mut out = Vec::with_capacity(*total as usize);
                self.assemble_overflow_payload_into(local, *total, *overflow, &mut out)?;
                Ok(out)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Binary-search an interior INDEX page for the first cell whose
    /// (key, rowid) separator is >= the target (key, rowid). Returns
    /// (cell_index, that cell's left_child). When all separators are <
    /// target, returns (n, right_most).
    ///
    /// Overflow-aware: for interior cells whose key spills to a chain, the
    /// view's `key` is the LOCAL prefix. Byte-comparing a strict prefix of
    /// the stored key K against the search key S is only inconclusive when
    /// the prefix is ALSO a strict prefix of S (K vs S then depends on
    /// K's tail) — in that one case the full key is reassembled from the
    /// chain. Common only for oversized-key indexes, where search keys
    /// are themselves oversized; in-page cells never pay anything.
    fn find_index_child(
        &mut self,
        data: &[u8],
        n: u16,
        cell_pointer: impl Fn(u16) -> u16,
        right_most: u32,
        key: &[u8],
        rowid: i64,
    ) -> (usize, u32) {
        let page_size = self.pager.page_size();
        let mut lo: u16 = 0;
        let mut hi: u16 = n;
        while lo < hi {
            // usize arithmetic: a corrupt n_cells can push lo+hi past u16::MAX.
            let mid = ((lo as usize + hi as usize) / 2) as u16;
            let ptr = cell_pointer(mid) as usize;
            // Fast probe: key bytes first, the rowid only on a tie.
            match data
                .get(ptr..)
                .and_then(|c| index_cell_lt(c, true, page_size, key, rowid))
            {
                Some(Some(true)) => {
                    lo = mid + 1;
                    continue;
                }
                Some(Some(false)) => {
                    hi = mid;
                    continue;
                }
                _ => {} // corrupt or overflow: the full decode below
            }
            // Corrupt cell pointer: out-of-range offsets must yield a miss,
            // never a slice panic (SQLite policy: corrupt -> SQLITE_CORRUPT).
            let Some(v) = data
                .get(ptr..)
                .and_then(|c| decode_index_cell(c, true, page_size))
            else {
                break;
            };
            // (v.key, v.rowid) < (key, rowid)  → go right. Ambiguous prefix
            // case (local prefix ⊂ search key): resolve via the full key.
            let go_right = if v.overflow != 0 && key.len() > v.key.len() && key.starts_with(v.key) {
                let mut full = Vec::with_capacity(v.total as usize);
                match self.assemble_overflow_payload_into(v.key, v.total, v.overflow, &mut full) {
                    Ok(()) => (full.as_slice(), v.rowid) < (key, rowid),
                    // Corrupt chain: treat as "go right" conservatively —
                    // the leaf walk below reports the corruption.
                    Err(_) => true,
                }
            } else {
                (v.key, v.rowid) < (key, rowid)
            };
            if go_right {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= n {
            if right_most != 0 || n == 0 {
                (n as usize, right_most)
            } else {
                // right=0 ("no right-most child" — the state delete/rebalance
                // leaves behind): the LAST cell's child is the de-facto right
                // edge. Returning literal 0 would descend the SCHEMA page.
                let ptr = cell_pointer(n - 1) as usize;
                match data
                    .get(ptr..)
                    .and_then(|c| decode_index_cell(c, true, page_size))
                {
                    Some(v) => (n as usize, v.left_child),
                    None => (n as usize, 0),
                }
            }
        } else {
            let ptr = cell_pointer(lo) as usize;
            match data
                .get(ptr..)
                .and_then(|c| decode_index_cell(c, true, page_size))
            {
                Some(v) => (lo as usize, v.left_child),
                None => (n as usize, right_most),
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

// ---------------------------------------------------------------------------
// Leaf hints (SQLite-style cursor hints)
//
// A per-thread advisory cache of "last leaf visited per tree root". A point
// lookup whose key falls inside the remembered leaf's key range can touch
// ONE page instead of descending every level — for a 2-level tree that
// halves the page-lock/Arc-clone traffic; deeper trees win more. The hint
// holds the PageRef itself, so a hit skips the pager cache entirely.
//
// Safety contract (what makes a stale hint harmless):
//   1. Hints are only ever USED as a shortcut probe: a probe that doesn't
//      find the key falls back to the FULL descent. A hint can therefore
//      never fabricate a "found" that isn't there.
//   2. `Pager::write_epoch` packs (pager-instance, write-version). Before
//      any use, the epoch is checked: ANY mutation anywhere (note_write)
//      — including ROLLBACK, which restores older page content — bumps
//      the version, and a NEW Pager object on the same thread gets a
//      fresh instance id. Both cases clear the whole cache, closing the
//      two dangerous staleness holes: pages recycled into other trees,
//      and page ids re-used by a different database object.
//   3. Because the epoch guarantees "no mutation since the bounds were
//      read", the remembered first/last keys stay exact — no live
//      re-verification is needed on the hot path.
//   4. Duplicate-key runs that spill past a leaf boundary fall back to the
//      full descent (see `lookup_index`), so prefix scans stay exact.
// ---------------------------------------------------------------------------

use crate::storage::pager::{PageIdHashBuild, PageRef};
use std::sync::Arc;

/// Biased cell search in a table leaf — SQLite's cursor-ix bias.
///
/// Probes the remembered cell first; a hit costs ONE probe. On a miss to
/// the right, gallops exponentially (1, 2, 4, ... cells past the bias)
/// before a bounded binary search; a miss to the left binary-searches
/// [0, bias). Sequential rowid access (`WHERE id = ?` loops cycling +1,
/// merge cursors, index-scan rowid batches) resolves in 1-2 probes vs
/// log2(cells) ≈ 8-10 for a ~280-1000-cell leaf.
///
/// Exactness is unconditional: the bias only picks the first probe and
/// narrows the search bracket; a completed search that misses means the
/// rowid is absent (cells are sorted, brackets are proven).
///
/// Returns the matching cell index, or None when the rowid is absent.
fn biased_rowid_search(
    borrowed: &crate::storage::page::Page,
    n: usize,
    bias: u32,
    rowid: i64,
) -> Result<Option<usize>> {
    #[inline]
    fn cell_rowid(borrowed: &crate::storage::page::Page, i: usize) -> Result<i64> {
        let ptr = borrowed.cell_pointer(i as u16) as usize;
        let Some((r, _)) = decode_rowid_only(borrowed.cell_slice_checked(ptr)?) else {
            return Err(Error::corruption("truncated leaf rowid in biased search"));
        };
        Ok(r)
    }
    if n == 0 {
        return Ok(None);
    }
    let bias = (bias as usize).min(n - 1);
    let b = cell_rowid(borrowed, bias)?;
    if b == rowid {
        return Ok(Some(bias));
    }
    if b > rowid {
        // Target is left of the bias: plain binary search [0, bias).
        let mut lo = 0usize;
        let mut hi = bias;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let r = cell_rowid(borrowed, mid)?;
            if r == rowid {
                return Ok(Some(mid));
            } else if r < rowid {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        return Ok(None);
    }
    // Target is right of the bias: probe the immediately-next cell (the
    // sequential +1 pattern — cursor stepping, ordered batches — resolves
    // in ONE more probe), then binary-search the remaining bracket.
    // Invariant: cells [0, lo) are all < rowid; cells [hi, n) are all > rowid.
    let mut lo = bias + 1;
    let mut hi = n;
    if lo < hi {
        let r = cell_rowid(borrowed, lo)?;
        if r == rowid {
            return Ok(Some(lo));
        }
        if r < rowid {
            lo += 1;
        } else {
            hi = lo;
        }
    }
    // Bounded binary search in [lo, hi).
    while lo < hi {
        let mid = (lo + hi) / 2;
        let r = cell_rowid(borrowed, mid)?;
        if r == rowid {
            return Ok(Some(mid));
        } else if r < rowid {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(None)
}

/// Binary search of a table LEAF for `rowid`: `Ok(i)` = cell `i` holds
/// it, `Err(pos)` = absent, `pos` is where it would be inserted.
fn table_leaf_search(
    borrowed: &crate::storage::page::Page,
    rowid: i64,
) -> Result<std::result::Result<u16, u16>> {
    let (mut lo, mut hi) = (0u16, borrowed.n_cells());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let ptr = borrowed.cell_pointer(mid) as usize;
        let Some((r, _)) = decode_rowid_only(borrowed.cell_slice_checked(ptr)?) else {
            return Err(Error::corruption("truncated leaf rowid in search"));
        };
        match r.cmp(&rowid) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(Ok(mid)),
        }
    }
    Ok(Err(lo))
}

/// Software-prefetch the page header + cell-pointer array (first 1 KiB).
///
/// A cold-leaf binary search touches the header line, then pointer-array
/// lines, then scattered cell lines — each miss serializes behind the
/// previous read (~100 ns each, ~1 µs per cold page). Issuing prefetches
/// for the search region right after the lock lets those misses proceed
/// in parallel while the lock returns. Warm pages pay ~16 prefetch
/// instructions (~10 ns) and the hints are no-ops.
///
/// `_mm_prefetch` is a pure performance hint: it never faults, so a page
/// being concurrently written stays correct (the reader still takes the
/// mutex before interpreting bytes).
#[inline(always)]
fn prefetch_search_lines(data: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: prefetch is a hint with no fault semantics; the pointer
        // derives from a live slice borrowed under the page lock.
        unsafe {
            let base = data.as_ptr();
            let n = data.len().min(1024);
            let mut off = 0usize;
            while off < n {
                core::arch::x86_64::_mm_prefetch(
                    base.add(off) as *const i8,
                    core::arch::x86_64::_MM_HINT_T0,
                );
                off += 64;
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = data;
}

struct IndexLeafHint {
    page: PageRef,
    /// First and last cell keys of the leaf (full key bytes).
    lo: Vec<u8>,
    hi: Vec<u8>,
    /// Cell index of the last successful lower-bound probe in this leaf —
    /// the search bias for `lookup_index_leaf`'s first probe.
    last_cell: u32,
}

type HintMap<V> = std::collections::HashMap<PageId, V, PageIdHashBuild>;

/// Thread-local INDEX-leaf hint state (single remembered leaf per root,
/// epoch-validated). Table leaves moved to the direct-mapped multi-slot
/// cache (`TABLE_HINTS`) that keeps a working set of leaves hot.
struct LeafHintCache {
    epoch: u64,
    indexes: HintMap<IndexLeafHint>,
}

thread_local! {
    static LEAF_HINTS: std::cell::RefCell<LeafHintCache> =
        std::cell::RefCell::new(LeafHintCache {
            epoch: u64::MAX, // force clear on first use
            indexes: HintMap::default(),
        });

    /// Monotonic per-thread generation of thread-local advisory-cache
    /// writes (table/index leaf hints). The committed-view scope compares
    /// it at arm and drop: a reader that created NO durable advisory
    /// state during its scope needs NO write-epoch bump on exit — the
    /// bump exists only to retire THIS thread's BEGIN-time-stamped
    /// hints, and absent state needs no retirement. Without it, every
    /// concurrent committed-view reader invalidated the WHOLE process's
    /// hint state on every query (hint rebuild per descent + writer
    /// append-hint retirement mid-transaction).
    static ADVISORY_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Read the per-thread advisory-write generation (see ADVISORY_GEN).
#[inline]
pub(crate) fn advisory_state_gen() -> u64 {
    ADVISORY_GEN.with(|c| c.get())
}

#[inline]
fn advisory_state_touched() {
    ADVISORY_GEN.with(|c| c.set(c.get().wrapping_add(1)));
}

/// Clear the hint cache when the epoch moved (any write, rollback, or a
/// different Pager object). Inlined into every accessor below.
#[inline]
fn hint_epoch_matches(epoch: u64) -> bool {
    LEAF_HINTS.with(|c| c.borrow().epoch == epoch)
}

/// Probe the table-leaf hint for `root`. Returns the cached page AND the
/// remembered cell index of the last successful lookup in that leaf — the
/// search bias (see `biased_rowid_search`).
///
/// Slots are keyed by rowid BUCKET (`rowid >> TABLE_HINT_BUCKET_SHIFT`),
/// 2-way set-associative: one descent primes a whole 256-rowid range, so
/// a first-visit sweep over sequential rowids (the fixed-parameter OLTP
/// loop shape) descends ~once per leaf pair instead of once per rowid.
#[inline]
fn table_hint_page(root: PageId, rowid: i64, epoch: u64) -> Option<(PageRef, u32)> {
    TABLE_HINTS.with(|c| {
        let slots = &*c.borrow();
        let pair = table_hint_pair(rowid);
        for slot in pair {
            if let Some(s) = slots.get(slot).and_then(|x| x.as_ref()) {
                if s.epoch == epoch && s.root == root && rowid >= s.lo && rowid <= s.hi {
                    return Some((Arc::clone(&s.page), s.last_cell));
                }
            }
        }
        None
    })
}

/// The two candidate slot indices for a rowid's bucket.
#[inline(always)]
fn table_hint_pair(rowid: i64) -> [usize; 2] {
    let bucket = (rowid >> TABLE_HINT_BUCKET_SHIFT) as usize;
    let base = (bucket << 1) & (TABLE_HINT_SLOTS - 1);
    [base, base | 1]
}

/// Record the table-leaf hint for `root` (bounds read live from the page).
#[inline]
fn set_table_hint(
    root: PageId,
    page: &PageRef,
    lo: i64,
    hi: i64,
    epoch: u64,
    probed_rowid: i64,
    cell: u32,
) {
    // Generation-tagged slots: each entry carries the write-epoch it was
    // recorded under; stale entries fail their per-slot epoch check on
    // probe. Epoch changes are therefore FREE (the old global-clear design
    // paid an O(slots) wipe on EVERY write — which is also why it could
    // not afford more than 64 slots).
    //
    // Fill policy (2-way): keep an existing entry for the SAME page (just
    // refresh it), else replace an empty/stale slot, else replace the slot
    // whose leaf is FARTHER from the probed rowid — the near one is the
    // one this access pattern is actually walking through. This resolves
    // the bucket-straddling-leaf-boundary case without thrashing: both
    // leaves of the straddle live in the pair.
    TABLE_HINTS.with(|c| {
        advisory_state_touched();
        let mut slots = c.borrow_mut();
        let pair = table_hint_pair(probed_rowid);
        // same page already present? refresh in place.
        for slot in pair {
            if let Some(s) = slots[slot].as_mut() {
                if s.page_ref_eq(page) {
                    s.epoch = epoch;
                    s.root = root;
                    s.lo = lo;
                    s.hi = hi;
                    s.last_cell = cell;
                    return;
                }
            }
        }
        let entry = TableHintSlot {
            epoch,
            root,
            page: Arc::clone(page),
            lo,
            hi,
            last_cell: cell,
        };
        // prefer an empty or stale slot
        for slot in pair {
            let take = match slots[slot].as_ref() {
                None => true,
                Some(s) => s.epoch != epoch,
            };
            if take {
                slots[slot] = Some(entry);
                return;
            }
        }
        // both live: evict the one farther from the probed rowid
        let dist = |slot: usize| {
            slots[slot]
                .as_ref()
                .map(|s| {
                    if probed_rowid < s.lo {
                        s.lo - probed_rowid
                    } else if probed_rowid > s.hi {
                        probed_rowid - s.hi
                    } else {
                        0
                    }
                })
                .unwrap_or(i64::MAX)
        };
        let victim = if dist(pair[0]) >= dist(pair[1]) {
            pair[0]
        } else {
            pair[1]
        };
        slots[victim] = Some(entry);
    })
}

/// Update the remembered cell index for a hint hit, so the next lookup's
/// biased search starts from the position that just succeeded. Touches
/// whichever slot of the rowid's pair holds this (root, epoch).
#[inline]
fn bump_table_hint_cell(root: PageId, probed_rowid: i64, cell: u32, epoch: u64) {
    TABLE_HINTS.with(|c| {
        let mut slots = c.borrow_mut();
        for slot in table_hint_pair(probed_rowid) {
            if let Some(s) = &mut slots[slot] {
                if s.epoch == epoch && s.root == root {
                    s.last_cell = cell;
                    return;
                }
            }
        }
    });
}

/// Table-leaf hint slots, keyed by rowid BUCKET with 2-way associativity:
/// pair index = `(rowid >> BUCKET_SHIFT) * 2`, two slots per pair. One
/// descent primes a whole 256-rowid bucket, so sequential first-visit
/// sweeps (fixed-parameter OLTP loops, table scans feeding joins) hit the
/// cache on every rowid after the first per leaf pair — versus a
/// per-rowid keying that descends once per ROWID on first visit. A pair
/// holds both leaves of a bucket that straddles a leaf boundary (a
/// 290-cell leaf holds ~1.13 buckets), eliminating straddle thrash.
/// 1024 slots = 512 pairs ≈ 130k rowids of leaves — the working set of a
/// 1M-row table's hot range. Validated by [lo, hi] + root + epoch.
const TABLE_HINT_SLOTS: usize = 1024;
const TABLE_HINT_BUCKET_SHIFT: i64 = 8;

struct TableHintSlot {
    epoch: u64,
    root: PageId,
    page: PageRef,
    lo: i64,
    hi: i64,
    /// Cell index of the last successful lookup in this leaf — the search
    /// bias (SQLite's cursor-ix bias): sequential or near-sequential rowid
    /// access resolves in 1-2 probes instead of log2(cells).
    last_cell: u32,
}

impl TableHintSlot {
    #[inline]
    fn page_ref_eq(&self, other: &PageRef) -> bool {
        Arc::ptr_eq(&self.page, other)
    }
}

thread_local! {
    static TABLE_HINTS: std::cell::RefCell<Vec<Option<TableHintSlot>>> =
        std::cell::RefCell::new((0..TABLE_HINT_SLOTS).map(|_| None).collect());
}

/// Root-children ROUTING cache: for a scattered-access table (join inner
/// fanouts, random probes), every full descent re-locks the root and
/// re-runs its binary search — but the root's (separator, child) mapping
/// is stable within a write epoch. Entries hold the FULL separator set
/// (recorded in one pass, so a partition_point probe routes exactly like
/// the root's own search would). Only armed in scattered mode (see
/// `Btree::tl_hint_miss_streak`) so single point lookups never pay the
/// recording pass.
///
/// ## Boundedness (the unbounded-growth fix)
///
/// Entries are keyed by `PageId` only. When a table's root SPLITS the
/// tree grows a NEW root — the old root's entry stays behind as dead
/// weight forever (it is still a valid page, but no longer any tree's
/// root: nothing probes it, nothing replaces it, nothing removes it).
/// One stale entry per root split per thread, accumulating for the
/// process lifetime — a slow, permanent RSS leak under write churn.
///
/// The epoch guard makes every entry from a PRIOR epoch useless (the
/// probe path already rejects them), so the fix is the same shape
/// `LeafHintCache` uses: when a recording arrives under a new epoch,
/// drop the WHOLE map. That bounds it to the roots actively recorded
/// in the current epoch — a handful — with zero behavioral change
/// (cross-epoch entries could never produce a hit anyway).
struct RootChildrenCache {
    /// Epoch the current entries were recorded under (`u64::MAX` forces
    /// the initial clear). Epochs pack (instance_id, write_version), so
    /// a different database object also reads as "moved".
    epoch: u64,
    map: std::collections::HashMap<PageId, (u64, Vec<(i64, PageId)>)>,
}
thread_local! {
    static ROOT_CHILDREN: std::cell::RefCell<RootChildrenCache> =
        std::cell::RefCell::new(RootChildrenCache {
            epoch: u64::MAX, // force clear on first use
            map: std::collections::HashMap::default(),
        });
}

/// Probe the root-children routing for `rowid`: the first entry whose
/// separator key >= rowid names the child the root's binary search would
/// take (the last entry, (i64::MAX, rightmost), catches rowids past every
/// separator). Epoch-guarded; None = not recorded (or stale).
#[inline]
fn root_child_page(root: PageId, rowid: i64, epoch: u64) -> Option<PageId> {
    ROOT_CHILDREN.with(|c| {
        let m = &c.borrow().map;
        let (ep, entries) = m.get(&root)?;
        if *ep != epoch {
            return None;
        }
        let idx = entries.partition_point(|&(k, _)| k < rowid);
        entries.get(idx).map(|&(_, p)| p)
    })
}

/// Record the root's full (separator, child) routing under `epoch`.
/// Replaces any previous mapping for this root (an epoch change means
/// the old separators may no longer route correctly); a recording under
/// a NEW epoch first drops every stale entry (see the struct docs —
/// split-off roots would otherwise accumulate forever).
#[inline]
fn set_root_children(root: PageId, epoch: u64, entries: Vec<(i64, PageId)>) {
    ROOT_CHILDREN.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.epoch != epoch {
            cache.map.clear();
            cache.epoch = epoch;
        }
        cache.map.insert(root, (epoch, entries));
    });
}

/// Struct-local TABLE leaf hint: the last leaf visited by THIS B+tree
/// handle, with its exact [lo, hi] rowid bounds and the write epoch it
/// was recorded under. A probe costs ~2 ns (borrow + two integer
/// compares), versus ~40 ns for the thread-local `RefCell<HashMap>`
/// probe — hoisted B+tree handles (join inner loops, rowid batches)
/// check this first and skip the map on every miss.
#[derive(Clone)]
struct StructTableHint {
    page: PageRef,
    lo: i64,
    hi: i64,
    epoch: u64,
    /// Last successful cell index — search bias for hoisted handles.
    last_cell: u32,
}

/// Struct-local INDEX leaf hint (byte-key bounds — order-preserving
/// encoding, so byte order == key order).
#[derive(Clone)]
struct StructIndexHint {
    page: PageRef,
    lo: Vec<u8>,
    hi: Vec<u8>,
    epoch: u64,
    /// Last successful lower-bound cell — search bias for hoisted handles.
    last_cell: u32,
}

/// Probe the index-leaf hint for `root`. Byte comparison — index cell keys
/// use the order-preserving encoding, so byte order == key order. Returns
/// the page plus the remembered search-bias cell index.
#[inline]
fn index_hint_page(root: PageId, key: &[u8], epoch: u64) -> Option<(PageRef, u32)> {
    if !hint_epoch_matches(epoch) {
        return None;
    }
    LEAF_HINTS.with(|c| {
        let c = c.borrow();
        let h = c.indexes.get(&root)?;
        if key >= h.lo.as_slice() && key <= h.hi.as_slice() {
            Some((Arc::clone(&h.page), h.last_cell))
        } else {
            None
        }
    })
}

/// Update the remembered bias cell of the index hint for `root` (only when
/// the entry still lives under this epoch).
#[inline]
fn bump_index_hint_cell(root: PageId, cell: u32, epoch: u64) {
    let mut touched = false;
    LEAF_HINTS.with(|c| {
        let mut c = c.borrow_mut();
        if c.epoch != epoch {
            return;
        }
        if let Some(h) = c.indexes.get_mut(&root) {
            h.last_cell = cell;
            touched = true;
        }
    });
    if touched {
        advisory_state_touched();
    }
}

/// Record the index-leaf hint for `root`.
fn set_index_hint(root: PageId, page: &PageRef, lo: &[u8], hi: &[u8], epoch: u64) {
    advisory_state_touched();
    LEAF_HINTS.with(|c| {
        let mut c = c.borrow_mut();
        if c.epoch != epoch {
            c.epoch = epoch;
            c.indexes.clear();
        }
        let h = c.indexes.entry(root).or_insert_with(|| IndexLeafHint {
            page: Arc::clone(page),
            lo: Vec::new(),
            hi: Vec::new(),
            last_cell: 0,
        });
        h.page = Arc::clone(page);
        h.lo.clear();
        h.lo.extend_from_slice(lo);
        h.hi.clear();
        h.hi.extend_from_slice(hi);
    });
}

/// Lifetime-free snapshot of a `Btree` handle's advisory read state
/// (pinned root + struct-local leaf hints), keyed to a (root, is_index)
/// pair and the pager write-epoch it was captured under. The SQL layer's
/// fast paths cache these per thread so consecutive statements reuse the
/// pinned root page and warm leaf hints instead of re-creating a bare
/// `Btree` (and re-fetching the root page) on every query.
pub struct BtreeHandleState {
    pub root: PageId,
    pub is_index: bool,
    pinned_root: Option<PageRef>,
    pinned_epoch: u64,
    table_leaf: Option<StructTableHint>,
    index_leaf: Option<StructIndexHint>,
}

/// One deferred visitor cell: either an in-page payload (offset/len
/// into `ScanBatch::payload`) or an overflow cell (local prefix in
/// `payload`, full length + chain head for assembly at visit time).
#[derive(Clone, Copy)]
enum ScanCell {
    InPage {
        rowid: i64,
        off: u32,
        len: u32,
    },
    Overflow {
        rowid: i64,
        off: u32,
        len: u32,
        plen: u64,
        chain: u32,
    },
}

/// Deferred leaf-visitor batch: cells are copied out of the leaf
/// UNDER its lock, and the visitor callbacks run AFTER the lock is
/// released. This is what makes RE-ENTRANT scans of the same table
/// work: a correlated subquery evaluated inside a visitor callback
/// (filter evaluation) re-enters `get_page` on this very leaf, and
/// the held page Mutex deadlocks on the same thread — found by the
/// nested-correlation differential probe (`WHERE x > (SELECT ..
/// FROM <same table> ..)` hung forever; pre-existing). The batch is
/// reused across all leaves of one scan (cleared per leaf); overflow
/// assembly uses the batch's own scratch, not a thread-local (a
/// nested scan reusing a thread-local buffer would corrupt the outer
/// visitor's payload). Contract unchanged: the callback must not
/// RETAIN the payload slice past its return.
#[derive(Default)]
struct ScanBatch {
    payload: Vec<u8>,
    cells: Vec<ScanCell>,
    overflow_scratch: Vec<u8>,
}

impl ScanBatch {
    fn clear(&mut self) {
        self.payload.clear();
        self.cells.clear();
    }

    fn push_in_page(&mut self, rowid: i64, bytes: &[u8]) {
        let off = self.payload.len() as u32;
        self.payload.extend_from_slice(bytes);
        self.cells.push(ScanCell::InPage {
            rowid,
            off,
            len: bytes.len() as u32,
        });
    }

    fn push_overflow(&mut self, rowid: i64, local: &[u8], plen: u64, chain: u32) {
        let off = self.payload.len() as u32;
        self.payload.extend_from_slice(local);
        self.cells.push(ScanCell::Overflow {
            rowid,
            off,
            len: local.len() as u32,
            plen,
            chain,
        });
    }

    /// Phase 2 — visit the batched cells with NO page lock held.
    /// Returns the visitor's continue flag.
    fn visit<F: FnMut(i64, &[u8]) -> bool>(&mut self, bt: &mut Btree, f: &mut F) -> Result<bool> {
        let mut idx = 0usize;
        while idx < self.cells.len() {
            let (rowid, off, len, over) = match self.cells[idx] {
                ScanCell::InPage { rowid, off, len } => (rowid, off, len, None),
                ScanCell::Overflow {
                    rowid,
                    off,
                    len,
                    plen,
                    chain,
                } => (rowid, off, len, Some((plen, chain))),
            };
            let cont = match over {
                None => {
                    let start = off as usize;
                    let end = start + len as usize;
                    f(rowid, &self.payload[start..end])
                }
                Some((plen, chain)) => {
                    // Disjoint field borrows: local prefix from
                    // `payload`, assembly into `overflow_scratch`.
                    let ScanBatch {
                        payload,
                        overflow_scratch,
                        ..
                    } = self;
                    overflow_scratch.clear();
                    let start = off as usize;
                    let end = start + len as usize;
                    let local: &[u8] = &payload[start..end];
                    bt.assemble_overflow_payload_into(local, plen, chain, overflow_scratch)?;
                    f(rowid, overflow_scratch)
                }
            };
            if !cont {
                return Ok(false);
            }
            idx += 1;
        }
        Ok(true)
    }
}

pub struct Btree<'a> {
    pub pager: &'a Pager,
    pub root: PageId,
    pub is_index: bool,
    /// Row-journal suppression for the APPEND family: the append entry
    /// points journal the whole op at THEIR boundary (after success) and
    /// must not double-record when their slow paths fall back to
    /// `insert_table` / `insert_index` (which journal on their own
    /// success). See `insert_table_append`.
    journal_suppress: bool,
    /// `insert_table_absent`: a table-leaf placement that finds the rowid
    /// already present returns `InsertResult::Duplicate` (nothing
    /// written) instead of inserting a second cell.
    reject_dup_rowid: bool,
    /// Pinned root page for the read path: avoids a page-cache read-lock +
    /// Arc refcount round-trip on EVERY descent (a 10-row join seeks the
    /// tree 10 times; each seek re-fetched the root). Validated against
    /// the pager's write epoch — any write invalidates the pin and the
    /// next call re-fetches. Write paths never use the pin.
    pinned_root: Option<PageRef>,
    pinned_epoch: u64,
    /// Last table leaf visited by this handle (see `StructTableHint`).
    table_leaf: Option<StructTableHint>,
    /// Last index leaf visited by this handle (see `StructIndexHint`).
    index_leaf: Option<StructIndexHint>,
    /// Consecutive thread-local hint-map misses on the table-lookup path.
    /// After `TL_HINT_DISABLE_AT` misses in a row (scattered rowids —
    /// join fanouts, random probes), the RefCell + slot probe is pure
    /// per-lookup overhead: stop probing until the access pattern
    /// changes (a struct-hint or TL hit resets the streak).
    tl_hint_miss_streak: u32,
    /// CUMULATIVE full root descents made by THIS handle (never reset by
    /// hint hits — unlike the streak above). Join inner fetches alternate
    /// miss/hit/miss/hit across scattered keys, which keeps the streak
    /// below the disable threshold forever; two root passes are enough
    /// to know the access is repeated, so the root-children routing arms
    /// after the second descent and every later one skips the root.
    root_descents: u32,
    /// When armed (bulk mass-delete), every page freed by this handle is
    /// recorded so the operation can FILE-TAIL-TRUNCATE the contiguous
    /// freed suffix afterwards (SQLite's truncate-on-mass-delete: pages
    /// never enter the freelist, the WAL, or the file).
    collect_frees: bool,
    freed_during_op: Vec<PageId>,
    /// The right-most-leaf walk is routing a WRITE (the append family's
    /// entry points arm this around their walks; `max_rowid_hint`'s
    /// read-only walk leaves it off). While armed, every page the walk
    /// traverses is noted as a DECISION READ into the armed concurrent
    /// transaction — a stale view of the right spine would append into
    /// a leaf that is no longer the tree's right-most, extending its
    /// keys past a separator a sibling's split installed (the S4 soak's
    /// stranded-rows corruption). No-op outside the concurrent regime.
    right_walk_for_write: bool,
    /// The index LEAF the latest cell placement landed in (pin taken
    /// after the write) — `insert_index_scattered` hands it back as the
    /// next entry's placement hint.
    last_index_leaf: Option<AppendHint>,
}

/// After this many consecutive thread-local hint misses, the lookup path
/// stops probing the shared hint map (see `tl_hint_miss_streak`).
const TL_HINT_DISABLE_AT: u32 = 16;

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
    /// `reject_dup_rowid` placement: the table leaf already holds the
    /// rowid; nothing was written.
    Duplicate,
    Split {
        new_page: PageId,
        split_key: i64,
        /// For index splits: the key bytes of the left page's max entry.
        split_key_bytes: Option<Vec<u8>>,
        /// Additional siblings from a 3+-way split. A page packed to
        /// near-avail with sizeable cells can receive a mid-key insert
        /// where NO contiguous 2-partition fits — every split point
        /// overloads one half past the page budget (observed: a leaf
        /// holding two ~1.9 KB blob rows takes a ~2.5 KB INSERT OR
        /// REPLACE between them; cells [1717, 2493, 1941] bytes against
        /// a 4084-byte budget — both 2-way points put ≥ 4210 on one
        /// side). The split then distributes the cells across 3+ pages:
        /// `new_page` is the FIRST new sibling (ascending key order)
        /// and `extra_splits` carries the remaining boundaries. Empty
        /// for ordinary 2-way splits — the overwhelmingly common case.
        extra_splits: Vec<ExtraSplit>,
    },
}

/// One additional boundary of a 3+-way split (see `InsertResult::Split`).
/// Same separator semantics as the primary: table trees report the right
/// run's first key (parents apply the -1 gap convention); index trees
/// report the left run's max entry, full key included.
struct ExtraSplit {
    new_page: PageId,
    split_key: i64,
    split_key_bytes: Option<Vec<u8>>,
}

impl InsertResult {
    /// Ordinary 2-way split constructor — no extra siblings.
    fn two_way(new_page: PageId, split_key: i64, split_key_bytes: Option<Vec<u8>>) -> Self {
        InsertResult::Split {
            new_page,
            split_key,
            split_key_bytes,
            extra_splits: Vec::new(),
        }
    }
}

/// A point lookup result.
pub enum LookupResult {
    Found(Vec<u8>),
    NotFound,
}

/// A validated right-most-leaf append hint.
///
/// Pinning `(page, serial, epoch)` captures the exact page OBJECT and its
/// exact mutation state at pin time (see `Page`'s docs). Re-validating all
/// three on every use certifies the page is byte-identical to what was
/// pinned — and since a right-most leaf can only STOP being the right-most
/// leaf by being structurally mutated (a split rewrites it), an
/// identity+state-valid pin whose last cell orders below the incoming
/// entry is a sound APPEND PROOF: no re-pin walk down the right edge is
/// ever needed to keep the hint exact. Any divergence — split, cell
/// insert/delete, rollback restore, page re-read, VACUUM rebuild —
/// changes `serial` or `epoch` and the fast path declines on its own.
///
/// This is what lets scattered-key streams keep their hints: an
/// out-of-order entry simply declines against the leaf's own last cell,
/// takes the general insert path, and leaves the (untouched, still
/// valid) hint in place for the next row. The historical contract —
/// re-walk the right edge after every general insert — cost a full
/// root-to-leaf descent per scattered index entry (3 of S12's 5
/// secondary indexes are permanently non-appendable: they paid it on
/// every row).
#[derive(Debug, Clone, Copy)]
pub struct AppendHint {
    pub page: PageId,
    pub serial: u64,
    pub epoch: u32,
    /// The armed CONCURRENT writer scope (transaction id) at the time
    /// this hint was handed to a caller. A hint is only usable inside
    /// the transaction that established it: the `(serial, epoch)` pin
    /// validates the SHADOW OBJECT's state, and a fresh transaction
    /// materializes FRESH shadows — a cross-transaction hint (the
    /// insert scratch carries it across statement, commit, and retry
    /// boundaries) can pin a leaf whose right-most-ness a sibling
    /// commit's split already moved, appending past the separator that
    /// sibling installed and stranding its committed rows (the S4
    /// soak's residual right-edge corruption). The append entry points
    /// drop hints stamped by a different (or no) scope while a writer
    /// scope is armed — the dropped hint's insert walks the right edge
    /// and marks the spine as decision reads instead. Plain-regime
    /// hints carry `None` and are never gated.
    pub(crate) scope: Option<u64>,
    /// Stream state for adaptive append suppression: consecutive
    /// out-of-order declines against the pinned leaf. Once this crosses
    /// `APPEND_SUPPRESS_AFTER`, the append ATTEMPT is skipped (the
    /// general insert is exactly what the attempt would fall back to) —
    /// a permanently scattered stream (`a = (i*7919)%N`, `d = i%1000`)
    /// then pays ZERO fast-path overhead instead of one declined
    /// fetch+lock+last-cell-decode per entry.
    pub miss_streak: u16,
    /// Rows until the next append attempt while suppressed (0 = attempt
    /// on the next insert). Geometric-ish probing: an ascending stretch
    /// that resumes after a scattered prefix re-engages the fast path
    /// within `APPEND_PROBE_EVERY` rows.
    pub probe_in: u16,
}

/// Out-of-order declines after which the append attempt is suppressed.
const APPEND_SUPPRESS_AFTER: u16 = 6;
/// Probe interval (rows between append attempts) while suppressed.
const APPEND_PROBE_EVERY: u16 = 64;

impl AppendHint {
    /// A fresh pin of `page` with a clean stream state.
    #[inline]
    fn fresh(page: PageId, serial: u64, epoch: u32) -> Self {
        Self {
            page,
            serial,
            epoch,
            scope: None,
            miss_streak: 0,
            probe_in: 0,
        }
    }

    /// Carry the stream state onto a re-pinned leaf (structural
    /// fallbacks change the pin, not the stream's shape).
    #[inline]
    fn carry_stream_state_from(&self, other: &AppendHint) -> Self {
        Self {
            page: self.page,
            serial: self.serial,
            epoch: self.epoch,
            scope: self.scope,
            miss_streak: other.miss_streak,
            probe_in: other.probe_in,
        }
    }
}

impl AppendHint {
    /// Is this hint still pinning `page`'s current object state?
    /// Called with the page lock held.
    #[inline]
    fn still_pins(&self, page: &Page) -> bool {
        page.serial == self.serial && page.epoch == self.epoch
    }
}

/// Outcome of a hinted append attempt against one pinned leaf.
enum AppendTry {
    /// Appended; carries the refreshed hint (post-`touch` epoch) for the
    /// NEXT entry of the stream.
    Appended(AppendHint),
    /// The entry is not past the pinned leaf's OWN last cell — a
    /// scattered stream. The pinned leaf was not modified by this
    /// attempt, so the OLD hint stays valid (the general insert that
    /// follows lands left of it, or mutates it and self-invalidates via
    /// the epoch on the next attempt): the caller keeps the hint and
    /// skips the right-edge re-pin walk entirely.
    OutOfOrder,
    /// Everything else — not a leaf, empty leaf, stale `(serial, epoch)`
    /// pin, oversized entry, no room. The caller takes the general
    /// insert and re-establishes the hint from the right edge.
    Declined,
}

/// Pinned index leaf for a sorted sweep: the leaf, its object-state
/// proof, its HIGH fence (the last cell — an ascending stream can
/// never fall below the leaf's low fence, so the high fence alone
/// decides membership), and the interior parent the establishing
/// descent routed through (empty-leaf recycling needs it).
struct IndexSweepPin {
    page: PageId,
    parent: PageId,
    serial: u64,
    epoch: u32,
    max_key: Vec<u8>,
    max_rowid: i64,
    /// The pinned leaf holds no cells: no fence bounds the next op
    /// (the parent separator is not knowable from the leaf alone), so
    /// every op re-descends. The pin is kept only to carry the
    /// recycle parent after a sweep delete empties the leaf.
    empty: bool,
    /// Monotone search hint: the position the previous op touched in
    /// this leaf. Sorted streams land at non-decreasing positions, so a
    /// gallop from here converges in 1-3 probes (a cold binary search
    /// pays ~8 on a 250-cell leaf).
    pos_hint: u16,
}

/// Outcome of a pinned-leaf attempt.
enum SweepTry {
    /// Applied (or a tolerated miss). `pin_alive` false = the leaf
    /// was emptied and recycled — the pin is dead, the next op
    /// re-descends.
    Done { pin_alive: bool },
    /// Not attempted — stale proof, wrong page type, no room, or an
    /// oversized key. The caller applies the op through the full
    /// per-op path (splits and overflow chains belong to it).
    Declined,
}

impl<'a> Btree<'a> {
    pub fn new(pager: &'a Pager, root: PageId, is_index: bool) -> Self {
        Self {
            pager,
            root,
            is_index,
            journal_suppress: false,
            reject_dup_rowid: false,
            pinned_root: None,
            pinned_epoch: 0,
            table_leaf: None,
            index_leaf: None,
            tl_hint_miss_streak: 0,
            root_descents: 0,
            collect_frees: false,
            freed_during_op: Vec::new(),
            right_walk_for_write: false,
            last_index_leaf: None,
        }
    }

    /// Thread-local hint probe with access-pattern adaptation (see
    /// `tl_hint_miss_streak`). Returns the hint on hit; on miss, counts
    /// the streak and eventually stops paying the probe cost.
    #[inline]
    fn tl_table_hint(&mut self, rowid: i64, epoch: u64) -> Option<(PageRef, u32)> {
        if self.tl_hint_miss_streak >= TL_HINT_DISABLE_AT {
            return None;
        }
        match table_hint_page(self.root, rowid, epoch) {
            Some(h) => {
                self.tl_hint_miss_streak = 0;
                Some(h)
            }
            None => {
                self.tl_hint_miss_streak = self.tl_hint_miss_streak.saturating_add(1);
                None
            }
        }
    }

    /// Record a page freed by this handle (when armed).
    #[inline]
    fn note_freed(&mut self, id: PageId) {
        if self.collect_frees && id != 0 {
            self.freed_during_op.push(id);
        }
    }

    /// Export this handle's advisory state for cross-statement reuse
    /// (see `BtreeHandleState`). Cheap: clones one Arc + optional hint
    /// bounds.
    pub fn export_handle_state(&self) -> BtreeHandleState {
        BtreeHandleState {
            root: self.root,
            is_index: self.is_index,
            pinned_root: self.pinned_root.clone(),
            pinned_epoch: self.pinned_epoch,
            table_leaf: self.table_leaf.clone(),
            index_leaf: self.index_leaf.clone(),
        }
    }

    /// Import advisory state exported by a previous handle for the SAME
    /// (root, is_index) — a no-op when the roots differ or a write has
    /// occurred since the state was captured (epoch mismatch).
    pub fn import_handle_state(&mut self, st: BtreeHandleState) {
        if st.root != self.root || st.is_index != self.is_index {
            return;
        }
        // Stale-pinned detection is handled lazily by `root_page()`
        // (epoch compare), so a mismatched epoch simply makes the pin a
        // miss; hints carry their own epoch checks. Import unconditionally.
        self.pinned_root = st.pinned_root;
        self.pinned_epoch = st.pinned_epoch;
        self.table_leaf = st.table_leaf;
        self.index_leaf = st.index_leaf;
    }

    /// Probe the struct-local TABLE leaf hint. ~2 ns; checked before the
    /// thread-local hint map. Returns the page and the search-bias cell.
    #[inline]
    fn probe_struct_table_hint(&self, rowid: i64, epoch: u64) -> Option<(PageRef, u32)> {
        let h = self.table_leaf.as_ref()?;
        if h.epoch != epoch {
            return None;
        }
        if rowid >= h.lo && rowid <= h.hi {
            Some((Arc::clone(&h.page), h.last_cell))
        } else {
            None
        }
    }

    /// Probe the struct-local INDEX leaf hint (byte bounds). Returns the
    /// page plus the remembered search-bias cell.
    #[inline]
    fn probe_struct_index_hint(&self, key: &[u8], epoch: u64) -> Option<(PageRef, u32)> {
        let h = self.index_leaf.as_ref()?;
        if h.epoch != epoch {
            return None;
        }
        if key >= h.lo.as_slice() && key <= h.hi.as_slice() {
            Some((Arc::clone(&h.page), h.last_cell))
        } else {
            None
        }
    }

    /// Record both the struct-local and thread-local table hints.
    /// `cell` is the just-found cell index (the search bias); 0 means
    /// "unknown — start unbiased".
    #[inline]
    fn record_table_hint(
        &mut self,
        page: &PageRef,
        lo: i64,
        hi: i64,
        epoch: u64,
        probed_rowid: i64,
        cell: u32,
    ) {
        self.table_leaf = Some(StructTableHint {
            page: Arc::clone(page),
            lo,
            hi,
            epoch,
            last_cell: cell,
        });
        set_table_hint(self.root, page, lo, hi, epoch, probed_rowid, cell);
    }

    /// Note a successful cell index on the CURRENT hints (struct-local +
    /// thread-local slot), so the next lookup's biased search starts from
    /// the position that just succeeded.
    #[inline]
    fn note_table_cell(&mut self, probed_rowid: i64, cell: u32, epoch: u64) {
        if let Some(h) = &mut self.table_leaf {
            h.last_cell = cell;
        }
        bump_table_hint_cell(self.root, probed_rowid, cell, epoch);
    }

    /// Record both the struct-local and thread-local index hints.
    #[inline]
    fn record_index_hint(&mut self, page: &PageRef, lo: &[u8], hi: &[u8], epoch: u64) {
        self.index_leaf = Some(StructIndexHint {
            page: Arc::clone(page),
            lo: lo.to_vec(),
            hi: hi.to_vec(),
            epoch,
            last_cell: 0,
        });
        set_index_hint(self.root, page, lo, hi, epoch);
    }

    /// Note a successful lower-bound cell on the current index hints
    /// (struct-local + thread-local) so the next lookup's biased search
    /// starts from the position that just succeeded.
    #[inline]
    fn note_index_cell(&mut self, cell: u32, epoch: u64) {
        if let Some(h) = &mut self.index_leaf {
            h.last_cell = cell;
        }
        bump_index_hint_cell(self.root, cell, epoch);
    }

    /// The root page, pinned across seeks when no write has happened since
    /// the last fetch (read-only statements reuse one Arc for every seek).
    #[inline]
    fn root_page(&mut self) -> Result<PageRef> {
        let epoch = self.pager.write_epoch();
        if let Some(p) = self.pinned_root.take() {
            if self.pinned_epoch == epoch {
                // Pin still valid — reuse.
                let out = p.clone();
                self.pinned_root = Some(p);
                return Ok(out);
            }
            // Stale — drop and re-fetch below.
        }
        let p = self.pager.get_page(self.root)?;
        self.pinned_root = Some(p.clone());
        self.pinned_epoch = epoch;
        Ok(p)
    }

    /// Post-build warm tap: descend to the leftmost and rightmost leaves,
    /// verify they decode, and seed the thread-local leaf-hint cache.
    ///
    /// Two purposes:
    /// 1. Cheap structural validation of a freshly built tree (page types
    ///    and boundary cells must decode — catches split bugs early).
    /// 2. The descent runs the READ path (page fetch, lock, cell decode,
    ///    hint seeding) right after a write storm, so the first user query
    ///    doesn't pay the read-path wake-up (~40 µs of allocator pool
    ///    carving + cold code on the first read after heavy writes —
    ///    measured in examples/probe_storm.rs).
    pub fn warm_read_path(&mut self) -> Result<()> {
        let epoch = self.pager.write_epoch();
        // Leftmost then rightmost leaf.
        for side in [0u8, 1] {
            let mut page_id = self.root;
            loop {
                let page = self.pager.get_page(page_id)?;
                let borrowed = page.lock();
                let pt = borrowed.page_type()?;
                match pt {
                    PageType::Overflow => {
                        // Never a valid tree node — treat as no hint.
                        break;
                    }
                    PageType::LeafTable => {
                        let n = borrowed.n_cells() as usize;
                        if n > 0 {
                            // Corrupt pages may hold out-of-range cell
                            // pointers: degrade to no-hint, never panic.
                            let lo = borrowed
                                .cell_slice(0)
                                .ok()
                                .and_then(varint::decode_signed)
                                .map(|(r, _)| r)
                                .unwrap_or(i64::MIN);
                            let hi = borrowed
                                .cell_slice((n - 1) as u16)
                                .ok()
                                .and_then(varint::decode_signed)
                                .map(|(r, _)| r)
                                .unwrap_or(i64::MAX);
                            drop(borrowed);
                            self.record_table_hint(&page, lo, hi, epoch, lo, 0);
                        }
                        break;
                    }
                    PageType::LeafIndex => {
                        let n = borrowed.n_cells() as usize;
                        if n > 0 {
                            let psz = borrowed.page_size();
                            // Overflow-bound keys are skipped (filter): a
                            // truncated local prefix as a hint bound would
                            // misroute lookups — no hint is strictly safe.
                            let lo = borrowed
                                .cell_slice(0)
                                .ok()
                                .and_then(|s| decode_index_cell(s, false, psz))
                                .filter(|c| c.overflow == 0)
                                .map(|c| c.key.to_vec());
                            let hi = borrowed
                                .cell_slice((n - 1) as u16)
                                .ok()
                                .and_then(|s| decode_index_cell(s, false, psz))
                                .filter(|c| c.overflow == 0)
                                .map(|c| c.key.to_vec());
                            if let (Some(lo), Some(hi)) = (lo, hi) {
                                drop(borrowed);
                                set_index_hint(self.root, &page, &lo, &hi, epoch);
                            }
                        }
                        break;
                    }
                    PageType::InteriorTable => {
                        let n = borrowed.n_cells();
                        let next = if side == 0 {
                            // Leftmost child: cell 0's left pointer.
                            if n == 0 {
                                borrowed.right_most_pointer()
                            } else {
                                // Bounds-checked: a corrupt cell pointer
                                // must not panic the child read.
                                borrowed
                                    .cell_slice(0)
                                    .ok()
                                    .and_then(|s| s.get(..4))
                                    .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
                                    .unwrap_or(0)
                            }
                        } else {
                            borrowed.right_most_pointer()
                        };
                        drop(borrowed);
                        if next == 0 {
                            return Err(Error::corruption("interior page with null child"));
                        }
                        page_id = next;
                    }
                    PageType::InteriorIndex => {
                        let n = borrowed.n_cells();
                        let next = if side == 0 {
                            if n == 0 {
                                borrowed.right_most_pointer()
                            } else {
                                // Bounds-checked: a corrupt cell pointer
                                // must not panic the child read.
                                borrowed
                                    .cell_slice(0)
                                    .ok()
                                    .and_then(|s| s.get(..4))
                                    .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
                                    .unwrap_or(0)
                            }
                        } else {
                            borrowed.right_most_pointer()
                        };
                        drop(borrowed);
                        if next == 0 {
                            return Err(Error::corruption("interior page with null child"));
                        }
                        page_id = next;
                    }
                }
            }
        }
        Ok(())
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
            journal_suppress: false,
            reject_dup_rowid: false,
            pinned_root: None,
            pinned_epoch: 0,
            table_leaf: None,
            index_leaf: None,
            tl_hint_miss_streak: 0,
            root_descents: 0,
            collect_frees: false,
            freed_during_op: Vec::new(),
            right_walk_for_write: false,
            last_index_leaf: None,
        })
    }

    /// Search the hinted table leaf for `rowid`. The epoch has already
    /// been checked by the caller (no mutation since the bounds were
    /// recorded), so the remembered bounds are exact and the page content
    /// is unchanged — a straight binary search is sufficient. Returns
    /// `Ok(None)` only on structural surprises (wrong page type, corrupt
    /// cell), which the caller treats as "fall back to the full descent".
    fn lookup_table_leaf(
        &mut self,
        page_ref: &PageRef,
        rowid: i64,
        bias: u32,
    ) -> Result<Option<LookupResult>> {
        let borrowed = page_ref.lock();
        let pt = borrowed.page_type()?;
        if pt != PageType::LeafTable {
            return Ok(None);
        }
        let n = borrowed.n_cells() as usize;
        if n == 0 {
            return Ok(None);
        }
        match biased_rowid_search(&borrowed, n, bias, rowid)? {
            Some(cell) => {
                let cell_ptr = borrowed.cell_pointer(cell as u16) as usize;
                let cell = Cell::decode(
                    borrowed.cell_slice_checked(cell_ptr)?,
                    pt,
                    borrowed.page_size(),
                )?;
                match cell {
                    Cell::TableLeaf { payload, .. } => Ok(Some(LookupResult::Found(payload))),
                    Cell::TableLeafOverflow {
                        local,
                        total,
                        overflow,
                        ..
                    } => {
                        // Lock order leaf → overflow pages is global (chains
                        // are only ever entered from a decoded leaf cell).
                        let payload = self.assemble_overflow_payload(&local, total, overflow)?;
                        Ok(Some(LookupResult::Found(payload)))
                    }
                    _ => unreachable!(),
                }
            }
            None => {
                // In bounds (verified via the hint) but not present: the rowid is
                // genuinely absent — keys are sorted and the bounds are exact.
                Ok(Some(LookupResult::NotFound))
            }
        }
    }

    /// Point lookup that decodes the payload UNDER the page lock via `f`,
    /// skipping the intermediate payload `Vec` copy that `lookup_table`
    /// returns. `f` receives the borrowed payload bytes; its result is
    /// returned as-is. Returns `Ok(None)` when the rowid is absent.
    pub fn lookup_table_with<R>(
        &mut self,
        rowid: i64,
        f: impl FnOnce(&[u8]) -> Result<R>,
    ) -> Result<Option<R>> {
        // Concurrent regime: this lookup's leaf fetch is a ROW-level
        // semantic read (see concurrent::PointReadScope).
        let _point = self.pager.point_read_scope(self.root, rowid);
        // --- Hint probe --------------------------------------------------
        // Struct-local FIRST (~2 ns), then the thread-local map (~40 ns):
        // a hoisted B+tree handle probing scattered rowids misses both, but
        // the struct probe makes the miss nearly free. The slot also carries
        // the last successful CELL INDEX — the biased search below resolves
        // sequential rowids in 1-2 probes instead of log2(cells).
        let epoch = self.pager.write_epoch();
        'hint: {
            let (page_ref, bias) = match self.probe_struct_table_hint(rowid, epoch) {
                Some(h) => {
                    self.tl_hint_miss_streak = 0;
                    h
                }
                None => match self.tl_table_hint(rowid, epoch) {
                    Some(h) => h,
                    None => break 'hint,
                },
            };
            let borrowed = page_ref.lock();
            prefetch_search_lines(&borrowed.data);
            if borrowed.page_type()? != PageType::LeafTable {
                break 'hint; // wrong page type — full descent
            }
            let n = borrowed.n_cells() as usize;
            match biased_rowid_search(&borrowed, n, bias, rowid)? {
                Some(cell) => {
                    let cell_ptr = borrowed.cell_pointer(cell as u16) as usize;
                    let rest = borrowed.cell_slice_checked(cell_ptr)?;
                    let Some((_rid, rn)) = decode_rowid_only(rest) else {
                        break 'hint;
                    };
                    let Some((plen, pn)) = varint::decode(&rest[rn..]) else {
                        break 'hint;
                    };
                    let start = rn + pn;
                    let local_len = overflow_local_len_for(plen as usize, borrowed.data.len());
                    if local_len != plen as usize {
                        // Spilled row: local prefix + 4-byte chain head;
                        // assemble after releasing the leaf lock.
                        if start + local_len + 4 > rest.len() {
                            break 'hint;
                        }
                        let local = rest[start..start + local_len].to_vec();
                        let chain = u32::from_be_bytes(
                            rest[start + local_len..start + local_len + 4]
                                .try_into()
                                .unwrap(),
                        );
                        drop(borrowed);
                        self.note_table_cell(rowid, cell as u32, epoch);
                        let payload = self.assemble_overflow_payload(&local, plen, chain)?;
                        let out = f(&payload)?;
                        return Ok(Some(out));
                    }
                    let end = start + plen as usize;
                    if end > rest.len() {
                        break 'hint;
                    }
                    let out = f(&rest[start..end])?;
                    drop(borrowed);
                    self.note_table_cell(rowid, cell as u32, epoch);
                    return Ok(Some(out));
                }
                None => {
                    // Completed search, no match: the epoch-checked hint
                    // guarantees the remembered bounds are exact, so the
                    // rowid is absent.
                    return Ok(None);
                }
            }
        }
        // --- Full descent --------------------------------------------------
        // Root-children ROUTING: once this handle has descended through
        // the root twice, the root's (separator, child) set is recorded
        // (one pass) and every subsequent descent enters the covering
        // child directly — skipping the root's lock + binary search.
        // Armed by the cumulative `root_descents` counter (NOT the miss
        // streak: alternating miss/hit fetch patterns — join inner fanouts
        // — reset the streak and would never arm) and bounded at two so
        // single point lookups never pay the recording pass.
        self.root_descents = self.root_descents.saturating_add(1);
        let scattered = self.root_descents >= 2 || self.tl_hint_miss_streak >= TL_HINT_DISABLE_AT;
        let mut page_id = if scattered {
            root_child_page(self.root, rowid, epoch).unwrap_or(self.root)
        } else {
            self.root
        };
        loop {
            // Root pin: reuse the pinned Arc while no write has occurred
            // (saves a cache read-lock + refcount round-trip per seek).
            let page = if page_id == self.root {
                self.root_page()?
            } else {
                self.pager.get_page(page_id)?
            };
            let borrowed = page.lock();
            prefetch_search_lines(&borrowed.data);
            let pt = borrowed.page_type()?;
            match pt {
                PageType::LeafTable => {
                    let n = borrowed.n_cells() as usize;
                    if n > 0 {
                        let lo_key = borrowed
                            .cell_slice(0)
                            .ok()
                            .and_then(|s| decode_rowid_only(s).map(|(r, _)| r));
                        let hi_key = borrowed
                            .cell_slice(n as u16 - 1)
                            .ok()
                            .and_then(|s| decode_rowid_only(s).map(|(r, _)| r));
                        if let (Some(lo), Some(hi)) = (lo_key, hi_key) {
                            self.record_table_hint(&page, lo, hi, epoch, rowid, 0);
                        }
                    }
                    // Descent search: bias 0 (plain binary). The record
                    // above just seeded the hints; the bias pays off on
                    // subsequent hint-hit lookups.
                    match biased_rowid_search(&borrowed, n, 0, rowid)? {
                        Some(cell) => {
                            let cell_ptr = borrowed.cell_pointer(cell as u16) as usize;
                            let rest = borrowed.cell_slice_checked(cell_ptr)?;
                            let (_rid, rn) = decode_rowid_only(rest)
                                .ok_or_else(|| Error::corruption("truncated rowid"))?;
                            let (plen, pn) = varint::decode(&rest[rn..])
                                .ok_or_else(|| Error::corruption("truncated payload length"))?;
                            let start = rn + pn;
                            let local_len =
                                overflow_local_len_for(plen as usize, borrowed.data.len());
                            if local_len != plen as usize {
                                // Spilled row: assemble local prefix + chain.
                                if start + local_len + 4 > rest.len() {
                                    return Err(Error::corruption("truncated overflow cell"));
                                }
                                let local = rest[start..start + local_len].to_vec();
                                let chain = u32::from_be_bytes(
                                    rest[start + local_len..start + local_len + 4]
                                        .try_into()
                                        .unwrap(),
                                );
                                drop(borrowed);
                                self.note_table_cell(rowid, cell as u32, epoch);
                                let payload =
                                    self.assemble_overflow_payload(&local, plen, chain)?;
                                let out = f(&payload)?;
                                return Ok(Some(out));
                            }
                            let end = start + plen as usize;
                            if end > rest.len() {
                                return Err(Error::corruption("truncated payload"));
                            }
                            let out = f(&rest[start..end])?;
                            drop(borrowed);
                            self.note_table_cell(rowid, cell as u32, epoch);
                            return Ok(Some(out));
                        }
                        None => return Ok(None),
                    }
                }
                PageType::InteriorTable => {
                    let n = borrowed.n_cells() as usize;
                    let mut next = borrowed.right_most_pointer();
                    let mut lo = 0usize;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                        if cell_ptr + 4 > borrowed.data.len() {
                            break;
                        }
                        let _left_child = u32::from_be_bytes(
                            borrowed.data[cell_ptr..cell_ptr + 4].try_into().unwrap(),
                        );
                        let (key, _) = varint::decode_signed(&borrowed.data[cell_ptr + 4..])
                            .ok_or_else(|| Error::corruption("truncated interior key in lookup"))?;
                        if rowid <= key {
                            next = _left_child;
                            hi = mid;
                        } else {
                            lo = mid + 1;
                        }
                    }
                    // Root-children routing record (armed mode only):
                    // one full pass over the root's separators arms the
                    // probe above for every subsequent descent in this
                    // epoch. Unarmed handles (first descent, single point
                    // lookups) never reach this.
                    if page_id == self.root && scattered && n > 0 {
                        let mut entries: Vec<(i64, PageId)> = Vec::with_capacity(n + 1);
                        for i in 0..n {
                            let cell_ptr = borrowed.cell_pointer(i as u16) as usize;
                            if cell_ptr + 4 > borrowed.data.len() {
                                break;
                            }
                            let child = u32::from_be_bytes(
                                borrowed.data[cell_ptr..cell_ptr + 4].try_into().unwrap(),
                            );
                            if let Some((key, _)) =
                                varint::decode_signed(&borrowed.data[cell_ptr + 4..])
                            {
                                entries.push((key, child));
                            }
                        }
                        entries.push((i64::MAX, borrowed.right_most_pointer()));
                        set_root_children(self.root, epoch, entries);
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
    ///
    /// A per-thread leaf hint (see the leaf-hints module) short-circuits
    /// the whole descent when the rowid falls inside the last-visited
    /// leaf's range: one page touch instead of one per level.
    pub fn lookup_table(&mut self, rowid: i64) -> Result<LookupResult> {
        let _point = self.pager.point_read_scope(self.root, rowid);
        // --- Hint probe: try the remembered leaf directly ---------------
        let epoch = self.pager.write_epoch();
        {
            let hinted = match self.probe_struct_table_hint(rowid, epoch) {
                Some(h) => {
                    self.tl_hint_miss_streak = 0;
                    Some(h)
                }
                None => self.tl_table_hint(rowid, epoch),
            };
            if let Some((page_ref, bias)) = hinted {
                if let Ok(Some(res)) = self.lookup_table_leaf(&page_ref, rowid, bias) {
                    return Ok(res);
                }
                // fall through to the full descent on any miss
            }
        }
        // --- Full descent ------------------------------------------------
        let mut page_id = self.root;
        loop {
            // Root pin (see the lookup_table descent).
            let page = if page_id == self.root {
                self.root_page()?
            } else {
                self.pager.get_page(page_id)?
            };
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
                    if n > 0 {
                        // Record the leaf hint (exact bounds read live).
                        let lo_key = borrowed
                            .cell_slice(0)
                            .ok()
                            .and_then(|s| decode_rowid_only(s).map(|(r, _)| r));
                        let hi_key = borrowed
                            .cell_slice(n as u16 - 1)
                            .ok()
                            .and_then(|s| decode_rowid_only(s).map(|(r, _)| r));
                        if let (Some(lo), Some(hi)) = (lo_key, hi_key) {
                            self.record_table_hint(&page, lo, hi, epoch, rowid, 0);
                        }
                    }
                    // Biased search by rowid (cells are stored sorted).
                    match biased_rowid_search(&borrowed, n, 0, rowid)? {
                        Some(cell_idx) => {
                            let cell_ptr = borrowed.cell_pointer(cell_idx as u16) as usize;
                            // Found — decode the full cell ONCE.
                            let cell = Cell::decode(
                                borrowed.cell_slice_checked(cell_ptr)?,
                                pt,
                                borrowed.page_size(),
                            )?;
                            match cell {
                                Cell::TableLeaf { payload, .. } => {
                                    drop(borrowed);
                                    self.note_table_cell(rowid, cell_idx as u32, epoch);
                                    return Ok(LookupResult::Found(payload));
                                }
                                Cell::TableLeafOverflow {
                                    local,
                                    total,
                                    overflow,
                                    ..
                                } => {
                                    drop(borrowed);
                                    self.note_table_cell(rowid, cell_idx as u32, epoch);
                                    let payload =
                                        self.assemble_overflow_payload(&local, total, overflow)?;
                                    return Ok(LookupResult::Found(payload));
                                }
                                _ => unreachable!(),
                            }
                        }
                        None => return Ok(LookupResult::NotFound),
                    }
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
    /// Maximum payload stored fully IN-PAGE. Larger payloads spill to an
    /// overflow chain (local prefix + linked Overflow pages). The margin
    /// covers the page header, the cell pointer, the rowid/payload-length
    /// varints, and the 4-byte overflow page pointer — an overflow cell is
    /// always ≥ 8 bytes SMALLER than this, which keeps the overflow-cell
    /// format unambiguous against a plain in-page cell of the same length.
    fn max_cell_payload(&self) -> usize {
        self.pager.page_size() as usize - 128
    }

    /// Data capacity of one overflow chain page (page size minus the
    /// 12-byte page header and 4-byte next pointer).
    fn overflow_page_capacity(&self) -> usize {
        self.pager.page_size() as usize - 16
    }

    /// SQLite's practical blob ceiling (SQLITE_MAX_LENGTH default).
    pub const MAX_PAYLOAD: usize = 1 << 30;

    /// Write `payload[local_len..]` into a fresh chain of Overflow pages.
    /// Returns the first page id (0 only when nothing needed to spill).
    ///
    /// The whole chain is allocated in ONE bulk pager call (one cache
    /// critical section + one LRU lock instead of one per page) and the
    /// pages are filled through the handed-out PageRefs — no per-page
    /// `get_page` round trip, no re-lock of the previous page to set its
    /// next pointer (its guard is still held). A 64 KB blob = ~17 chain
    /// pages; the old loop paid ~3 lock round trips per page.
    fn build_overflow_chain(&mut self, payload: &[u8], local_len: usize) -> Result<PageId> {
        let cap = self.overflow_page_capacity();
        let rest = &payload[local_len..];
        if rest.is_empty() {
            return Ok(0);
        }
        self.build_overflow_chain_rest(rest, cap)
    }

    /// Chain builder over the EXACT spill slice (all bytes past the local
    /// prefix): the direct-write insert path already holds the payload as
    /// `(small prefix, body slice)`, so the spill region is contiguous in
    /// the source `Value`'s own buffer — pages are filled straight from
    /// it, no intermediate payload materialization.
    fn build_overflow_chain_rest(&mut self, rest: &[u8], cap: usize) -> Result<PageId> {
        let rest_len = rest.len();
        debug_assert!(!rest.is_empty());
        let npages = rest_len.div_ceil(cap);
        // Fresh (growth) pages come UNZEROED — the fill below overwrites
        // every byte: [0, 16) by `init_overflow` (type + counts + next),
        // [16, 16+take) by the payload copy, and the LAST page's tail
        // [16+take, psz) by an explicit memset. A 4 KB calloc per chain
        // page was pure waste (17 pages x memset per 64 KB blob insert).
        // Freelist-recycled pages (if any) are still zeroed by the pager.
        let pages = self.pager.allocate_pages_opts(npages, false)?;
        let first = pages.first().map(|(id, _)| *id).unwrap_or(0);
        let mut off = 0usize;
        let is_last_page = pages.len() - 1;
        // Owned guard of the previous chain page, held across the loop
        // boundary so `set_overflow_next` needs no second lock
        // acquisition (an owned guard carries its Arc — no borrow tie to
        // the loop binding).
        let mut prev: Option<parking_lot::ArcMutexGuard<parking_lot::RawMutex, Page>> = None;
        for (i, (page_id, page_ref)) in pages.into_iter().enumerate() {
            if let Some(mut pg) = prev.take() {
                pg.set_overflow_next(page_id);
                drop(pg);
            }
            let mut p = page_ref.lock_arc();
            p.init_overflow();
            let take = (rest_len - off).min(cap);
            p.overflow_data_mut()[..take].copy_from_slice(&rest[off..off + take]);
            if i == is_last_page && take < cap {
                // Final page: zero the dead tail so no uninitialized (or
                // allocator-scratch) bytes ever reach the WAL or file.
                let data = p.overflow_data_mut();
                data[take..].fill(0);
            }
            off += take;
            prev = Some(p);
        }
        Ok(first)
    }

    /// Free every page in an overflow chain (delete/update of a spilled
    /// row). Safe against cycles: bounds the walk by the file's page count.
    fn free_overflow_chain(&mut self, first: PageId) -> Result<()> {
        let mut cur = first;
        let max_pages = self.pager.n_pages() as usize + 4;
        let mut steps = 0usize;
        while cur != 0 {
            steps += 1;
            if steps > max_pages {
                return Err(Error::corruption(format!(
                    "overflow chain cycle starting at page {first}"
                )));
            }
            let next = {
                let page_ref = self.pager.get_page(cur)?;
                let p = page_ref.lock();
                let pt = p.page_type()?;
                if pt != PageType::Overflow {
                    return Err(Error::corruption(format!(
                        "overflow chain hit non-overflow page {} ({:?})",
                        cur, pt
                    )));
                }
                p.overflow_next()
            };
            self.pager.free_page(cur)?;
            self.note_freed(cur);
            cur = next;
        }
        Ok(())
    }

    /// Reassemble the FULL payload of an overflow cell: local prefix +
    /// every chain page's data. The returned length is exactly `total`.
    pub(crate) fn assemble_overflow_payload(
        &mut self,
        local: &[u8],
        total: u64,
        first: PageId,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.assemble_overflow_payload_into(local, total, first, &mut out)?;
        Ok(out)
    }

    /// `assemble_overflow_payload` into a caller-provided buffer (cleared
    /// first, capacity retained) — lets scans reuse one assembly buffer
    /// across all overflow rows instead of malloc+free per row.
    pub(crate) fn assemble_overflow_payload_into(
        &mut self,
        local: &[u8],
        total: u64,
        first: PageId,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        // Reborrow through a mutable local so the borrow checker sees
        // disjoint uses (the loop below passes `&mut *buf` per call).
        let buf = &mut *out;
        let total = total as usize;
        if total < local.len() {
            return Err(Error::corruption(format!(
                "overflow cell local prefix {} longer than payload {}",
                local.len(),
                total
            )));
        }
        // Corrupt files can carry multi-exabyte length varints: clamp the
        // reservation (the walk below grows/validates the real length).
        // Reserve the FULL payload up front: the walk appends `total -
        // local.len()` bytes across the chain, and without this each
        // chain page triggers a realloc+memcpy of the growing buffer —
        // for a 64KB blob that's ~16 doublings of a ~32KB average = ~512KB
        // of memcpy per row (8x the payload itself).
        buf.clear();
        buf.reserve(
            total
                .min(1 << 20)
                .saturating_sub(buf.capacity().min(total.min(1 << 20))),
        );
        buf.extend_from_slice(local);
        // Chain part: batched under ONE cache read-guard per resident run
        // (the per-page `read_overflow_page_append` walked 16 RwLock
        // acquisitions per 64KB blob — the lock overhead was ~15% of the
        // scan on bandwidth-bound hardware).
        let want = total - local.len();
        let got =
            self.pager
                .gather_overflow_chain_range(first, local.len(), local.len(), total, buf)?;
        if got != want {
            return Err(Error::corruption(format!(
                "overflow chain shorter than payload ({} < {})",
                local.len() + got,
                total
            )));
        }
        Ok(())
    }

    /// Make a leaf cell for (rowid, payload): in-page when it fits,
    /// overflow-spilled when it does not. The local-prefix size uses the
    /// SAME deterministic formula as `Cell::decode` (readers derive the
    /// split from the payload length alone).
    fn make_leaf_cell(&mut self, rowid: i64, payload: &[u8]) -> Result<Cell> {
        if payload.len() <= self.max_cell_payload() {
            return Ok(Cell::TableLeaf {
                rowid,
                payload: payload.to_vec(),
            });
        }
        if payload.len() > Self::MAX_PAYLOAD {
            return Err(Error::InvalidArgument(format!(
                "string or blob too big ({} bytes; max is {})",
                payload.len(),
                Self::MAX_PAYLOAD
            )));
        }
        let page_size = self.pager.page_size() as usize;
        let local_len = overflow_local_len_for(payload.len(), page_size);
        debug_assert!(local_len + 4 <= self.max_cell_payload());
        let overflow = self.build_overflow_chain(payload, local_len)?;
        Ok(Cell::TableLeafOverflow {
            rowid,
            total: payload.len() as u64,
            local: payload[..local_len].to_vec(),
            overflow,
        })
    }

    pub fn insert_table(&mut self, rowid: i64, payload: &[u8]) -> Result<()> {
        let _structural = self.pager.structural_scope();
        // Concurrent-writer row journal: record the op AFTER success so
        // the journal never contains phantom effects of failed statements
        // (a statement-level abort would otherwise leave journal/pages
        // drift). Any error invalidates the whole journal — the commit then
        // falls back to page-granularity conflict semantics (abort on
        // conflict), never a bogus merge.
        let r = self.insert_table_inner(rowid, payload);
        if r.is_ok() {
            if !self.journal_suppress {
                self.pager.note_row_write(
                    self.root,
                    false,
                    JournalKind::Insert,
                    rowid,
                    &[],
                    payload,
                );
            }
        } else {
            self.pager.note_journal_invalidated();
        }
        r
    }

    /// Insert `rowid` unless the table already holds it: ONE root-to-leaf
    /// descent serves both the conflict probe and the placement (SQLite's
    /// OP_NotExists seek followed by an OP_Insert that reuses the cursor
    /// position) — the INSERT path used to pay a `lookup_table` descent
    /// and then a second descent in `insert_table`. `Ok(false)` = the
    /// rowid is present and nothing was written.
    pub fn insert_table_absent(&mut self, rowid: i64, payload: &[u8]) -> Result<bool> {
        // Concurrent regime: the absence probe must stay a recorded
        // semantic point read (a sibling's commit of the same rowid is
        // caught at validation) — the fused descent runs inside the
        // structural scope and records none, so take the probe + insert.
        if self.pager.concurrent_scope_armed() {
            if let LookupResult::Found(_) = self.lookup_table(rowid)? {
                return Ok(false);
            }
            self.insert_table(rowid, payload)?;
            return Ok(true);
        }
        let _structural = self.pager.structural_scope();
        // As insert_table_inner: announce the write before any page
        // changes (a duplicate only costs an advisory-cache epoch bump).
        self.pager.note_write();
        self.reject_dup_rowid = true;
        let r = self
            .make_leaf_cell(rowid, payload)
            .and_then(|cell| self.place_cell_root_aware(cell));
        self.reject_dup_rowid = false;
        match r {
            Ok(true) => {
                if !self.journal_suppress {
                    self.pager.note_row_write(
                        self.root,
                        false,
                        JournalKind::Insert,
                        rowid,
                        &[],
                        payload,
                    );
                }
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn insert_table_inner(&mut self, rowid: i64, payload: &[u8]) -> Result<()> {
        // Notify the pager that a write is about to happen. This maintains
        // the O(1) dirty-page counter used by `flush()`'s fast path
        // (see `Pager::note_write`).
        self.pager.note_write();
        let cell = self.make_leaf_cell(rowid, payload)?;
        self.insert_cell_root_aware(cell)
    }

    /// Place an already-built cell through the normal descent + split
    /// machinery, growing the root when the split reaches it. Extracted
    /// from `insert_table_inner` so the spilled-append path can place a
    /// pre-built overflow cell WITHOUT materializing a contiguous
    /// payload first.
    fn insert_cell_root_aware(&mut self, cell: Cell) -> Result<()> {
        self.place_cell_root_aware(cell).map(|_| ())
    }

    /// [`Self::insert_cell_root_aware`] reporting whether the cell was
    /// placed (`false` only under `reject_dup_rowid`, for a rowid the
    /// table already holds).
    fn place_cell_root_aware(&mut self, cell: Cell) -> Result<bool> {
        match self.insert_into_page(self.root, cell)? {
            InsertResult::Done => Ok(true),
            InsertResult::Duplicate => Ok(false),
            InsertResult::Split {
                new_page,
                split_key,
                split_key_bytes,
                extra_splits,
            } => {
                // The root split: create a new root pointing to the old and
                // new pages (3+-way splits: one cell per boundary, the LAST
                // new page becomes the right-most child).
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
                // Additional boundaries (3+-way split): each reports the
                // first key of its right run; the page left of it is the
                // previously installed sibling.
                let mut left = new_page;
                for ex in &extra_splits {
                    let cell = Cell::TableInterior {
                        left_child: left,
                        key: ex.split_key - 1,
                    };
                    self.insert_cell_into_page(new_root, &cell)?;
                    left = ex.new_page;
                }
                // The last sibling takes over the right-most slot.
                self.pager
                    .get_page(new_root)?
                    .lock()
                    .set_right_most_pointer(left);
                self.pager.note_root_split(old_root, new_root);
                if crate::executor::dbg_index_trace() {
                    eprintln!(
                        "[root-split:tab] old={} new={} split_key={}",
                        old_root, new_root, split_key
                    );
                }
                self.root = new_root;
                Ok(true)
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
        let _structural = self.pager.structural_scope();
        let root = self.root;
        self.journal_suppress = true;
        self.right_walk_for_write = true;
        let r = self
            .insert_table_append_inner(rowid, payload, None)
            .map(|_| ());
        self.right_walk_for_write = false;
        self.journal_suppress = false;
        self.journal_typed_result(root, JournalKind::Insert, rowid, &[], payload, r)
    }

    /// Append with a LEAF HINT from a previous append in the SAME statement:
    /// skips the root-to-leaf descent entirely when the hinted leaf is still
    /// the right-most leaf with room (the overwhelmingly common case in bulk
    /// sequential inserts). The hint is validated per use — wrong type,
    /// non-monotonic rowid, stale object state, or a full page falls back
    /// to the full descent. Returns the new hint (the leaf that received
    /// this row).
    ///
    /// SAFETY of the hint: `AppendHint` pins the page OBJECT and its
    /// mutation epoch, so any intervening structural change (split,
    /// rollback restore, page re-read — even across statements via the
    /// insert scratch) is detected on the next use, never applied.
    pub fn insert_table_append_hinted(
        &mut self,
        rowid: i64,
        payload: &[u8],
        hint: Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        let _structural = self.pager.structural_scope();
        let root = self.root;
        self.journal_suppress = true;
        self.right_walk_for_write = true;
        // Cross-scope hint guard (concurrent regime): the scratch-carried
        // hint may predate this transaction — its pin cannot see sibling
        // installs through a fresh shadow object. See `AppendHint::scope`.
        let hint = match self.pager.armed_writer_scope() {
            Some(txn) => hint.filter(|h| h.scope == Some(txn)),
            None => hint,
        };
        let r = self.insert_table_append_inner(rowid, payload, hint);
        // Stamp the outgoing hint with the CURRENT scope so its next use
        // (this transaction only) passes the guard above.
        let r = r.map(|opt| {
            opt.map(|mut h| {
                h.scope = self.pager.armed_writer_scope();
                h
            })
        });
        self.right_walk_for_write = false;
        self.journal_suppress = false;
        self.journal_typed_result(root, JournalKind::Insert, rowid, &[], payload, r)
    }

    /// Shared journal tail for the hooked TABLE entry points: records the
    /// op after success (respecting suppression — the append family
    /// suppresses the nested `insert_table` fallback journal), invalidates
    /// the journal on failure (journal/pages drift would otherwise make a
    /// merge replay unsound), and passes `r` through unchanged.
    fn journal_typed_result<T>(
        &mut self,
        root: PageId,
        kind: JournalKind,
        rowid: i64,
        key: &[u8],
        payload: &[u8],
        r: Result<T>,
    ) -> Result<T> {
        match r {
            Ok(v) => {
                if !self.journal_suppress {
                    self.pager
                        .note_row_write(root, false, kind, rowid, key, payload);
                }
                Ok(v)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    /// DIRECT-WRITE spilled append: like `insert_table_append_hinted`
    /// but for a payload handed over as two parts — `[prefix][body]` —
    /// where `body` is the trailing huge BLOB/TEXT column's bytes still
    /// resident in the caller's `Value`. The overflow chain is filled
    /// STRAIGHT from `body` and the leaf cell takes `prefix` + the
    /// body's local head: the full payload never materializes in an
    /// intermediate buffer (saves one full-body memcpy — and its
    /// allocation — per row on blob-table loads).
    ///
    /// `Ok(None)` = could not take the fast path (hint stale, leaf full
    /// / not a leaf, non-monotonic rowid): the caller re-encodes the
    /// full payload and falls back to `insert_table_append_hinted` /
    /// `insert_table`. No bytes were written in that case.
    pub fn insert_table_append_spilled(
        &mut self,
        rowid: i64,
        prefix: &[u8],
        body: &[u8],
        hint: Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        let _structural = self.pager.structural_scope();
        let root = self.root;
        self.journal_suppress = true;
        self.right_walk_for_write = true;
        // Cross-scope hint guard — see `AppendHint::scope`.
        let hint = match self.pager.armed_writer_scope() {
            Some(txn) => hint.filter(|h| h.scope == Some(txn)),
            None => hint,
        };
        let r = self.insert_table_append_spilled_inner(rowid, prefix, body, hint);
        let r = r.map(|opt| {
            opt.map(|mut h| {
                h.scope = self.pager.armed_writer_scope();
                h
            })
        });
        self.right_walk_for_write = false;
        self.journal_suppress = false;
        match r {
            Ok(v) => {
                if v.is_some() && self.pager.armed_writer_scope().is_some() {
                    // The journal needs the FULL payload — but ONLY when a
                    // concurrent writer's row journal is actually armed
                    // (note_row_write itself no-ops otherwise). The eager
                    // materialization ran on EVERY spilled insert and was a
                    // second full-body copy + alloc per blob row: the S09
                    // shape measured 1.65 GB/s effective — 2x the one-pass
                    // floor (prefix + chain are written straight from the
                    // Value's buffer; this was the only extra pass).
                    let mut full = Vec::with_capacity(prefix.len() + body.len());
                    full.extend_from_slice(prefix);
                    full.extend_from_slice(body);
                    self.pager
                        .note_row_write(root, false, JournalKind::Insert, rowid, &[], &full);
                }
                Ok(v)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn insert_table_append_spilled_inner(
        &mut self,
        rowid: i64,
        prefix: &[u8],
        body: &[u8],
        hint: Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        self.pager.note_write();
        let psz = self.pager.page_size() as usize;
        let total = prefix.len() + body.len();
        debug_assert!(total > self.max_cell_payload());
        let local_len = overflow_local_len_for(total, psz);
        debug_assert!(
            local_len >= prefix.len(),
            "spill local prefix {local_len} must cover the encoded prefix {}",
            prefix.len()
        );
        let body_local = local_len - prefix.len();
        let cell_size = varint::len_of(rowid as u64) + varint::len_of(total as u64) + local_len + 4;

        // 1. Resolve the target leaf (hint or right-most walk) as a
        //    validated pin: `leaf_hint` carries the identity certificate
        //    the write step below re-asserts under its own lock.
        let leaf_hint = match self.spill_target_leaf(hint, rowid, cell_size)? {
            Some(l) => l,
            None => {
                // Stale hint / full leaf / non-append shape: place the
                // PRE-BUILT overflow cell through the normal descent +
                // split machinery — the row never materializes as one
                // contiguous payload. The old caller-side fallback
                // re-encoded the whole row (a FULL extra pass per
                // spilled row); the blob-append shape hits this on
                // nearly every row (one ~4KB local cell fills a leaf),
                // so it was a guaranteed second pass — the S09
                // 64KB-blob shape measured 1.65 GB/s effective vs the
                // 16KB shape's 3.0+ GB/s on the same one-pass path.
                let mut local = Vec::with_capacity(local_len);
                local.extend_from_slice(prefix);
                local.extend_from_slice(&body[..body_local]);
                let cap = self.overflow_page_capacity();
                let overflow = self.build_overflow_chain_rest(&body[body_local..], cap)?;
                let cell = Cell::TableLeafOverflow {
                    rowid,
                    total: total as u64,
                    local,
                    overflow,
                };
                match self.insert_cell_root_aware(cell) {
                    Ok(()) => return Ok(Some(self.right_most_leaf_hinted()?)),
                    Err(e) => {
                        // The cell never landed: reclaim the orphan
                        // chain (mirrors the leaf-bail cleanup below).
                        let _ = self.free_overflow_chain(overflow);
                        return Err(e);
                    }
                }
            }
        };
        let leaf_id = leaf_hint.page;

        // 2. Build the chain (NO leaf lock held while allocating — the
        //    pager's allocation path takes the cache write lock, and an
        //    evictor may hold cache-write while wanting THIS leaf's page
        //    lock; lock order here is allocate-then-leaf).
        let cap = self.overflow_page_capacity();
        let chain = self.build_overflow_chain_rest(&body[body_local..], cap)?;

        // 3. Write the cell into the leaf under one lock. The identity
        //    re-check (`still_pins`) closes the validation window left
        //    open by the chain allocation between step 1 and here: a
        //    page object swap (evict + re-read) or any structural
        //    change declines the write instead of landing blind.
        let wrote: Option<AppendHint> = {
            let page = self.pager.get_page(leaf_id)?;
            let mut borrowed = page.lock();
            let ok = borrowed.page_type()? == PageType::LeafTable
                && leaf_hint.still_pins(&borrowed)
                && borrowed.free_space() >= cell_size as u32 + 2;
            if !ok {
                None
            } else {
                let mut rid_buf = [0u8; 9];
                let n_rid = varint::encode_signed(rowid, &mut rid_buf);
                let mut plen_buf = [0u8; 9];
                let n_plen = varint::encode(total as u64, &mut plen_buf);
                let n = borrowed.n_cells();
                let new_content_start = borrowed
                    .cell_content_start()
                    .saturating_sub(cell_size as u32);
                let off = new_content_start as usize;
                let mut o = off;
                if o + cell_size > borrowed.data.len() {
                    None // corrupt content-start: bail (chain freed below)
                } else {
                    borrowed.data[o..o + n_rid].copy_from_slice(&rid_buf[..n_rid]);
                    o += n_rid;
                    borrowed.data[o..o + n_plen].copy_from_slice(&plen_buf[..n_plen]);
                    o += n_plen;
                    borrowed.data[o..o + prefix.len()].copy_from_slice(prefix);
                    o += prefix.len();
                    borrowed.data[o..o + body_local].copy_from_slice(&body[..body_local]);
                    o += body_local;
                    borrowed.data[o..o + 4].copy_from_slice(&chain.to_be_bytes());
                    debug_assert_eq!(o + 4 - off, cell_size);
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
                    borrowed.touch();
                    Some(AppendHint::fresh(leaf_id, borrowed.serial, borrowed.epoch))
                }
            }
        };
        let new_hint = match wrote {
            Some(h) => h,
            None => {
                self.free_overflow_chain(chain)?;
                return Ok(None);
            }
        };
        self.pager.note_dirty(leaf_id);
        Ok(Some(new_hint))
    }

    /// Resolve + validate the target leaf for a spilled append: the
    /// hinted leaf when it is a table leaf past the last rowid with room
    /// for the (small) local cell, else the tree's right-most leaf under
    /// the same tests. `Ok(None)` = no usable leaf (caller falls back).
    /// Both branches return a FRESH pin of the accepted leaf (its
    /// `(serial, epoch)` read under the validation lock) so the write
    /// step can re-assert identity before touching bytes.
    fn spill_target_leaf(
        &mut self,
        hint: Option<AppendHint>,
        rowid: i64,
        cell_size: usize,
    ) -> Result<Option<AppendHint>> {
        if let Some(h) = hint {
            if let Some(pinned) = self.leaf_accepts_spill(h, rowid, cell_size)? {
                return Ok(Some(pinned));
            }
        }
        // No hint (or hint unusable): walk the right-most chain.
        let leaf = self.right_most_leaf_hinted()?;
        if let Some(pinned) = self.leaf_accepts_spill(leaf, rowid, cell_size)? {
            return Ok(Some(pinned));
        }
        Ok(None)
    }

    /// Cheap pre-validation of a leaf for a spilled append: table-leaf
    /// type, still the pinned object state, rowid strictly past the last
    /// cell's key, and room for the local cell bytes + pointer.
    /// Read-only — no state is touched. Returns the validated pin.
    fn leaf_accepts_spill(
        &self,
        hint: AppendHint,
        rowid: i64,
        cell_size: usize,
    ) -> Result<Option<AppendHint>> {
        let page = match self.pager.get_page(hint.page) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let borrowed = page.lock();
        match borrowed.page_type() {
            Ok(PageType::LeafTable) => {}
            // Zeroed / unreadable pinned page (rollback, recycle): a
            // decline, never an error — the caller falls back.
            Ok(_) | Err(_) => return Ok(None),
        }
        if !hint.still_pins(&borrowed) {
            // Stale pin: the page object was replaced (evict + re-read)
            // or structurally mutated since the hint was taken — the
            // append proof no longer holds; take the general path.
            return Ok(None);
        }
        let n = borrowed.n_cells();
        if n == 0 {
            // EMPTY leaf: no last key bounds the append, and the leaf's
            // separator context may not cover the rowid (the mass-DELETE
            // stale-interior shape). Route through the full descent.
            return Ok(None);
        }
        {
            let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
            if let Some((last_rowid, _)) =
                varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?)
            {
                if rowid <= last_rowid {
                    return Ok(None); // not an append
                }
            }
        }
        if borrowed.free_space() < cell_size as u32 + 2 {
            return Ok(None); // needs a split — caller falls back
        }
        Ok(Some(AppendHint::fresh(
            hint.page,
            borrowed.serial,
            borrowed.epoch,
        )))
    }

    fn insert_table_append_inner(
        &mut self,
        rowid: i64,
        payload: &[u8],
        hint: Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        self.pager.note_write();

        // Fast path: validate the hinted leaf and append directly into
        // it — with the same adaptive suppression as the index side (a
        // random-rowid stream pays no declined attempts).
        if let Some(h) = hint {
            let attempt = h.miss_streak < APPEND_SUPPRESS_AFTER || h.probe_in == 0;
            if !attempt {
                let mut h = h;
                h.probe_in = h.probe_in.saturating_sub(1);
                self.insert_table(rowid, payload)?;
                return Ok(Some(h));
            }
            match self.try_append_into_leaf(h, rowid, payload)? {
                AppendTry::Appended(mut new_hint) => {
                    new_hint.miss_streak = 0;
                    new_hint.probe_in = 0;
                    return Ok(Some(new_hint));
                }
                AppendTry::OutOfOrder => {
                    // Scattered rowid stream: the general insert goes to
                    // this row's true home (left of the pinned leaf — or
                    // INTO it, which bumps its epoch and self-invalidates
                    // the pin on the next attempt). Either way the OLD
                    // pin stays SOUND: identity + state are re-validated
                    // on every use, so the right-edge re-pin walk is
                    // pure waste for never-append streams.
                    self.insert_table(rowid, payload)?;
                    let mut h = h;
                    h.miss_streak = h.miss_streak.saturating_add(1);
                    if h.miss_streak == APPEND_SUPPRESS_AFTER {
                        h.probe_in = APPEND_PROBE_EVERY;
                    }
                    return Ok(Some(h));
                }
                AppendTry::Declined => {
                    // Stale pin / full leaf (a split is incoming): the
                    // general insert places the row, and the right edge
                    // genuinely moved — re-establish the hint. The
                    // stream's shape is unchanged — carry the streak.
                    self.insert_table(rowid, payload)?;
                    let fresh = self.right_most_leaf_hinted()?;
                    return Ok(Some(fresh.carry_stream_state_from(&h)));
                }
            }
        }

        // No hint yet: walk the right-most chain to the right-most leaf
        // through `right_most_leaf_hinted` — every entry into this
        // function runs with `right_walk_for_write` armed, so the walked
        // spine becomes STRUCTURAL DECISION READS.
        //
        // The historical inline walk marked NOTHING, and that omission
        // was the S4 soak's residual 1M-scale corruption: a first-append
        // statement pinned the right edge through the transaction's own
        // (possibly stale) shadow view, a sibling commit's append-SPLIT
        // then demoted the pinned leaf without moving its stamp (the old
        // page is byte-identical in an append-split — only the parent
        // separator is installed), and the transaction's later hinted
        // appends sailed past the sibling's separator with base==now
        // validation passing (leaf unstamped, spine unmarked) —
        // stranding the sibling's committed rows behind a mis-routed
        // interior cell (leaf [.. 1030000025] under a separator of
        // 1000000032). The marks route such commits through the
        // conflict/merge path, whose replay re-derives every placement
        // against the current tree.
        let leaf_pin = self.right_most_leaf_hinted()?;

        // Rightmost leaf: verify the append, check space, and write. The
        // pin was taken fresh under the walk's lock — an OutOfOrder
        // decline below still leaves it valid to hand back (validation,
        // not freshness-by-construction, is the safety contract).
        match self.try_append_into_leaf(leaf_pin, rowid, payload)? {
            AppendTry::Appended(new_hint) => Ok(Some(new_hint)),
            AppendTry::OutOfOrder => {
                self.insert_table(rowid, payload)?;
                Ok(Some(leaf_pin))
            }
            AppendTry::Declined => {
                // Not an append / full leaf — the normal insert path
                // handles splitting + propagation.
                self.insert_table(rowid, payload)?;
                // Re-pin the right-most leaf (one descent; runs only
                // after splits).
                Ok(Some(self.right_most_leaf_hinted()?))
            }
        }
    }

    /// Try to append `rowid -> payload` directly into the leaf pinned by
    /// `hint`. Returns the attempt outcome (see `AppendTry`): the pin is
    /// validated against the page's live object identity + mutation epoch
    /// BEFORE any ordering conclusion is drawn, so a structurally-stale
    /// hint can never append, and an out-of-order entry keeps the (still
    /// valid) pin for the next row without a re-pin walk.
    fn try_append_into_leaf(
        &self,
        hint: AppendHint,
        rowid: i64,
        payload: &[u8],
    ) -> Result<AppendTry> {
        let leaf_id = hint.page;
        // Overflow-spilled payloads go through the full insert path (the
        // cell is local-prefix + chain pointer, built by `make_leaf_cell`).
        if payload.len() > self.max_cell_payload() {
            return Ok(AppendTry::Declined);
        }
        // The pinned page may be UNFETCHABLE by now (rolled back past the
        // snapshot bound in the concurrent regime, freed and truncated,
        // beyond the live page count) — a decline, never an error: the
        // general insert path re-derives everything from the root and
        // surfaces any HONEST storage failure itself.
        let page = match self.pager.get_page(leaf_id) {
            Ok(p) => p,
            Err(_) => return Ok(AppendTry::Declined),
        };
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
        // Stale-pin tolerance: a hint that crossed a ROLLBACK / failed
        // statement / freelist recycling may point at a page that is now
        // ZEROED or otherwise unreadable — that is a DECLINE (the general
        // insert path re-establishes everything), never an error. The
        // index-leaf twin has always handled this shape; the table twin's
        // historical `?` only stayed quiet because per-statement hint
        // resets kept zeroed pins unreachable.
        match borrowed.page_type() {
            Ok(PageType::LeafTable) => {}
            Ok(_) | Err(_) => return Ok(AppendTry::Declined),
        }
        if !hint.still_pins(&borrowed) {
            // The pinned object state diverged (split, delete, restore,
            // re-read) — the append proof is void; take the general path.
            return Ok(AppendTry::Declined);
        }
        let n = borrowed.n_cells();
        if n == 0 {
            // EMPTY leaf: refuse the fast path. An empty leaf has no last
            // key to bound the append, so ANY rowid would be accepted —
            // but the leaf's POSITION in the tree (its separator context)
            // may not cover the rowid. The classic shape: a mass DELETE
            // leaves the interior with stale cells over empty leaves; the
            // rightmost empty leaf then swallows low rowids that belong
            // in a LEFT subtree (their separator-correct home), breaking
            // the tree order — ordered scans misroute, bulk deletes skip
            // ranges. The full descent routes via the separators and is
            // always correct.
            return Ok(AppendTry::Declined);
        }
        {
            let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
            if let Some((last_rowid, _)) =
                varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?)
            {
                if rowid <= last_rowid {
                    // Not an append — but the pin itself is still valid
                    // (this page is untouched); the caller keeps it.
                    return Ok(AppendTry::OutOfOrder);
                }
            }
        }

        // Check if the leaf has space.
        let free = borrowed.free_space();
        if free < cell_size + 2 {
            // Need to split — caller handles splitting + propagation.
            return Ok(AppendTry::Declined);
        }

        // Append: write cell at the new content start, write pointer at end.
        {
            let new_content_start = borrowed.cell_content_start().saturating_sub(cell_size);
            let off = new_content_start as usize;
            if off + cell_size as usize > borrowed.data.len() {
                // Corrupt page header (content start out of range): refuse
                // the append instead of slicing out of bounds.
                return Ok(AppendTry::Declined);
            }
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
            borrowed.data[dst..dst + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n + 1);
            borrowed.touch();
            let new_hint = AppendHint::fresh(leaf_id, borrowed.serial, borrowed.epoch);
            drop(borrowed);
            self.pager.note_dirty(leaf_id);
            Ok(AppendTry::Appended(new_hint))
        }
    }

    /// Cheap upper-bound row estimate for parallel-scan eligibility: the
    /// LAST cell's rowid on the right-most leaf — one O(log N) descent,
    /// no payload decode. For dense-rowid tables (bulk INSERT shapes)
    /// this is the row count; with deletes it over-counts, which at
    /// worst starts a parallel scan whose workers find fewer rows (the
    /// split stays valid — the right-most worker is just lighter).
    /// Returns 0 for an empty (or degenerate) tree.
    pub fn max_rowid_hint(&self) -> Result<i64> {
        let _structural = self.pager.structural_scope();
        let leaf = self.right_most_leaf()?;
        let page = self.pager.get_page(leaf)?;
        let guard = page.lock();
        match guard.page_type()? {
            PageType::LeafTable => {
                let n = guard.n_cells();
                if n == 0 {
                    return Ok(0);
                }
                let ptr = guard.cell_pointer(n - 1) as usize;
                match decode_rowid_only(guard.cell_slice_checked(ptr)?) {
                    Some((rowid, _)) => Ok(rowid),
                    None => Err(Error::corruption("truncated leaf rowid in max_rowid_hint")),
                }
            }
            // Degenerate interior (delete-heavy churn can leave one; see
            // right_most_leaf). No live right-most leaf row to read.
            _ => Ok(0),
        }
    }

    /// The EXACT maximum rowid in the table btree — the last cell of
    /// the right-most leaf, one O(log N) descent, no payload decode.
    /// This is `max_rowid_hint` with the empty/degenerate cases made
    /// explicit so rowid ALLOCATION can use it as the truthful basis
    /// (SQLite's OP_NewRowid: OP_Last + last-cell rowid). `None` means
    /// the right edge cannot answer — empty tree, a degenerate
    /// delete-churned right leaf (earlier leaves may still hold rows),
    /// or a non-table (index-shaped) root — and the caller must fall
    /// back to the full scan for the truthful answer.
    ///
    /// Why this matters: the max-rowid scan that used to run here
    /// walked EVERY page of the table, and inside an open transaction
    /// every page the walk touched was COPIED (the BEGIN CONCURRENT
    /// regime's page shadows; a plain transaction's savepoint undo
    /// capture) — a one-row INSERT into a 10M-row table transiently
    /// materialized ~500 MB of page images per connection (the
    /// mega-scale marathon's M5 soak RSS spike).
    pub fn max_rowid_exact(&self) -> Result<Option<i64>> {
        let _structural = self.pager.structural_scope();
        let leaf = self.right_most_leaf()?;
        let page = self.pager.get_page(leaf)?;
        let guard = page.lock();
        if !matches!(guard.page_type()?, PageType::LeafTable) {
            // Degenerate right edge (delete churn) or an index-shaped
            // root: the scan fallback owns the truthful answer.
            return Ok(None);
        }
        let n = guard.n_cells();
        if n == 0 {
            // Empty root leaf = empty table; empty NON-root right leaf =
            // degenerate (earlier leaves may hold rows). Either way the
            // scan decides (O(1) on the empty root, O(n) only in the
            // degenerate delete-churn case).
            return Ok(None);
        }
        let ptr = guard.cell_pointer(n - 1) as usize;
        match decode_rowid_only(guard.cell_slice_checked(ptr)?) {
            Some((rowid, _)) => Ok(Some(rowid)),
            None => Err(Error::corruption("truncated leaf rowid in max_rowid_exact")),
        }
    }

    /// Descend the right-most-child chain to the right-most leaf and
    /// PIN it: `(page, serial, epoch)` read under the leaf's lock — the
    /// fresh pin an append hint needs after a structural change (split,
    /// stale hint) re-established the right edge.
    fn right_most_leaf_hinted(&self) -> Result<AppendHint> {
        let mut page_id = self.root;
        loop {
            // The right-edge walk routes an append: every page traversed
            // is a decision read (a stale view would append into a leaf
            // that is no longer the tree's right-most — see the field
            // doc on `right_walk_for_write`). Read-only walks
            // (`max_rowid_hint`) leave the flag off and mark nothing.
            if self.right_walk_for_write {
                self.pager.note_decision_read(page_id);
            }
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafTable => {
                    return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                }
                PageType::InteriorTable => {
                    let right = guard.right_most_pointer();
                    if right != 0 {
                        page_id = right;
                        continue;
                    }
                    // right=0: mirror right_most_leaf's degenerate-tree
                    // handling — pin the last cell's de-facto right-most
                    // child (this interior itself when it is empty, page
                    // 0 never: the schema page is not a leaf).
                    let n = guard.n_cells();
                    if n == 0 {
                        return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                    }
                    let last_ptr = guard.cell_pointer(n - 1) as usize;
                    let last_child =
                        decode_table_interior_child(&guard.data[last_ptr..]).unwrap_or(0);
                    if last_child == 0 || last_child == page_id {
                        return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                    }
                    page_id = last_child;
                }
                _ => {
                    return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                }
            }
        }
    }

    /// Descend the right-most-child chain to the right-most leaf.
    fn right_most_leaf(&self) -> Result<PageId> {
        let mut page_id = self.root;
        loop {
            // Decision read when the walk routes a write — see
            // `right_most_leaf_hinted` and the `right_walk_for_write`
            // field doc.
            if self.right_walk_for_write {
                self.pager.note_decision_read(page_id);
            }
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafTable => return Ok(page_id),
                PageType::InteriorTable => {
                    let right = guard.right_most_pointer();
                    if right != 0 {
                        page_id = right;
                        continue;
                    }
                    // right=0: the LAST cell's child is the de-facto
                    // right-most (the state delete/rebalance leaves when
                    // the right-most subtree emptied). Returning THIS
                    // interior would poison the append hint (every hinted
                    // insert re-validates and falls back — correct but
                    // slow); returning literal page 0 hands out the SCHEMA
                    // page as a leaf (corruption).
                    let n = guard.n_cells();
                    if n == 0 {
                        return Ok(page_id);
                    }
                    let last_ptr = guard.cell_pointer(n - 1) as usize;
                    let last_child =
                        decode_table_interior_child(&guard.data[last_ptr..]).unwrap_or(0);
                    if last_child == 0 || last_child == page_id {
                        return Ok(page_id);
                    }
                    page_id = last_child;
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
        hint: Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        self.insert_index_append_placed(key, rowid, hint, &mut None)
    }

    /// [`Self::insert_index_append_hinted`] with a PLACEMENT hint for the
    /// stream's non-append entries: `place` pins the leaf the previous
    /// scattered entry landed in, and an entry that falls strictly inside
    /// that leaf's key range goes straight into it — no root-to-leaf
    /// descent (see [`Self::insert_index_scattered`]). Clustered streams
    /// (`d = i % 1000`, sequential text keys) land next to their
    /// predecessor most of the time.
    pub fn insert_index_append_placed(
        &mut self,
        key: &[u8],
        rowid: i64,
        hint: Option<AppendHint>,
        place: &mut Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        let _structural = self.pager.structural_scope();
        let root = self.root;
        self.journal_suppress = true;
        // The right-edge walks this entry takes (first append, Declined
        // re-pins) route an append — arm the decision-read marking, the
        // exact discipline the table entries apply (see
        // `right_walk_for_write`). The index append machinery historically
        // marked NOTHING: a stale right-edge pin survived a sibling's
        // index append-split (old leaf unstamped — byte-identical) and
        // the transaction's entries installed past the sibling's
        // separator (the S4 soak's original "index entries out of order
        // or duplicated" signature).
        self.right_walk_for_write = true;
        // Cross-scope hint guard (the table twin's discipline — see
        // `AppendHint::scope`): the scratch-carried hint may pin a leaf
        // another transaction established (its pin validates that
        // transaction's SHADOW object, not this one's). The historical
        // index entry had NO guard: a foreign hint flowed straight into
        // `try_append` (the pin mismatch declined it, but the fallback
        // walk then re-pinned through this transaction's stale view).
        let hint = match self.pager.armed_writer_scope() {
            Some(txn) => hint.filter(|h| h.scope == Some(txn)),
            None => hint,
        };
        let r = self.insert_index_append_hinted_inner(key, rowid, hint, place);
        self.right_walk_for_write = false;
        // Stamp the outgoing hint with the CURRENT scope so its next use
        // (this transaction only) passes the guard above — the table
        // twin's rule; an unstamped hint would be dropped forever after.
        let r = r.map(|opt| {
            opt.map(|mut h| {
                h.scope = self.pager.armed_writer_scope();
                h
            })
        });
        self.journal_suppress = false;
        match r {
            Ok(v) => {
                if !self.journal_suppress {
                    self.pager
                        .note_row_write(root, true, JournalKind::Insert, rowid, key, &[]);
                }
                Ok(v)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn insert_index_append_hinted_inner(
        &mut self,
        key: &[u8],
        rowid: i64,
        hint: Option<AppendHint>,
        place: &mut Option<AppendHint>,
    ) -> Result<Option<AppendHint>> {
        self.pager.note_write();

        // Fast path: validate the hinted leaf and append directly into
        // it — unless the stream is KNOWN scattered (miss_streak at or
        // past the suppression threshold): then the attempt would just
        // decline again (fetch + lock + last-cell decode for nothing),
        // so take the general insert straight away and count down to
        // the next probe.
        if let Some(h) = hint {
            let attempt = h.miss_streak < APPEND_SUPPRESS_AFTER || h.probe_in == 0;
            if !attempt {
                let mut h = h;
                h.probe_in = h.probe_in.saturating_sub(1);
                self.insert_index_scattered(key, rowid, place)?;
                return Ok(Some(h));
            }
            match self.try_append_into_index_leaf(h, key, rowid)? {
                AppendTry::Appended(mut new_hint) => {
                    new_hint.miss_streak = 0;
                    new_hint.probe_in = 0;
                    return Ok(Some(new_hint));
                }
                AppendTry::OutOfOrder => {
                    // Scattered key stream — the shape S12's secondary
                    // indexes hit permanently (`a = (i*7919)%100_003`,
                    // `d = i%1000`): the entry declines against the
                    // pinned leaf's OWN last cell, the general insert
                    // lands it at its true home, and the untouched pin
                    // stays valid for the next row. The historical
                    // contract — re-walk the right edge after EVERY
                    // general insert — turned each scattered index entry
                    // into a double descent (insert + re-pin walk) and
                    // was the dominant per-row cost of multi-index load.
                    self.insert_index_scattered(key, rowid, place)?;
                    let mut h = h;
                    h.miss_streak = h.miss_streak.saturating_add(1);
                    if h.miss_streak == APPEND_SUPPRESS_AFTER {
                        h.probe_in = APPEND_PROBE_EVERY;
                    }
                    return Ok(Some(h));
                }
                AppendTry::Declined => {
                    // Stale pin / full leaf (split incoming): general
                    // insert, then re-establish the right-edge pin. The
                    // stream's shape is unchanged — carry the streak.
                    self.insert_index(key, rowid)?;
                    let fresh = self.right_most_index_leaf_hinted()?;
                    return Ok(Some(fresh.carry_stream_state_from(&h)));
                }
            }
        }

        // No hint yet: walk the right-most-child chain to the right-most
        // index leaf through `right_most_index_leaf_hinted` — the marking
        // twin of the table walk (this entry arms `right_walk_for_write`;
        // see the table-side comment in `insert_table_append_inner` for
        // the corruption class the marks close).
        let leaf_pin = self.right_most_index_leaf_hinted()?;

        match self.try_append_into_index_leaf(leaf_pin, key, rowid)? {
            AppendTry::Appended(new_hint) => Ok(Some(new_hint)),
            // The fresh pin is still valid after an out-of-order general
            // insert (validation, not freshness, is the safety contract).
            AppendTry::OutOfOrder => {
                self.insert_index_scattered(key, rowid, place)?;
                Ok(Some(leaf_pin))
            }
            // Not an append / full leaf — the normal insert path handles
            // splitting + propagation.
            AppendTry::Declined => {
                self.insert_index(key, rowid)?;
                Ok(Some(self.right_most_index_leaf_hinted()?))
            }
        }
    }

    /// A non-append index entry: into the leaf the PREVIOUS scattered
    /// entry landed in when it falls strictly inside that leaf's key
    /// range (see [`Self::try_insert_into_placed_leaf`]); otherwise the
    /// general insert, re-pinning `place` to wherever the entry landed.
    /// Off in the concurrent regime: an armed writer's placements route
    /// through the descent's decision-read marking.
    fn insert_index_scattered(
        &mut self,
        key: &[u8],
        rowid: i64,
        place: &mut Option<AppendHint>,
    ) -> Result<()> {
        if self.pager.concurrent_scope_armed() {
            *place = None;
            return self.insert_index(key, rowid);
        }
        // Adaptive suppression (the append hint's discipline): a stream
        // whose entries keep missing the previous leaf (`a = (i*7919)%N`)
        // stops paying the declined fetch + bound decodes, probing again
        // every APPEND_PROBE_EVERY entries.
        let mut streak = (0u16, 0u16);
        if let Some(h) = place.take() {
            if h.miss_streak < APPEND_SUPPRESS_AFTER || h.probe_in == 0 {
                if let Some(pin) = self.try_insert_into_placed_leaf(h, key, rowid)? {
                    *place = Some(pin);
                    return Ok(());
                }
                let misses = h.miss_streak.saturating_add(1);
                let probe = if misses >= APPEND_SUPPRESS_AFTER {
                    APPEND_PROBE_EVERY
                } else {
                    0
                };
                streak = (misses, probe);
            } else {
                streak = (h.miss_streak, h.probe_in.saturating_sub(1));
            }
        }
        self.last_index_leaf = None;
        self.insert_index(key, rowid)?;
        *place = self.last_index_leaf.take().map(|mut pin| {
            (pin.miss_streak, pin.probe_in) = streak;
            pin
        });
        Ok(())
    }

    /// Insert `(key, rowid)` into the pinned index leaf when that is
    /// provably its home: the pin is current (no write touched the page
    /// since), the entry sorts STRICTLY between the leaf's first and last
    /// entries (a B-tree leaf owns every key between its own bounds), the
    /// key needs no overflow chain and the cell fits without a split.
    /// `None` = declined, nothing written.
    fn try_insert_into_placed_leaf(
        &mut self,
        hint: AppendHint,
        key: &[u8],
        rowid: i64,
    ) -> Result<Option<AppendHint>> {
        let psz = self.pager.page_size() as usize;
        if key.len() > index_max_local(psz) {
            return Ok(None);
        }
        let leaf_id = hint.page;
        let Ok(page) = self.pager.get_page(leaf_id) else {
            return Ok(None);
        };
        let cell = Cell::IndexLeaf {
            key: key.to_vec(),
            rowid,
        };
        let need = cell.encoded_size() as u32 + 2;
        let pos = {
            let b = page.lock();
            if !matches!(b.page_type(), Ok(PageType::LeafIndex)) || !hint.still_pins(&b) {
                return Ok(None);
            }
            let n = b.n_cells();
            if n < 2 || b.free_space() < need {
                return Ok(None);
            }
            let page_size = b.page_size();
            let view = |i: u16| -> Result<IndexCellView<'_>> {
                let ptr = b.cell_pointer(i) as usize;
                decode_index_cell(b.cell_slice_checked(ptr)?, false, page_size)
                    .ok_or_else(|| Error::corruption("truncated index cell in placed insert"))
            };
            let (first, last) = (view(0)?, view(n - 1)?);
            if first.overflow != 0
                || last.overflow != 0
                || (first.key, first.rowid) >= (key, rowid)
                || (key, rowid) >= (last.key, last.rowid)
            {
                return Ok(None);
            }
            // Position in (0, n - 1]: cells [0, lo) sort below the entry.
            let (mut lo, mut hi) = (1u16, n - 1);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let ptr = b.cell_pointer(mid) as usize;
                match index_cell_lt(b.cell_slice_checked(ptr)?, false, page_size, key, rowid) {
                    Some(Some(true)) => lo = mid + 1,
                    Some(Some(false)) => hi = mid,
                    // An overflow cell inside the range: the general insert.
                    Some(None) => return Ok(None),
                    None => return Err(Error::corruption("truncated index cell in placed insert")),
                }
            }
            lo
        };
        self.insert_cell_into_ref_at(leaf_id, &page, &cell, Some(pos))?;
        let b = page.lock();
        Ok(Some(AppendHint::fresh(leaf_id, b.serial, b.epoch)))
    }

    /// Descend the right-most-child chain to the right-most INDEX leaf
    /// and PIN it (`(page, serial, epoch)` under the leaf's lock) — the
    /// fresh pin an index append hint needs after a structural change.
    fn right_most_index_leaf_hinted(&self) -> Result<AppendHint> {
        let mut page_id = self.root;
        loop {
            // The right-edge walk routes an append: every page traversed
            // is a decision read, mirroring the table twin (a stale view
            // would pin a leaf a sibling's split already demoted — see
            // `right_walk_for_write` and the S4 residual-corruption notes
            // in `insert_table_append_inner`).
            if self.right_walk_for_write {
                self.pager.note_decision_read(page_id);
            }
            let page = self.pager.get_page(page_id)?;
            let guard = page.lock();
            match guard.page_type()? {
                PageType::LeafIndex => {
                    return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                }
                PageType::InteriorIndex => {
                    let right = guard.right_most_pointer();
                    if right == 0 {
                        return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                    }
                    page_id = right;
                }
                _ => {
                    return Ok(AppendHint::fresh(page_id, guard.serial, guard.epoch));
                }
            }
        }
    }

    /// Try to append `(key, rowid)` directly into the index leaf pinned
    /// by `hint`. Returns the attempt outcome (see `AppendTry`): the pin
    /// is validated against the page's live object identity + mutation
    /// epoch before any ordering conclusion is drawn, so a stale hint can
    /// never append, and an out-of-order entry keeps the still-valid pin
    /// for the next row without a re-pin walk.
    fn try_append_into_index_leaf(
        &self,
        hint: AppendHint,
        key: &[u8],
        rowid: i64,
    ) -> Result<AppendTry> {
        let leaf_id = hint.page;
        // Unfetchable pinned page (see the table twin): decline.
        let page = match self.pager.get_page(leaf_id) {
            Ok(p) => p,
            Err(_) => return Ok(AppendTry::Declined),
        };
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
        match borrowed.page_type() {
            Ok(PageType::LeafIndex) => {}
            // Stale hint: wrong page kind, or the page was freed /
            // zeroed by a rollback since the hint was pinned (an error
            // here used to surface as "invalid page type byte: 0x0"
            // instead of falling back to the full insert path).
            Ok(_) | Err(_) => return Ok(AppendTry::Declined),
        }
        if !hint.still_pins(&borrowed) {
            // The pinned object state diverged (split, cell insert or
            // delete, rollback restore, page re-read) — the append proof
            // is void; take the general path.
            return Ok(AppendTry::Declined);
        }
        let n = borrowed.n_cells();
        if n == 0 {
            // EMPTY leaf: refuse the fast path (no last key bounds the
            // append; the leaf's separator context may not cover the
            // entry — see the table-leaf mirror in try_append_into_leaf).
            return Ok(AppendTry::Declined);
        }
        {
            let cell_ptr = borrowed.cell_pointer(n - 1) as usize;
            let psz = borrowed.page_size();
            match decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz) {
                Some(v) if v.overflow == 0 => {
                    // Append requires (key, rowid) strictly after the last
                    // entry — index pages are sorted by (key, rowid).
                    if !(v.key < key || (v.key == key && v.rowid < rowid)) {
                        // Out-of-order against this leaf's own last cell:
                        // the leaf (and the pin) is untouched — the caller
                        // keeps the pin and skips the re-pin walk.
                        return Ok(AppendTry::OutOfOrder);
                    }
                }
                // Overflow last key: the local prefix alone is an
                // inconclusive bound (the full key reassembly costs a
                // chain walk) — DECLINE the fast path and take the
                // general insert. The previous code fell through to the
                // append here with NO ordering check at all: an
                // out-of-key-order entry whose rowid merely exceeded the
                // spilled cell's (the limit-stress spilled-index shape —
                // big(9000)-style keys then 'pad') was appended AFTER
                // the spilled key, leaving the leaf unsorted (binary
                // searches then misroute: deletes missed their entries
                // and left stale index rows).
                _ => return Ok(AppendTry::Declined),
            }
        }

        // Check if the leaf has space.
        let free = borrowed.free_space();
        if free < cell_size + 2 {
            return Ok(AppendTry::Declined); // need a split — caller handles it
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
            borrowed.data[dst..dst + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n + 1);
            borrowed.touch();
            let new_hint = AppendHint::fresh(leaf_id, borrowed.serial, borrowed.epoch);
            drop(borrowed);
            self.pager.note_dirty(leaf_id);
            Ok(AppendTry::Appended(new_hint))
        }
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
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.update_table_bulk_inner(updates, deferred);
        match r {
            Ok(()) => {
                if !self.journal_suppress {
                    // Deferred (size-changed) rows are re-applied by the
                    // caller as delete+insert — journaled there. Their
                    // Replace entries here are net-idempotent with that
                    // sequence on replay, so journaling all updates keeps
                    // validation sound without splitting the batch.
                    for (rid, payload) in updates {
                        self.pager.note_row_write(
                            root,
                            false,
                            JournalKind::Replace,
                            *rid,
                            &[],
                            payload,
                        );
                        self.pager.note_row_outcome(true);
                    }
                }
                Ok(())
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn update_table_bulk_inner(
        &mut self,
        updates: &[(i64, &[u8])],
        deferred: &mut Vec<usize>,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        // Same-size in-place patches never change leaf bounds, so advisory
        // leaf hints stay valid (no write-epoch bump). Size-changed rows
        // are deferred to delete+insert, which bump the epoch themselves.
        self.pager.note_write_in_place();
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
                        match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
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
                        return Err(Error::corruption(
                            "cell pointer out of range in bulk update",
                        ));
                    }
                    let (cell_rowid, n_rid) =
                        varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?)
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
                                .ok_or_else(|| {
                                    Error::corruption("truncated payload len in bulk update")
                                })?;
                            let payload_offset = plen_pos + n_plen;
                            if plen as usize == new_payload.len()
                                && payload_offset + new_payload.len() <= psz
                                && overflow_local_len_for(plen as usize, psz) == plen as usize
                            {
                                borrowed.data[payload_offset..payload_offset + new_payload.len()]
                                    .copy_from_slice(new_payload);
                                borrowed.touch();
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
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.update_table_inner(rowid, new_payload);
        match r {
            Ok(did) => {
                if !self.journal_suppress {
                    self.pager.note_row_write(
                        root,
                        false,
                        JournalKind::Replace,
                        rowid,
                        &[],
                        new_payload,
                    );
                    self.pager.note_row_outcome(did);
                }
                Ok(did)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn update_table_inner(&mut self, rowid: i64, new_payload: &[u8]) -> Result<bool> {
        // Notify the pager that a write is about to happen (in-place UPDATE).
        // Same-size payload patches and in-slot shrinks never move an
        // offset — the non-invalidating variant keeps advisory leaf hints
        // alive. The GROW branch of the in-leaf replace bumps the epoch
        // itself; callers that fall back to delete+insert bump it there.
        self.pager.note_write_in_place();
        // Hint probe: try patching directly in the remembered leaf (the
        // epoch check guarantees the hint bounds are exact, so a completed
        // binary search that misses means "not in this tree"). Same-size
        // payloads byte-patch; size changes take the IN-LEAF REPLACE
        // (shrink into the old cell's slot, grow at the content start when
        // the leaf has room); a grow that cannot fit returns false and the
        // caller falls back to delete+insert.
        {
            let epoch = self.pager.write_epoch();
            if let Some((page_ref, _bias)) = table_hint_page(self.root, rowid, epoch) {
                let geo = {
                    let borrowed = page_ref.lock();
                    if borrowed.page_type()? != PageType::LeafTable {
                        None
                    } else {
                        let n = borrowed.n_cells() as usize;
                        let mut lo = 0usize;
                        let mut hi = n;
                        // (pos, cell_ptr, n_rid, n_plen, payload_off, plen)
                        let mut hit: Option<(usize, usize, usize, usize, usize, usize)> = None;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                            let Some((cell_rowid, n_rid)) =
                                decode_rowid_only(borrowed.cell_slice_checked(cell_ptr)?)
                            else {
                                break;
                            };
                            match cell_rowid.cmp(&rowid) {
                                std::cmp::Ordering::Equal => {
                                    let plen_pos = cell_ptr + n_rid;
                                    let Some((plen, n_plen)) =
                                        varint::decode(&borrowed.data[plen_pos..])
                                    else {
                                        break;
                                    };
                                    hit = Some((
                                        mid,
                                        cell_ptr,
                                        n_rid,
                                        n_plen,
                                        plen_pos + n_plen,
                                        plen as usize,
                                    ));
                                    break;
                                }
                                std::cmp::Ordering::Less => lo = mid + 1,
                                std::cmp::Ordering::Greater => hi = mid,
                            }
                        }
                        let psz = borrowed.data.len();
                        hit.map(|(pos, cp, n_rid, n_plen, off, plen)| {
                            (pos, cp, n_rid, n_rid + n_plen + plen, off, plen, psz)
                        })
                    }
                };
                let page_id_now = page_ref.lock().id;
                if let Some((pos, cell_ptr, n_rid, old_cell_size, payload_off, old_plen, psz)) = geo
                {
                    // Spilled rows never take the in-place patch path
                    // (their bytes span a chain, not the leaf).
                    if overflow_local_len_for(old_plen, psz) != old_plen {
                        return Ok(false);
                    }
                    if old_plen == new_payload.len() {
                        let mut borrowed = page_ref.lock();
                        if payload_off + new_payload.len() <= borrowed.data.len() {
                            borrowed.data[payload_off..payload_off + new_payload.len()]
                                .copy_from_slice(new_payload);
                            borrowed.touch();
                            drop(borrowed);
                            self.pager.note_dirty(page_id_now);
                            return Ok(true);
                        }
                        return Ok(false);
                    }
                    // Size change — in-leaf replace (shrink / grow).
                    return self.apply_leaf_replace(
                        &page_ref,
                        page_id_now,
                        rowid,
                        pos as u16,
                        cell_ptr,
                        n_rid,
                        old_cell_size,
                        old_plen,
                        new_payload,
                        psz,
                    );
                }
                // Not in the hinted leaf and bounds are epoch-exact:
                // not in the tree at all.
                return Ok(false);
            }
        }
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
                    let geo = {
                        let borrowed = page.lock();
                        let psz = borrowed.data.len();
                        let n = borrowed.n_cells() as usize;
                        let mut lo = 0usize;
                        let mut hi = n;
                        // (pos, cell_ptr, n_rid, n_plen, payload_off, plen)
                        let mut found: Option<(usize, usize, usize, usize, usize, usize)> = None;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid as u16) as usize;
                            let (cell_rowid, n_rid) =
                                decode_rowid_only(borrowed.cell_slice_checked(cell_ptr)?)
                                    .ok_or_else(|| {
                                        Error::corruption("truncated leaf rowid in update")
                                    })?;
                            match cell_rowid.cmp(&rowid) {
                                std::cmp::Ordering::Equal => {
                                    let plen_pos = cell_ptr + n_rid;
                                    let (plen, n_plen) = varint::decode(&borrowed.data[plen_pos..])
                                        .ok_or_else(|| {
                                            Error::corruption("truncated payload length in update")
                                        })?;
                                    found = Some((
                                        mid,
                                        cell_ptr,
                                        n_rid,
                                        n_plen,
                                        plen_pos + n_plen,
                                        plen as usize,
                                    ));
                                    break;
                                }
                                std::cmp::Ordering::Less => lo = mid + 1,
                                std::cmp::Ordering::Greater => hi = mid,
                            }
                        }
                        found.map(|(pos, cp, n_rid, n_plen, off, plen)| {
                            (pos, cp, n_rid, n_rid + n_plen + plen, off, plen, psz)
                        })
                    };
                    let Some((pos, cell_ptr, n_rid, old_cell_size, payload_off, old_plen, psz)) =
                        geo
                    else {
                        return Ok(false); // rowid not present
                    };
                    // Spilled rows never take the in-place patch path.
                    if overflow_local_len_for(old_plen, psz) != old_plen {
                        return Ok(false);
                    }
                    if old_plen == new_payload.len() {
                        // Overwrite the payload bytes — single mutable
                        // borrow, no immutable borrows outstanding.
                        {
                            let mut borrowed = page.lock();
                            borrowed.data[payload_off..payload_off + new_payload.len()]
                                .copy_from_slice(new_payload);
                            borrowed.touch();
                        }
                        self.pager.note_dirty(page_id);
                        return Ok(true);
                    }
                    // Size change — in-leaf replace (shrink / grow).
                    return self.apply_leaf_replace(
                        &page,
                        page_id,
                        rowid,
                        pos as u16,
                        cell_ptr,
                        n_rid,
                        old_cell_size,
                        old_plen,
                        new_payload,
                        psz,
                    );
                }
                PageType::InteriorTable => {
                    let borrowed = page.lock();
                    let n = borrowed.n_cells();
                    let mut next = borrowed.right_most_pointer();
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let cell = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
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

    /// Shape-changing in-leaf UPDATE, applied where the cell was already
    /// FOUND by `update_table`'s hint probe or descent: replace the cell
    /// for `rowid` with a new payload of a DIFFERENT size, entirely
    /// within its leaf page — no delete/reinsert descent, no tree
    /// rebalance, no split when the leaf has room. This is the b-tree
    /// twin of SQLite's own in-cursor UPDATE (delete the old cell, insert
    /// the new one into the SAME page when it fits); it closes the OLTP
    /// gap where a payload that grows or shrinks by a few bytes (`score`
    /// flipping between integral-int and X.5-real encodings) pushed the
    /// row into the full delete+insert path — a second root descent,
    /// cell shuffling, and leaf-split churn.
    ///
    /// Returns `Ok(true)` when the replacement was applied in-page.
    /// `Ok(false)` = the caller must use the delete+insert fallback:
    /// the new payload would spill to an overflow chain, or the leaf
    /// cannot fit the grown cell (the old cell's BYTES become dead space
    /// in the content area — reclaimed by the next split/rebuild
    /// compaction, exactly like `delete_table`'s pointer drop).
    ///
    /// Hint/epoch contract: the SHRINK branch writes inside the old
    /// cell's slot, so no offset moves and advisory leaf hints stay
    /// valid (`note_write_in_place`); the GROW branch moves the content
    /// start and bumps the write version (all advisory hints for the
    /// tree die) through `note_write()`.
    #[allow(clippy::too_many_arguments)]
    fn apply_leaf_replace(
        &self,
        page: &crate::storage::pager::PageRef,
        page_id: PageId,
        rowid: i64,
        pos: u16,
        cell_ptr: usize,
        n_rid: usize,
        old_cell_size: usize,
        _old_plen: usize,
        new_payload: &[u8],
        psz: usize,
    ) -> Result<bool> {
        // A payload that would spill needs the overflow path's chain
        // bookkeeping — bail to delete+insert (which builds/frees chains).
        if overflow_local_len_for(new_payload.len(), psz) != new_payload.len() {
            return Ok(false);
        }
        let new_cell_size = n_rid + varint::len_of(new_payload.len() as u64) + new_payload.len();
        // ---- SHRINK / EQUAL-SLOT branch: the new cell fits inside the
        // old cell's slot. Write it there — the pointer, the cell count,
        // the content start, and every other cell's offset stay EXACTLY
        // as they were, so advisory leaf hints remain valid. The tail
        // bytes of the old slot become dead space, reclaimed by the next
        // split/rebuild compaction — the same lifecycle delete_table's
        // dead cells follow. This is the workhorse for the
        // integral-<->-fractional REAL flips (codec v2 stores integral
        // reals as 2-5 byte zigzags and fractional ones as 9-byte
        // doubles) on APPEND-PACKED leaves, where the grow branch's
        // free-space check would fail and the row would pay
        // delete+insert.
        if new_cell_size <= old_cell_size {
            let mut borrowed = page.lock();
            let head = cell_ptr + n_rid; // just past the (unchanged) rowid varint
            let mut vb = [0u8; 10];
            let n_plen = varint::encode(new_payload.len() as u64, &mut vb);
            if head + n_plen + new_payload.len() > borrowed.data.len() {
                return Err(Error::corruption(format!(
                    "cell slot out of range in replace (page {})",
                    page_id
                )));
            }
            borrowed.data[head..head + n_plen].copy_from_slice(&vb[..n_plen]);
            borrowed.data[head + n_plen..head + n_plen + new_payload.len()]
                .copy_from_slice(new_payload);
            // The pointer at `pos` already == cell_ptr.
            borrowed.touch();
            drop(borrowed);
            self.pager.note_write_in_place();
            self.pager.note_dirty(page_id);
            return Ok(true);
        }
        // ---- GROW branch: offsets move, so advisory hints die
        // (note_write) and the insert needs the full new cell size + its
        // 2-byte pointer out of CURRENT free space (the old cell's bytes
        // become dead, not free).
        let mut cell_buf = Vec::with_capacity(new_cell_size);
        let mut vb = [0u8; 10];
        let k = varint::encode_signed(rowid, &mut vb);
        cell_buf.extend_from_slice(&vb[..k]);
        let p = varint::encode(new_payload.len() as u64, &mut vb);
        cell_buf.extend_from_slice(&vb[..p]);
        cell_buf.extend_from_slice(new_payload);
        debug_assert_eq!(cell_buf.len(), new_cell_size);
        self.pager.note_write();
        {
            let borrowed = page.lock();
            let free = borrowed.free_space();
            if free < new_cell_size as u32 + 2 {
                return Ok(false); // caller: delete + insert (may split)
            }
        }
        // Apply: drop the old pointer (cell bytes become dead space),
        // then insert the new cell at the content start with the pointer
        // restored at the same slot.
        let header_offset = if page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
        {
            let mut borrowed = page.lock();
            let n = borrowed.n_cells();
            let pos_usize = pos as usize;
            // Remove old pointer.
            borrowed.data.copy_within(
                ptr_array_start + (pos_usize + 1) * 2..ptr_array_start + n as usize * 2,
                ptr_array_start + pos_usize * 2,
            );
            // Insert new cell bytes at the content start.
            let new_content_start = borrowed
                .cell_content_start()
                .saturating_sub(new_cell_size as u32);
            let off = new_content_start as usize;
            if off + new_cell_size > borrowed.data.len() {
                return Err(Error::corruption(format!(
                    "cell content area out of range in replace (page {})",
                    page_id
                )));
            }
            borrowed.data[off..off + new_cell_size].copy_from_slice(&cell_buf);
            borrowed.set_cell_content_start(new_content_start);
            // Re-open the pointer slot at `pos` (n is now the
            // post-removal count; the memmove above closed it).
            let n_now = n - 1;
            borrowed.data.copy_within(
                ptr_array_start + pos_usize * 2..ptr_array_start + n_now as usize * 2,
                ptr_array_start + (pos_usize + 1) * 2,
            );
            let dst = ptr_array_start + pos_usize * 2;
            borrowed.data[dst..dst + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());
            borrowed.set_n_cells(n);
            borrowed.touch();
        }
        self.pager.note_dirty(page_id);
        Ok(true)
    }

    /// Insert a cell into a page (and propagate splits if needed).
    fn insert_into_page(&mut self, page_id: PageId, cell: Cell) -> Result<InsertResult> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;

        if pt.is_leaf() {
            // Leaf: insert directly.
            // insert_table_absent: the probe for an existing rowid shares
            // this descent; its search position places the cell.
            let mut known_pos: Option<u16> = None;
            if self.reject_dup_rowid && pt == PageType::LeafTable {
                match table_leaf_search(&page.lock(), cell.key())? {
                    Ok(_) => return Ok(InsertResult::Duplicate),
                    Err(pos) => known_pos = Some(pos),
                }
            }
            let cell_size = cell.encoded_size();
            let free = page.lock().free_space();
            // Need space for the cell + a 2-byte pointer. When the GAP
            // is too small but the page carries dead cell bytes (delete
            // churn — see compact_leaf_if_fragmented), reclaim them in
            // place instead of splitting: churned workloads re-fill
            // their old pages rather than growing the file.
            if free < cell_size as u32 + 2 {
                if self.compact_leaf_if_fragmented(page_id, cell_size + 2)? {
                    self.insert_cell_into_ref(page_id, &page, &cell)?;
                    return Ok(InsertResult::Done);
                }
                drop(page);
                return self.split_leaf(page_id, cell);
            }
            self.insert_cell_into_ref_at(page_id, &page, &cell, known_pos)?;
            Ok(InsertResult::Done)
        } else {
            // Interior: find the child to descend into.
            // Convention: cell (left_child, sep) means left_child contains
            // entries <= sep. Descent: take the FIRST cell whose separator is
            // >= the target; go to its left_child. If none, go to right_most.
            // Table pages compare by i64 key; index pages by (key, rowid).
            //
            // This interior's CELLS route the write — a decision read.
            // In the concurrent regime the shadow may be stale (a sibling
            // split moved a boundary or the right-most edge after our
            // fetch); routing a write from stale separators strands
            // committed rows behind mis-routed interior cells. The mark
            // routes this page through commit's validate-or-merge gate.
            self.pager.note_decision_read(page_id);
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
                    // Full target key when the inserted cell overflows (the
                    // variant's index_key() is only the local prefix).
                    let nk_owned: Vec<u8>;
                    let nk: &[u8] = if cell.index_overflow() != 0 {
                        nk_owned = self.cell_index_key(&cell)?;
                        &nk_owned
                    } else {
                        cell.index_key()
                    };
                    let (_, child) = self.find_index_child(data, n, cp, right_most, nk, cell.key());
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
                        if right_most != 0 {
                            right_most
                        } else if n > 0 {
                            // right=0 ("no right-most child" — a state the
                            // delete/rebalance paths leave behind when the
                            // right-most subtree emptied): the LAST cell's
                            // child is the de-facto right edge. Descending
                            // into literal page 0 would insert rows into the
                            // SCHEMA page (silent corruption: schema rows
                            // + table rows interleaved, unreachable table
                            // rows, and a phantom (0, key) parent cell once
                            // page 0 splits).
                            let ptr = cp(n - 1) as usize;
                            decode_table_interior_child(&data[ptr..]).unwrap_or(0)
                        } else {
                            0
                        }
                    } else {
                        let ptr = cp(lo) as usize;
                        decode_table_interior_child(&data[ptr..]).unwrap_or(right_most)
                    }
                }
            };
            drop(page);
            match self.insert_into_page(child_id, cell)? {
                InsertResult::Done => Ok(InsertResult::Done),
                InsertResult::Duplicate => Ok(InsertResult::Duplicate),
                InsertResult::Split {
                    new_page,
                    split_key,
                    split_key_bytes,
                    extra_splits,
                } => {
                    // The child split. We must replace the cell that pointed
                    // to `child_id` with TWO cells (a 3+-way split: one cell
                    // per boundary, the LAST new page inheriting the old
                    // bound):
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
                            (
                                b.n_cells(),
                                b.page_type()?,
                                b.free_space(),
                                b.right_most_pointer(),
                            )
                        };
                        let is_idx_now = pt_now.is_index();
                        // Find the cell pointing at child_id (view-decode, no
                        // allocation), or note the right-most case.
                        let mut found: Option<(usize, usize)> = None; // (index, byte offset)
                        let mut is_right_most = right_now == child_id && child_id != 0;
                        if !is_right_most {
                            let b = page.lock();
                            let psz = b.page_size();
                            for i in 0..n_now {
                                let ptr = b.cell_pointer(i) as usize;
                                let child = if is_idx_now {
                                    decode_index_cell(&b.data[ptr..], true, psz)
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
                            } else if right_now == 0
                                && found.is_some_and(|(i, _)| i + 1 == n_now as usize)
                            {
                                // right=0 and the child is the LAST cell's
                                // child — the de-facto right-most split. The
                                // two-cell fast splice would emit
                                // (split_key-1, old_key) — a NON-MONOTONIC
                                // pair (old_key < split_key) that misroutes
                                // every later descent. Decline to the slow
                                // path, which rewrites the last cell and
                                // promotes new_page to the right-most slot.
                                found = None;
                                is_right_most = false;
                            }
                        }

                        // The in-place splices below handle ONE new page;
                        // a 3+-way split goes straight to the rebuild path.
                        if is_right_most && extra_splits.is_empty() {
                            if crate::executor::dbg_index_trace() {
                                eprintln!(
                                    "[parent:rm] parent={} child={} new_page={} sep_rowid={}",
                                    page_id, child_id, new_page, split_key
                                );
                            }
                            // Append cell1 = (child_id, separator) and make
                            // new_page the new right-most child.
                            // NOTE: `is_right_most` is only true when
                            // right_now != 0 — the right=0 shape (child is
                            // the LAST cell's child, the de-facto right
                            // edge) must NOT append (the old cell still
                            // references child_id): it rewrites the last
                            // cell's separator instead — handled by the
                            // slow path (fast path declines below).
                            let cell1 = if is_idx_now {
                                self.index_interior_separator(
                                    child_id,
                                    split_key_bytes.as_deref().unwrap_or_default(),
                                    split_key,
                                )?
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
                                    let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
                                    let dst = ptr_array_start + n_now as usize * 2;
                                    b.data[dst..dst + 2]
                                        .copy_from_slice(&(c1_start as u16).to_be_bytes());
                                    b.set_n_cells(n_now + 1);
                                    b.set_right_most_pointer(new_page);
                                    b.touch();
                                }
                                self.pager.note_dirty(page_id);
                                return Ok(InsertResult::Done);
                            }
                            // No room: the rebuild path builds its own
                            // separators — release this one's fresh chain
                            // (an oversized key copy) or it leaks.
                            let leaked = cell1.index_overflow();
                            if leaked != 0 {
                                self.free_overflow_chain(leaked)?;
                            }
                        } else if let Some((idx, old_off)) =
                            found.filter(|_| extra_splits.is_empty())
                        {
                            // Build cell1/cell2 and splice them in place of
                            // the old cell at `idx`.
                            let (cell1, cell2, old_chain) = if is_idx_now {
                                let psz = page.lock().page_size();
                                let (old_key, old_rowid, old_chain) = match decode_index_cell(
                                    &page.lock().data[old_off..],
                                    true,
                                    psz,
                                ) {
                                    Some(v) => {
                                        // The old separator's key is COPIED
                                        // into cell2 — oversized keys need
                                        // the FULL bytes (prefix would
                                        // corrupt the tree order). The old
                                        // cell's chain dies with it (the
                                        // copy builds its own).
                                        let full = self.index_view_key(&v)?;
                                        (full, v.rowid, v.overflow)
                                    }
                                    None => (Vec::new(), i64::MAX, 0),
                                };
                                (
                                    self.index_interior_separator(
                                        child_id,
                                        split_key_bytes.as_deref().unwrap_or_default(),
                                        split_key,
                                    )?,
                                    self.index_interior_separator(new_page, &old_key, old_rowid)?,
                                    old_chain,
                                )
                            } else {
                                let old_key =
                                    decode_table_interior_key(&page.lock().data[old_off..])
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
                                    0u32,
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
                                    let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
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
                                    b.touch();
                                }
                                self.pager.note_dirty(page_id);
                                // The old separator cell was replaced by
                                // cell1+cell2 (cell2 carries a fresh chain
                                // copy) — free the old chain.
                                if old_chain != 0 {
                                    self.free_overflow_chain(old_chain)?;
                                }
                                return Ok(InsertResult::Done);
                            }
                            // No room: the old separator stays (its chain
                            // too); the two fresh copies were never stored.
                            for leaked in [cell1.index_overflow(), cell2.index_overflow()] {
                                if leaked != 0 {
                                    self.free_overflow_chain(leaked)?;
                                }
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
                    let mut right_most;
                    let mut found_idx: Option<usize> = None;
                    {
                        let p = self.pager.get_page(page_id)?;
                        let borrowed = p.lock();
                        right_most = borrowed.right_most_pointer();
                        for i in 0..n_cells {
                            let cell_ptr = borrowed.cell_pointer(i) as usize;
                            let c = Cell::decode(
                                borrowed.cell_slice_checked(cell_ptr)?,
                                pt2,
                                borrowed.page_size(),
                            )?;
                            if c.left_child() == child_id {
                                found_idx = Some(i as usize);
                            }
                            cells.push(c);
                        }
                    }

                    // right=0 + the child is the LAST cell's child: the
                    // de-facto right-most split. Rewrite the last cell's
                    // separator to split_key-1 and promote new_page to the
                    // right-most SLOT (a real right_most pointer). The plain
                    // two-cell splice would emit (split_key-1, old_key) —
                    // non-monotonic (old_key < split_key) — misrouting every
                    // later descent.
                    let effective_right_most =
                        right_most == 0 && found_idx == Some(cells.len() - 1);

                    // The boundary cells this split installs, one per
                    // boundary: each separator points at the page LEFT of
                    // its boundary, and the LAST new page inherits the old
                    // bound (the old separator cell below, or the
                    // right-most slot).
                    let mut boundary_cells: Vec<Cell> = Vec::with_capacity(1 + extra_splits.len());
                    let mut left_page = child_id;
                    let bounds = std::iter::once((new_page, split_key, split_key_bytes)).chain(
                        extra_splits
                            .into_iter()
                            .map(|e| (e.new_page, e.split_key, e.split_key_bytes)),
                    );
                    for (np, sk, skb) in bounds {
                        let cell = if is_idx_page {
                            self.index_interior_separator(
                                left_page,
                                skb.as_deref().unwrap_or_default(),
                                sk,
                            )?
                        } else {
                            Cell::TableInterior {
                                left_child: left_page,
                                key: sk - 1,
                            }
                        };
                        boundary_cells.push(cell);
                        left_page = np;
                    }
                    // `left_page` is now the LAST new page; it takes the old
                    // bound (the last inserted cell below / right_most).
                    let n_bounds = boundary_cells.len();

                    if effective_right_most {
                        // The replaced cell's overflow chain (if any) dies
                        // with it — the copy builds a fresh chain.
                        let dead_chain = if is_idx_page {
                            if let Some(idx) = found_idx {
                                cells[idx].index_overflow()
                            } else {
                                0
                            }
                        } else {
                            0
                        };
                        if let Some(idx) = found_idx {
                            for (offset, c) in boundary_cells.into_iter().enumerate() {
                                if offset == 0 {
                                    cells[idx] = c;
                                } else {
                                    cells.insert(idx + offset, c);
                                }
                            }
                        }
                        if dead_chain != 0 {
                            self.free_overflow_chain(dead_chain)?;
                        }
                        right_most = left_page;
                    } else if let Some(idx) = found_idx {
                        if is_idx_page {
                            // FULL old key (reassembled for overflow cells —
                            // the local prefix would corrupt the order), and
                            // the removed cell's chain dies (the last cell's
                            // copy builds its own).
                            let (old_key, old_rowid, dead_chain) = {
                                let c = &cells[idx];
                                match self.cell_index_key(c) {
                                    Ok(full) => (full, c.key(), c.index_overflow()),
                                    Err(_) => (Vec::new(), i64::MAX, 0),
                                }
                            };
                            let last_cell =
                                self.index_interior_separator(left_page, &old_key, old_rowid)?;
                            cells.remove(idx);
                            for (offset, c) in boundary_cells.into_iter().enumerate() {
                                cells.insert(idx + offset, c);
                            }
                            cells.insert(idx + n_bounds, last_cell);
                            if dead_chain != 0 {
                                self.free_overflow_chain(dead_chain)?;
                            }
                        } else {
                            let old_key = if let Cell::TableInterior { key, .. } = &cells[idx] {
                                *key
                            } else {
                                i64::MAX
                            };
                            let last_cell = Cell::TableInterior {
                                left_child: left_page,
                                key: old_key,
                            };
                            cells.remove(idx);
                            for (offset, c) in boundary_cells.into_iter().enumerate() {
                                cells.insert(idx + offset, c);
                            }
                            cells.insert(idx + n_bounds, last_cell);
                        }
                    } else {
                        // right_most case: child_id was right_most.
                        cells.extend(boundary_cells);
                        right_most = left_page;
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
                        // Byte-aware: a count-mid can overload one half
                        // with oversized-key separators (see the helper).
                        let psz_u = self.pager.page_size() as usize;
                        let avail_i = psz_u
                            - if page_id == 0 {
                                crate::storage::page::DB_HEADER_SIZE as usize
                            } else {
                                0
                            }
                            - PAGE_HEADER_SIZE as usize;
                        let mid = match Self::byte_aware_mid(&cells, avail_i) {
                            Some(m) => m,
                            None => {
                                // No contiguous 2-partition fits (oversized
                                // index separators can pack a parent the
                                // same way blob rows pack a leaf) —
                                // distribute across 3+ pages.
                                return self.multi_way_interior_split(
                                    page_id,
                                    is_idx_page,
                                    cells,
                                    right_most,
                                    avail_i,
                                );
                            }
                        };
                        // Separator to propagate = cells[mid-1]'s separator.
                        let (prop_rowid, prop_key_bytes) = if is_idx_page {
                            // Overflow separators propagate their FULL key
                            // (the parent builds a fresh chain copy).
                            match self.cell_index_key(&cells[mid - 1]) {
                                Ok(full) => (cells[mid - 1].key(), Some(full)),
                                Err(_) => (cells[mid - 1].key(), None),
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

                        return Ok(InsertResult::two_way(
                            new_interior,
                            prop_rowid,
                            prop_key_bytes,
                        ));
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
        self.insert_cell_into_ref(page_id, &page, cell)
    }

    /// `insert_cell_into_page` with the page already fetched — the
    /// descent in `insert_into_page` holds the leaf's PageRef; fetching
    /// it a second time was one full cache round-trip (RwLock + hash +
    /// Arc clone) per inserted cell, paid by every scattered-key insert.
    fn insert_cell_into_ref(&mut self, page_id: PageId, page: &PageRef, cell: &Cell) -> Result<()> {
        self.insert_cell_into_ref_at(page_id, page, cell, None)
    }

    /// [`Self::insert_cell_into_ref`] at a position the caller already
    /// searched for (under the same descent, page unchanged since).
    fn insert_cell_into_ref_at(
        &mut self,
        page_id: PageId,
        page: &PageRef,
        cell: &Cell,
        known_pos: Option<u16>,
    ) -> Result<()> {
        if crate::executor::dbg_index_trace() {
            let pt = self
                .pager
                .get_page(page_id)
                .ok()
                .and_then(|p| p.lock().page_type().ok());
            if matches!(pt, Some(PageType::LeafIndex | PageType::InteriorIndex)) {
                eprintln!(
                    "[cell+->{}] rowid={} key={:02X?}",
                    page_id,
                    cell.key(),
                    cell.index_key()
                        .iter()
                        .take(8)
                        .copied()
                        .collect::<Vec<u8>>()
                );
            }
        }
        // ONE guard across the whole search + write: the historical code
        // locked the page FIVE times per cell (type, n_cells, search,
        // content-start, write) — five uncontended mutex round-trips on
        // every inserted cell, the dominant fixed cost left in the
        // scattered-key index path. get_page is never called while the
        // guard is held (overflow-key reassembly locks OTHER pages'
        // mutexes; no cycle exists), so the single scope is deadlock-free.
        let mut borrowed = page.lock();
        let pt = borrowed.page_type()?;
        let cell_size = cell.encoded_size();
        let n = borrowed.n_cells();
        let is_idx = pt.is_index();

        // Find insertion position by the page's sort order — allocation-free
        // binary search over cell VIEWS. (The old code decoded a full
        // allocating `Cell` — one Vec per probe, ~9 probes per insert —
        // which made page rebuilds after splits O(n) allocations.)
        let pos = if let Some(p) = known_pos {
            p
        } else {
            let borrowed = &*borrowed;
            let mut lo: u16 = 0;
            let mut hi: u16 = n;
            if is_idx {
                // Full key when the inserted cell is an overflow cell (the
                // variant carries only the local prefix) — the binary
                // search must see the total order. In-page cells keep the
                // zero-copy slice.
                let nk_owned: Vec<u8>;
                let nk: &[u8] = if cell.index_overflow() != 0 {
                    nk_owned = self.cell_index_key(cell)?;
                    &nk_owned
                } else {
                    cell.index_key()
                };
                let nr = cell.key();
                let interior = pt.is_interior();
                let psz = borrowed.page_size();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let stored = borrowed.cell_slice_checked(cell_ptr)?;
                    // Fast probe (no rowid decode unless keys tie).
                    match index_cell_lt(stored, interior, psz, nk, nr) {
                        Some(Some(true)) => {
                            lo = mid + 1;
                            continue;
                        }
                        Some(Some(false)) => {
                            hi = mid;
                            continue;
                        }
                        _ => {}
                    }
                    let Some(v) = decode_index_cell(stored, interior, psz) else {
                        return Err(Error::corruption("truncated index cell in search"));
                    };
                    // Stored overflow cells: reassemble the full key (rare —
                    // only oversized-key indexes; in-page cells compare free).
                    let go_right = if v.overflow != 0 {
                        let full = self.index_view_key(&v)?;
                        (full.as_slice(), v.rowid) < (nk, nr)
                    } else {
                        (v.key, v.rowid) < (nk, nr)
                    };
                    if go_right {
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
                        decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                    } else {
                        varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?)
                            .map(|(k, _)| k)
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
        let new_content_start = borrowed
            .cell_content_start()
            .saturating_sub(cell_size as u32);

        // Write the cell bytes STRAIGHT into the page's content area (no
        // staging buffer: a 2 KB row used to be encoded into a heap Vec
        // and copied again — one allocation + one full copy per wide
        // insert).
        {
            let borrowed = &mut *borrowed;
            let off = new_content_start as usize;
            // No-clobber guard: a cell larger than the page's remaining
            // content area would underflow the content start to 0 and
            // overwrite the page HEADER with cell bytes (silent tree
            // corruption). Callers guarantee fit (byte-aware splits); this
            // turns any residual violation into a clean corruption error.
            if new_content_start == 0
                && cell_size > 0
                && borrowed.cell_content_start() < cell_size as u32
            {
                return Err(Error::corruption(format!(
                    "cell ({} bytes) does not fit page {} (content start {})",
                    cell_size,
                    page_id,
                    borrowed.cell_content_start()
                )));
            }
            if off + cell_size > borrowed.data.len() {
                // Corrupt page header (content start out of range): clean
                // corruption error, never an out-of-bounds slice.
                return Err(Error::corruption(format!(
                    "cell content area out of range in insert (page {})",
                    page_id
                )));
            }
            cell.encode_into(&mut borrowed.data[off..off + cell_size]);
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
            borrowed.touch();
            if pt == PageType::LeafIndex {
                self.last_index_leaf =
                    Some(AppendHint::fresh(page_id, borrowed.serial, borrowed.epoch));
            }
        }
        drop(borrowed);
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
            let psz = b.page_size();
            for i in 0..n {
                let p = b.cell_pointer(i) as usize;
                let size = if is_idx {
                    index_leaf_cell_size(&b.data[p..], psz)
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
                // Total-order search: full keys when either side is an
                // overflow cell (see insert_cell_into_page).
                let nk_owned: Vec<u8>;
                let nk: &[u8] = if new_cell.index_overflow() != 0 {
                    nk_owned = self.cell_index_key(new_cell)?;
                    &nk_owned
                } else {
                    new_cell.index_key()
                };
                let nr = new_cell.key();
                let psz = b.page_size();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let p = b.cell_pointer(mid) as usize;
                    let Some(v) = decode_index_cell(&b.data[p..], false, psz) else {
                        return Ok(None);
                    };
                    let go_right = if v.overflow != 0 {
                        let full = self.index_view_key(&v)?;
                        (full.as_slice(), v.rowid) < (nk, nr)
                    } else {
                        (v.key, v.rowid) < (nk, nr)
                    };
                    if go_right {
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
        let mid = n_usize.div_ceil(2);

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
            // NOTE: when pos == mid the new cell belongs to the RIGHT half
            // (sequence index `mid` is not in [0..mid)) — do NOT add it
            // here. The old code added `s_new` in that case, which
            // undercounted the RIGHT page's bytes by exactly `s_new`: a
            // packed right page then overflowed its cell content into the
            // pointer array and corrupted the tree (missing entries,
            // cyclic child pointers). Found via 8 KiB-page backfill; latent
            // at every page size whenever a mid-insert lands exactly at the
            // split point.
            b
        };
        let avail = page_size - header_offset - PAGE_HEADER_SIZE as usize;
        // Exact byte sums for both halves: sequence bytes total =
        // existing cells + the new cell.
        let right_bytes = total_bytes + s_new - left_bytes;
        if left_bytes + left_cells * 2 > avail || right_bytes + right_cells * 2 > avail {
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
        if crate::executor::dbg_index_trace() {
            eprintln!(
                "[split:mid] page={} type={:?} n={} pos={} mid={} left_cells={} right_cells={} left_bytes={} right_bytes={} new_page={}",
                page_id, pt, n_usize, pos, mid, left_cells, right_cells, left_bytes, right_bytes, new_page_id
            );
        }
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
            b.touch();
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
            nb.touch();
        }
        self.pager.note_dirty(page_id);
        self.pager.note_dirty(new_page_id);

        // Separator for the parent.
        if is_idx {
            // Exact separator: the left page's last entry = sequence[mid-1].
            let (sep_key, sep_rowid) = {
                let page = self.pager.get_page(page_id)?;
                let b = page.lock();
                let psz = b.page_size();
                let p = b.cell_pointer((mid - 1) as u16) as usize;
                match decode_index_cell(&b.data[p..], false, psz) {
                    // The FULL key flows up — the parent builds its own
                    // overflow copy via index_interior_separator.
                    Some(v) => {
                        let full = self.index_view_key(&v)?;
                        (full, v.rowid)
                    }
                    None => return Ok(None),
                }
            };
            Ok(Some(InsertResult::two_way(
                new_page_id,
                sep_rowid,
                Some(sep_key),
            )))
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
            Ok(Some(InsertResult::two_way(new_page_id, split_key, None)))
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
        if crate::executor::dbg_index_trace() {
            eprintln!(
                "[split:append] page={} type={:?} cell_rowid={} key={:02X?}",
                page_id,
                pt,
                new_cell.key(),
                new_cell
                    .index_key()
                    .iter()
                    .take(8)
                    .copied()
                    .collect::<Vec<u8>>()
            );
        }
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
        // CONCURRENT-REGIME SOUNDNESS: the append-split's correctness rests
        // on the OLD page's content — its last cell bounds the append, the
        // old page keeps every cell (no redistribution), and the parent's
        // promoted separator must not under-cover the page's true max. In
        // a concurrent transaction the old page's shadow may be STALE (a
        // sibling commit appended LARGER keys to this very page after our
        // fetch — the right-most leaf is exactly where sibling writers
        // append). The old page stays byte-identical here, so its shadow
        // remains CLEAN and the commit validation would skip it — a stale
        // view would then install a parent separator that strands the
        // sibling's tail rows behind a mis-routed interior cell (the S4
        // soak's intermittent corruption: leaf [..360, 1.02e9..25] got
        // separator 360, 25 committed rows unreachable by descent). Mark
        // it a decision read: if the stamp moved, the commit conflicts and
        // the merge replay re-inserts against the CURRENT tree; if not,
        // the view was current and the promotion is sound.
        self.pager.note_decision_read(page_id);
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
                let psz = borrowed.page_size();
                let last_ptr = borrowed.cell_pointer(n - 1) as usize;
                match decode_index_cell(&borrowed.data[last_ptr..], false, psz) {
                    Some(v) => {
                        // Full key up (parent separator copies get their
                        // own chains via index_interior_separator).
                        let full = self.index_view_key(&v)?;
                        (full, v.rowid)
                    }
                    None => {
                        let full = self.cell_index_key(&new_cell)?;
                        (full, new_cell.key())
                    }
                }
            };
            Ok(InsertResult::two_way(new_page_id, sep_rowid, Some(sep_key)))
        } else {
            // Table separator: first key of the new page = the new rowid.
            Ok(InsertResult::two_way(new_page_id, new_cell.key(), None))
        }
    }

    /// Split a leaf page. Returns the new page ID and the separator info.
    ///
    /// DELETE-CHURN COMPACTION (the pre-split reclaim): deletes drop only
    /// the cell POINTER — the cell bytes stay in the content area as dead
    /// space until a split's rebuild compacts them, and `free_space()`
    /// (the gap between the pointer array and the content frontier) does
    /// NOT count them. A leaf that is 75% dead therefore reports almost
    /// no free space, and a re-inserting workload SPLITS it instead of
    /// reusing the dead bytes — the classic churn bloat (measured:
    /// delete-75% + re-insert the same shape grew a 60k-row file 1.67x,
    /// 683 → 1140 pages, with the logical content unchanged).
    ///
    /// This is the SQLite balancer's answer applied at leaf granularity:
    /// when a leaf cannot fit a cell in its GAP but the cell WOULD fit
    /// after reclaiming the dead bytes, compact the page in place (one
    /// scratch copy + rewrite, no new page, no parent churn) and let the
    /// insert proceed. Splits now happen only for genuinely full pages.
    ///
    /// Returns `Ok(true)` when the page was compacted AND now has room
    /// for `need` bytes (cell + pointer) — the caller inserts directly.
    /// `Ok(false)` = not a leaf / packed / wouldn't fit even compacted /
    /// malformed cell — the caller splits as before.
    fn compact_leaf_if_fragmented(&mut self, page_id: PageId, need: usize) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let psz;
        let pt;
        let n;
        let content_start;
        let is_packed;
        let live_bytes;
        {
            let b = page.lock();
            pt = b.page_type()?;
            if !matches!(pt, PageType::LeafTable | PageType::LeafIndex) {
                return Ok(false);
            }
            psz = b.page_size() as usize;
            n = b.n_cells() as usize;
            content_start = b.cell_content_start() as usize;
            if n == 0 {
                // Nothing to compact — an empty leaf's whole content area
                // is already gap (free_space is honest). Splitting (or the
                // caller's insert) proceeds normally.
                return Ok(false);
            }
            // Exact in-page byte sum, overflow-aware.
            let mut total = 0usize;
            let mut packed_ok = true;
            for i in 0..n as u16 {
                let p = b.cell_pointer(i) as usize;
                let size = match pt {
                    PageType::LeafIndex => index_leaf_cell_size(&b.data[p..], psz as u32),
                    _ => table_leaf_cell_size_paged(&b.data[p..], psz as u32),
                };
                match size {
                    Some(s) if s > 0 && p + s <= psz => total += s,
                    Some(_) => {
                        // Oversized/truncated view (spilled table cell seen
                        // by the non-paged helper, or corruption): let the
                        // split path's own validation handle it.
                        packed_ok = false;
                        break;
                    }
                    None => {
                        packed_ok = false;
                        break;
                    }
                }
            }
            if !packed_ok {
                return Ok(false);
            }
            live_bytes = total;
            is_packed = live_bytes + content_start == psz;
        }
        if is_packed {
            return Ok(false); // no dead space to reclaim
        }
        // Would the cell fit after compaction? Layout after rewrite:
        //   [header][ptr array (n+1) * 2][gap][cells ... live_bytes]
        let header_offset = if page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_end = header_offset + PAGE_HEADER_SIZE as usize + (n + 1) * 2;
        let free_after = psz.saturating_sub(ptr_array_end + live_bytes);
        if free_after < need {
            return Ok(false); // genuinely full — split is the answer
        }
        // ---- Compact: scratch-copy every live cell (pointer order), then
        //      rewrite the content area contiguously from the page tail
        //      downward and rebuild the pointer array. O(page) work, one
        //      scratch allocation, no page-count change, no parent churn.
        //      The scratch copy is mandatory: writing cells to the tail can
        //      overlap the source region of cells not yet copied.
        let mut scratch: Vec<u8> = Vec::with_capacity(live_bytes);
        let mut sizes: Vec<usize> = Vec::with_capacity(n);
        {
            let b = page.lock();
            for i in 0..n as u16 {
                let p = b.cell_pointer(i) as usize;
                let size = match pt {
                    PageType::LeafIndex => index_leaf_cell_size(&b.data[p..], psz as u32),
                    _ => table_leaf_cell_size_paged(&b.data[p..], psz as u32),
                }
                .unwrap_or(0);
                scratch.extend_from_slice(&b.data[p..p + size]);
                sizes.push(size);
            }
        }
        debug_assert_eq!(scratch.len(), live_bytes);
        {
            let mut b = page.lock();
            let mut src = scratch.len();
            let mut write_off = psz;
            for i in (0..n).rev() {
                let size = sizes[i];
                src -= size;
                write_off -= size;
                b.data[write_off..write_off + size].copy_from_slice(&scratch[src..src + size]);
                // Pointer for cell i (big-endian u16 offset).
                let dst = header_offset + PAGE_HEADER_SIZE as usize + i * 2;
                b.data[dst..dst + 2].copy_from_slice(&(write_off as u16).to_be_bytes());
            }
            debug_assert_eq!(src, 0);
            b.set_cell_content_start(write_off as u32);
            // n_cells unchanged — only offsets moved.
            b.touch();
        }
        self.pager.note_dirty(page_id);
        // The compaction moved every cell and bumped the page serial:
        // advisory leaf hints / append pins re-validate by serial and
        // will re-establish on their next use. The insert below takes
        // the normal path through the (now honest) free-space gap.
        Ok(true)
    }

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
                    let psz = borrowed.page_size();
                    if let Some(last) = decode_index_cell(&borrowed.data[last_ptr..], false, psz) {
                        // Total order: full keys when either side overflows.
                        let nk_owned: Vec<u8>;
                        let nk: &[u8] = if new_cell.index_overflow() != 0 {
                            nk_owned = self.cell_index_key(&new_cell)?;
                            &nk_owned
                        } else {
                            new_cell.index_key()
                        };
                        let last_owned: Vec<u8>;
                        let last_key: &[u8] = if last.overflow != 0 {
                            last_owned = self.index_view_key(&last)?;
                            &last_owned
                        } else {
                            last.key
                        };
                        nk > last_key || (nk == last_key && new_cell.key() > last.rowid)
                    } else {
                        false
                    }
                } else {
                    if let Some((last_rowid, _)) = varint::decode_signed(&borrowed.data[last_ptr..])
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
        let new_cell_key: Option<Vec<u8>> = if new_cell.index_overflow() != 0
            || matches!(
                new_cell,
                Cell::IndexLeaf { .. } | Cell::IndexInterior { .. }
            ) {
            // Overflow variants carry only the local prefix — the full key
            // drives the merge order below.
            Some(self.cell_index_key(&new_cell)?)
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
            let c = Cell::decode(
                borrowed.cell_slice_checked(cell_ptr)?,
                pt,
                borrowed.page_size(),
            )?;
            let existing_before_new = if is_idx {
                // Total order: full keys when either cell overflows.
                let c_full = self.cell_index_key(&c)?;
                let nk: &[u8] = new_cell_key.as_deref().unwrap_or(&[]);
                (c_full.as_slice(), c.key()) < (nk, new_cell.key())
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
                    let c = Cell::decode(
                        borrowed.cell_slice_checked(cell_ptr)?,
                        pt,
                        borrowed.page_size(),
                    )?;
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
            let new_is_last = cells
                .last()
                .map(|c| {
                    if is_idx {
                        // Last cell IS the new cell iff its full key matches
                        // (overflow cells compare by reassembled key).
                        let same = if c.index_overflow() != 0 {
                            self.cell_index_key(c)
                                .ok()
                                .as_deref()
                                .map(|f| f == new_cell_key.as_deref().unwrap_or(&[]))
                                .unwrap_or(false)
                        } else {
                            c.index_key() == new_cell_key.as_deref().unwrap_or(&[])
                        };
                        same && c.key() == new_cell_key_rowid
                    } else {
                        c.key() == new_cell_key_rowid
                    }
                })
                .unwrap_or(false);
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
                    let c = Cell::decode(
                        borrowed.cell_slice_checked(cell_ptr)?,
                        pt,
                        borrowed.page_size(),
                    )?;
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
        let mid = if is_append && total > 1 {
            total - 1
        } else {
            // Byte-aware: oversized index cells make the count-mid load
            // one half past the page budget (header clobber otherwise).
            let psz_u = self.pager.page_size() as usize;
            let avail_l = psz_u
                - if page_id == 0 {
                    crate::storage::page::DB_HEADER_SIZE as usize
                } else {
                    0
                }
                - PAGE_HEADER_SIZE as usize;
            match Self::byte_aware_mid(&cells, avail_l) {
                Some(m) => m,
                None => {
                    // No contiguous 2-partition fits (a packed page plus a
                    // mid-key insert bigger than the leftover gap — see
                    // `InsertResult::Split::extra_splits`). Distribute
                    // across 3+ pages instead of failing.
                    return self.multi_way_leaf_split(page_id, pt, cells, avail_l);
                }
            }
        };

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
            // child's separator. OVERFLOW cells must send the FULL key
            // (the local prefix would corrupt the parent's ordering).
            let left_max = &cells[mid - 1];
            let sep_bytes = if left_max.index_overflow() != 0 {
                self.cell_index_key(left_max)?
            } else {
                left_max.index_key().to_vec()
            };
            Ok(InsertResult::Split {
                new_page: new_page_id,
                split_key: left_max.key(),
                split_key_bytes: Some(sep_bytes),
                extra_splits: Vec::new(),
            })
        } else {
            // The split key is the FIRST key of the new page (the min key in
            // the second half). The parent applies the -1 gap convention.
            let split_key = cells[mid].key();
            Ok(InsertResult::Split {
                new_page: new_page_id,
                split_key,
                split_key_bytes: None,
                extra_splits: Vec::new(),
            })
        }
    }

    /// 3+-way LEAF split — the no-feasible-2-way corner (`greedy_runs`).
    ///
    /// A page packed to near-avail with sizeable rows can receive a
    /// mid-key insert where EVERY contiguous 2-partition overloads one
    /// half past the page budget. Distribute the cells across as many
    /// pages as needed: run 1 stays on `page_id` (stable page identity
    /// for in-flight descents), every further run gets a fresh page, and
    /// every boundary reports a separator to the parent exactly like an
    /// ordinary split (table trees: the right run's first key; index
    /// trees: the left run's max entry, full key included).
    fn multi_way_leaf_split(
        &mut self,
        page_id: PageId,
        pt: PageType,
        cells: Vec<Cell>,
        avail: usize,
    ) -> Result<InsertResult> {
        let is_index = matches!(pt, PageType::LeafIndex);
        // A lone cell exceeding the page budget cannot fit ANY page — the
        // pre-existing corruption signal (unreachable for legal cells:
        // max_cell_payload < budget).
        if cells.iter().any(|c| c.encoded_size() + 2 > avail) {
            return Err(Error::corruption(format!(
                "leaf page {} cannot split: a single cell exceeds the page budget",
                page_id
            )));
        }
        let runs = Self::greedy_runs(&cells, avail);
        debug_assert!(runs.len() >= 2, "over-budget cell set split into one run");
        // Clear the old page; run 1 re-populates it.
        {
            let page_ref = self.pager.get_page(page_id)?;
            let mut borrowed = page_ref.lock();
            if is_index {
                borrowed.init_leaf_index();
            } else {
                borrowed.init_leaf_table();
            }
        }
        self.pager.note_dirty(page_id);

        let mut boundaries: Vec<ExtraSplit> = Vec::with_capacity(runs.len() - 1);
        let mut prev_end = 0usize;
        for (ri, run) in runs.iter().enumerate() {
            if ri == 0 {
                for c in &cells[run.clone()] {
                    self.insert_cell_into_page(page_id, c)?;
                }
            } else {
                let new_page_id = self.pager.allocate_page()?;
                let new_page = self.pager.get_page(new_page_id)?;
                if is_index {
                    new_page.lock().init_leaf_index();
                } else {
                    new_page.lock().init_leaf_table();
                }
                for c in &cells[run.clone()] {
                    self.insert_cell_into_page(new_page_id, c)?;
                }
                // Boundary separator between the previous run and this
                // one — same conventions as the 2-way split's return.
                let (split_key, split_key_bytes) = if is_index {
                    // Exact separator: the LEFT run's last entry, full
                    // key (overflow variants reassemble their chain).
                    let left_max = &cells[prev_end - 1];
                    let full = self.cell_index_key(left_max)?;
                    (left_max.key(), Some(full))
                } else {
                    // First key of the right run; the parent applies the
                    // -1 gap convention.
                    (cells[run.start].key(), None)
                };
                boundaries.push(ExtraSplit {
                    new_page: new_page_id,
                    split_key,
                    split_key_bytes,
                });
            }
            prev_end = run.end;
        }
        let mut it = boundaries.into_iter();
        // ≥ 2 runs ⇒ ≥ 1 boundary; an over-budget set cannot yield one.
        let first = it.next().ok_or_else(|| {
            Error::corruption("multi-way leaf split found no boundary".to_string())
        })?;
        Ok(InsertResult::Split {
            new_page: first.new_page,
            split_key: first.split_key,
            split_key_bytes: first.split_key_bytes,
            extra_splits: it.collect(),
        })
    }

    /// 3+-way INTERIOR split — the same no-feasible-2-way corner for the
    /// parent-rewrite path (reachable with oversized index separators:
    /// interior index cells carry full key copies in-page). Identical
    /// distribution discipline as `multi_way_leaf_split`; the tree's old
    /// right-most pointer moves to the LAST new page, and table trees
    /// report boundary keys by the interior convention (left run's max
    /// separator + 1; the parent applies -1 to recover it).
    fn multi_way_interior_split(
        &mut self,
        page_id: PageId,
        is_idx_page: bool,
        cells: Vec<Cell>,
        right_most: PageId,
        avail: usize,
    ) -> Result<InsertResult> {
        if cells.iter().any(|c| c.encoded_size() + 2 > avail) {
            return Err(Error::corruption(format!(
                "interior page {} cannot split: a single cell exceeds the page budget",
                page_id
            )));
        }
        let runs = Self::greedy_runs(&cells, avail);
        debug_assert!(runs.len() >= 2, "over-budget cell set split into one run");
        // Rewrite the old page with run 1.
        {
            let p = self.pager.get_page(page_id)?;
            let mut borrowed = p.lock();
            if is_idx_page {
                borrowed.init_interior_index();
            } else {
                borrowed.init_interior_table();
            }
        }
        self.pager.note_dirty(page_id);
        for c in &cells[runs[0].clone()] {
            self.insert_cell_into_page(page_id, c)?;
        }

        let mut boundaries: Vec<ExtraSplit> = Vec::with_capacity(runs.len() - 1);
        let mut prev_end = 0usize;
        for (ri, run) in runs.iter().enumerate().skip(1) {
            let new_page_id = self.pager.allocate_page()?;
            {
                let p = self.pager.get_page(new_page_id)?;
                let mut borrowed = p.lock();
                if is_idx_page {
                    borrowed.init_interior_index();
                } else {
                    borrowed.init_interior_table();
                }
            }
            for c in &cells[run.clone()] {
                self.insert_cell_into_page(new_page_id, c)?;
            }
            let (split_key, split_key_bytes) = if is_idx_page {
                // Exact separator: the left run's last cell, FULL key.
                let left_max = &cells[prev_end - 1];
                let full = self.cell_index_key(left_max)?;
                (left_max.key(), Some(full))
            } else {
                // Interior convention: left run's max separator + 1 (the
                // parent applies -1 to recover the left max).
                (cells[prev_end - 1].key() + 1, None)
            };
            if ri == runs.len() - 1 {
                // The tree's right-most pointer moves to the LAST page.
                self.pager
                    .get_page(new_page_id)?
                    .lock()
                    .set_right_most_pointer(right_most);
            }
            boundaries.push(ExtraSplit {
                new_page: new_page_id,
                split_key,
                split_key_bytes,
            });
            prev_end = run.end;
        }
        let mut it = boundaries.into_iter();
        let first = it.next().ok_or_else(|| {
            Error::corruption("multi-way interior split found no boundary".to_string())
        })?;
        Ok(InsertResult::Split {
            new_page: first.new_page,
            split_key: first.split_key,
            split_key_bytes: first.split_key_bytes,
            extra_splits: it.collect(),
        })
    }

    // Split an interior page. Same idea but the middle cell moves up.
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
    //         let c = Cell::decode(borrowed.cell_slice_checked(cell_ptr)?, pt, borrowed.page_size())?;
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

    /// Bulk-delete unlink cascade: remove `child`'s reference from its
    /// parent (separator cell or rightmost pointer), free the child, and
    /// — when the parent becomes fully childless (0 cells, rightmost 0)
    /// — recurse upward through `path` (the descent's interior chain,
    /// root → … → parent), freeing childless interiors too. When the
    /// cascade empties the ROOT, the root page is converted in place to
    /// an empty leaf (a valid empty tree, root id preserved for the
    /// schema maps).
    ///
    /// This is the tail-reclaim half of SQLite's truncate-on-mass-delete:
    /// without the cascade, emptied interior orphans and rightmost-guard
    /// leaves sit at the file's highest page ids and block the
    /// contiguity walk in `Pager::truncate_tail`.
    ///
    /// Safety: only ever frees pages that hold NO data and NO children —
    /// an emptied leaf (n_cells == 0), or an interior with 0 cells AND
    /// rightmost == 0. A childless interior covers an empty key range,
    /// so removing the parent's pointer to it loses no keys; descents
    /// already treat (0 cells, rightmost 0) as an empty subtree.
    fn unlink_child_cascade(&mut self, path: &[PageId], leaf: PageId) -> Result<()> {
        let mut child = leaf;
        let mut depth = path.len();
        loop {
            if child == 0 || child == self.root {
                // Root emptied as a LEAF (single-page tree) — a valid
                // empty tree; nothing to unlink. Root emptied as an
                // interior is handled by the in-place conversion below.
                return Ok(());
            }
            let parent = if depth == 0 {
                self.root
            } else {
                path[depth - 1]
            };
            // Remove the parent's reference to `child` (cell or
            // rightmost). Returns whether the parent now references
            // NOTHING for this subtree.
            let parent_childless = self.unlink_child_reference(parent, child)?;
            // Free the child (it is empty of data by construction).
            self.pager.free_page(child)?;
            self.note_freed(child);
            if !parent_childless {
                return Ok(());
            }
            if parent == self.root {
                // The root lost its last child: convert it in place to an
                // empty leaf — the canonical empty tree. (A 0-cell
                // childless interior root would confuse the INSERT
                // descent's null-child guard; an empty leaf root is what
                // `Btree::create` produces and every path handles.)
                let root_ref = self.pager.get_page(self.root)?;
                let mut b = root_ref.lock();
                b.init_leaf_table();
                b.touch();
                drop(b);
                self.pager.note_dirty(self.root);
                return Ok(());
            }
            // Parent is a childless interior: continue the cascade with
            // the parent as the child of ITS parent.
            child = parent;
            depth = depth.saturating_sub(1);
        }
    }

    /// Remove `child`'s reference from `parent`: the separator cell that
    /// points at it, or the rightmost pointer. Returns true when the
    /// parent now covers NOTHING (0 cells and rightmost 0) — the caller
    /// cascades. `child == self.root` is never passed here.
    fn unlink_child_reference(&mut self, parent: PageId, child: PageId) -> Result<bool> {
        let (n_cells, found_cell_idx, is_rightmost) = {
            let parent_ref = self.pager.get_page(parent)?;
            let borrowed = parent_ref.lock();
            if borrowed.page_type()? != PageType::InteriorTable {
                // Parent is the root-as-leaf (single-page tree): nothing
                // to unlink; the leaf stays as the empty root.
                return Ok(false);
            }
            let n = borrowed.n_cells();
            let mut found = None;
            for i in 0..n {
                let cell_ptr = borrowed.cell_pointer(i) as usize;
                if let Ok(Some(c)) = borrowed
                    .cell_slice_checked(cell_ptr)
                    .map(decode_table_interior_child)
                {
                    if c == child {
                        found = Some(i as usize);
                        break;
                    }
                }
            }
            (
                n,
                found,
                found.is_none() && borrowed.right_most_pointer() == child,
            )
        };
        if !is_rightmost && found_cell_idx.is_none() {
            // No reference anywhere (an orphan from an earlier partial
            // unlink): the page is unreferenced garbage — the caller
            // frees it. The parent's coverage is unaffected.
            return Ok(false);
        }
        let header_offset = if parent == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
        match found_cell_idx {
            Some(idx) => {
                // Remove the separator cell slot (memmove the pointer
                // array). Removing the LAST cell of a rightmost==0
                // interior is safe HERE because the caller cascades the
                // now-childless parent away immediately.
                let parent_ref = self.pager.get_page(parent)?;
                let mut borrowed = parent_ref.lock();
                let n_usize = n_cells as usize;
                for i in idx..n_usize.saturating_sub(1) {
                    let src = ptr_array_start + (i + 1) * 2;
                    let dst = ptr_array_start + i * 2;
                    let v = u16::from_be_bytes(
                        borrowed.data[src..src + 2].try_into().unwrap_or([0; 2]),
                    );
                    borrowed.data[dst..dst + 2].copy_from_slice(&v.to_be_bytes());
                }
                let new_n = n_cells.saturating_sub(1);
                borrowed.set_n_cells(new_n);
                borrowed.touch();
                drop(borrowed);
                self.pager.note_dirty(parent);
                let childless = new_n == 0 && {
                    let p = self.pager.get_page(parent)?;
                    let b = p.lock();
                    b.right_most_pointer() == 0
                };
                Ok(childless)
            }
            None => {
                // Rightmost child: the last cell's left_child becomes the
                // new rightmost. With 0 cells, rightmost becomes 0 (the
                // childless signal for the cascade).
                let new_rightmost = {
                    let parent_ref = self.pager.get_page(parent)?;
                    let mut borrowed = parent_ref.lock();
                    let mut r = 0u32;
                    if n_cells > 0 {
                        let last_idx = n_cells as usize - 1;
                        let cell_ptr = borrowed.cell_pointer(last_idx as u16) as usize;
                        if let Ok(buf) = borrowed.cell_slice_checked(cell_ptr) {
                            r = decode_table_interior_child(buf).unwrap_or(0);
                        }
                        borrowed.set_n_cells(n_cells - 1);
                        borrowed.touch();
                    }
                    borrowed.set_right_most_pointer(r);
                    r
                };
                self.pager.note_dirty(parent);
                Ok(new_rightmost == 0 && n_cells <= 1)
            }
        }
    }

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
    fn maybe_recycle_empty_child(&mut self, parent_id: PageId, child_id: PageId) -> Result<bool> {
        if child_id == 0 || child_id == self.root {
            return Ok(false);
        }
        // Child must be an EMPTY leaf.
        {
            let child_ref = self.pager.get_page(child_id)?;
            let borrowed = child_ref.lock();
            match borrowed.page_type()? {
                PageType::LeafTable | PageType::LeafIndex => {
                    if borrowed.n_cells() != 0 {
                        return Ok(false);
                    }
                }
                _ => return Ok(false), // interior child — don't recycle
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
                let c = Cell::decode(
                    borrowed.cell_slice_checked(cell_ptr)?,
                    pt,
                    borrowed.page_size(),
                )?;
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
                // Removing this cell would leave the parent with zero
                // cells. With a non-zero rightmost that is still routable
                // (everything goes right), but with rightmost == 0 (a
                // post-split interior) the whole subtree would become
                // unreachable and descents would fall through to page 0.
                // Keep the empty leaf linked instead — scans skip it.
                if n_cells == 1 {
                    let right = {
                        let parent_ref = self.pager.get_page(parent_id)?;
                        let r = parent_ref.lock().right_most_pointer();
                        r
                    };
                    if right == 0 {
                        return Ok(false);
                    }
                }
                // Remove the separator cell slot. An overflow separator's
                // chain dies with it — capture it BEFORE the pointer shift.
                let dead_chain = {
                    let parent_ref = self.pager.get_page(parent_id)?;
                    let borrowed = parent_ref.lock();
                    let psz = borrowed.page_size();
                    let cell_ptr = borrowed.cell_pointer(idx as u16) as usize;
                    decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, true, psz)
                        .map(|v| v.overflow)
                        .unwrap_or(0)
                };
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
                borrowed.touch();
                if dead_chain != 0 {
                    self.free_overflow_chain(dead_chain)?;
                }
            }
            None => {
                // Child is the rightmost. The last cell's left_child becomes
                // the new rightmost.
                if n_cells == 0 {
                    return Ok(false); // only child — keep the empty leaf
                }
                let last_idx = n_cells as usize - 1;
                let (new_rightmost, dead_chain) = {
                    let parent_ref = self.pager.get_page(parent_id)?;
                    let borrowed = parent_ref.lock();
                    let cell_ptr = borrowed.cell_pointer(last_idx as u16) as usize;
                    let c = Cell::decode(
                        borrowed.cell_slice_checked(cell_ptr)?,
                        pt,
                        borrowed.page_size(),
                    )?;
                    // The dropped cell is a separator: an overflow key's
                    // chain dies with it (the other branch frees it too).
                    (c.left_child(), c.index_overflow())
                };
                {
                    let parent_ref = self.pager.get_page(parent_id)?;
                    let mut borrowed = parent_ref.lock();
                    borrowed.set_right_most_pointer(new_rightmost);
                    // Remove the last cell slot (no shift needed — it's the tail).
                    borrowed.set_n_cells(n_cells - 1);
                    borrowed.touch();
                }
                if dead_chain != 0 {
                    self.free_overflow_chain(dead_chain)?;
                }
            }
        }
        self.pager.note_dirty(parent_id);
        // Free the empty leaf — the pager zeroes it and links it onto the
        // freelist; the next allocate_page pops it instead of growing the
        // file.
        self.pager.free_page(child_id)?;
        self.note_freed(child_id);
        Ok(true)
    }

    /// Delete a (rowid) from a table B+tree. Does not rebalance (we leave
    /// pages underfull rather than risk concurrent-merge bugs).
    pub fn delete_table(&mut self, rowid: i64) -> Result<bool> {
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.delete_table_inner(rowid);
        match r {
            Ok(did) => {
                if !self.journal_suppress {
                    self.pager
                        .note_row_write(root, false, JournalKind::Delete, rowid, &[], &[]);
                    self.pager.note_row_outcome(did);
                }
                Ok(did)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn delete_table_inner(&mut self, rowid: i64) -> Result<bool> {
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
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.delete_table_get_payload_inner(rowid);
        match r {
            Ok(payload) => {
                if !self.journal_suppress {
                    self.pager
                        .note_row_write(root, false, JournalKind::Delete, rowid, &[], &[]);
                    self.pager.note_row_outcome(payload.is_some());
                }
                Ok(payload)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn delete_table_get_payload_inner(&mut self, rowid: i64) -> Result<Option<Vec<u8>>> {
        self.pager.note_write();
        // Payload of the doomed cell: inline bytes, or a spilled prefix +
        // chain head that must be assembled (and whose chain is freed by
        // `delete_from_page`).
        enum DecodedOldPayload {
            Inline(Vec<u8>),
            Spilled {
                local: Vec<u8>,
                total: u64,
                chain: PageId,
            },
        }
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
                            let (cell_rowid, _) =
                                decode_rowid_only(borrowed.cell_slice_checked(cell_ptr)?)
                                    .ok_or_else(|| {
                                        Error::corruption("truncated leaf rowid in delete")
                                    })?;
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
                                    let (_, n_rid) =
                                        decode_rowid_only(borrowed.cell_slice_checked(cell_ptr)?)
                                            .ok_or_else(|| Error::corruption("truncated rowid"))?;
                                    cell_ptr + n_rid
                                };
                                let (plen, n_plen) = varint::decode(&borrowed.data[plen_pos..])
                                    .ok_or_else(|| Error::corruption("truncated payload length"))?;
                                let start = plen_pos + n_plen;
                                let psz = borrowed.data.len();
                                let local_len = overflow_local_len_for(plen as usize, psz);
                                if local_len == plen as usize {
                                    Some(DecodedOldPayload::Inline(
                                        borrowed.data[start..start + plen as usize].to_vec(),
                                    ))
                                } else {
                                    // Spilled row: local prefix + 4-byte chain
                                    // head; assembled after the page lock drops.
                                    let local = borrowed.data[start..start + local_len].to_vec();
                                    let chain = u32::from_be_bytes(
                                        borrowed.data[start + local_len..start + local_len + 4]
                                            .try_into()
                                            .unwrap(),
                                    );
                                    Some(DecodedOldPayload::Spilled {
                                        local,
                                        total: plen,
                                        chain,
                                    })
                                }
                            }
                        }
                    };
                    let payload: Option<Vec<u8>> = match payload {
                        None => return Ok(None),
                        Some(DecodedOldPayload::Inline(v)) => Some(v),
                        Some(DecodedOldPayload::Spilled {
                            local,
                            total,
                            chain,
                        }) => {
                            // Leaf guard already dropped above; the chain
                            // walk locks only overflow pages.
                            Some(self.assemble_overflow_payload(&local, total, chain)?)
                        }
                    };
                    drop(page);
                    let deleted = self.delete_from_page(page_id, rowid)?;
                    debug_assert!(
                        deleted,
                        "binary search found the cell but delete_from_page didn't"
                    );
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
                            let sep =
                                decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
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
                            decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                                .unwrap_or_else(|| borrowed.right_most_pointer())
                        }
                    };
                    drop(page);
                    if next == 0 {
                        return Ok(None);
                    }
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

    /// Bulk in-order delete for the streaming DELETE fast path. `rowids`
    /// MUST be strictly ascending (the range/scan collectors guarantee
    /// this — they walk the table B+tree once in rowid order). A sticky
    /// leaf plus live key bounds skips the root-to-leaf descent for every
    /// row after the first: sequential mass deletes (`DELETE FROM t WHERE
    /// id > K`) drop from ~500 ns/row (fresh descent each) to ~40-80
    /// ns/row. Callers that need the deleted payloads (index maintenance,
    /// RETURNING, triggers) keep the per-row path.
    pub fn delete_rowids_inorder(&mut self, rowids: &[i64]) -> Result<u64> {
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.delete_rowids_inorder_inner(rowids);
        match r {
            Ok(deleted) => {
                if !self.journal_suppress {
                    if deleted as usize == rowids.len() {
                        // Every requested rowid actually removed — the
                        // journal exactly describes the bulk effect.
                        for &rid in rowids {
                            self.pager.note_row_write(
                                root,
                                false,
                                JournalKind::Delete,
                                rid,
                                &[],
                                &[],
                            );
                            self.pager.note_row_outcome(true);
                        }
                    } else {
                        // Some rowids were absent: WHICH ones failed is
                        // unknown at this boundary — invalidate rather
                        // than journal guesses (page-granularity fallback).
                        self.pager.note_journal_invalidated();
                    }
                }
                Ok(deleted)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn delete_rowids_inorder_inner(&mut self, rowids: &[i64]) -> Result<u64> {
        self.pager.note_write();
        let mut deleted: u64 = 0;
        let mut sticky: Option<PageId> = None;
        let mut lo = i64::MIN;
        let mut hi = i64::MAX;
        let mut i = 0usize;
        let mut spill_chains: Vec<PageId> = Vec::new();
        // Interior path of the current sticky descent (root → … →
        // sticky's parent): the whole-leaf unlink cascade frees childless
        // interiors upward through it.
        let mut path: Vec<PageId> = Vec::new();
        // Arm the freed-page collector: every page this bulk delete
        // unlinks + frees is a candidate for FILE-TAIL truncation (the
        // append-insert then mass-delete-tail shape: pages were
        // allocated in ascending order, so the freed suffix is exactly
        // the file's tail).
        self.collect_frees = true;
        self.freed_during_op.clear();
        while i < rowids.len() {
            let rid = rowids[i];
            // (Re)pin the sticky leaf for this rowid.
            if sticky.is_none() || rid < lo || rid > hi {
                let mut page_id = self.root;
                // Full interior path root→…→parent (for the unlink cascade).
                path.clear();
                loop {
                    let page = self.pager.get_page(page_id)?;
                    let pt = page.lock().page_type()?;
                    match pt {
                        PageType::LeafTable => break,
                        PageType::InteriorTable => {
                            let next = {
                                let borrowed = page.lock();
                                let n = borrowed.n_cells();
                                let mut l: u16 = 0;
                                let mut h: u16 = n;
                                while l < h {
                                    let mid = (l + h) / 2;
                                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                                    let sep = decode_table_interior_key(
                                        borrowed.cell_slice_checked(cell_ptr)?,
                                    )
                                    .ok_or_else(|| {
                                        Error::corruption("truncated interior cell in bulk delete")
                                    })?;
                                    if rid <= sep {
                                        h = mid;
                                    } else {
                                        l = mid + 1;
                                    }
                                }
                                if l >= n {
                                    borrowed.right_most_pointer()
                                } else {
                                    let cell_ptr = borrowed.cell_pointer(l) as usize;
                                    decode_table_interior_child(
                                        borrowed.cell_slice_checked(cell_ptr)?,
                                    )
                                    .unwrap_or_else(|| borrowed.right_most_pointer())
                                }
                            };
                            drop(page);
                            if next == 0 {
                                // Unset rightmost on an emptied interior:
                                // the rowid is not in this subtree (never
                                // recurse into page 0 — the header/schema
                                // page). Skip this rowid; the bounds
                                // refresh below unpins the interior page.
                                i += 1;
                                break;
                            }
                            path.push(page_id);
                            page_id = next;
                        }
                        _ => {
                            return Err(Error::corruption(format!(
                                "unexpected page type in bulk delete: {:?}",
                                pt
                            )))
                        }
                    }
                }
                sticky = Some(page_id);
                match leaf_rowid_bounds(self.pager, page_id)? {
                    Some((a, b)) => {
                        lo = a;
                        hi = b;
                    }
                    None => {
                        // Empty leaf: the rowid cannot be here.
                        i += 1;
                        sticky = None;
                        continue;
                    }
                }
            }
            let leaf = sticky.expect("sticky pinned above");
            // Run of rowids that fall within the sticky leaf's bounds
            // (rowids are ascending; the leaf partition guarantees they
            // all live in THIS leaf).
            // partition_point returns an index RELATIVE to rowids[i..] —
            // convert to the absolute index before slicing.
            let run_end = i + rowids[i..].partition_point(|&r| r <= hi);
            if run_end == i {
                // Defensive: no progress — force a re-descend.
                sticky = None;
                continue;
            }
            // WHOLE-LEAF fast case: the run covers every cell of the leaf
            // (equal length + matching endpoints — the scan collected
            // existing rowids, so equal cardinality in the bounds implies
            // identity). One lock + one header write replaces n_cells
            // binary searches + pointer-array memmoves. This is the
            // dominant shape of mass range deletes: `DELETE WHERE id > K`.
            let n_cells = {
                let page = self.pager.get_page(leaf)?;
                let n = page.lock().n_cells() as usize;
                n
            };
            let run_len = run_end - i;
            if run_len == n_cells && n_cells > 0 && rowids[i] == lo && rowids[run_end - 1] == hi {
                // Spilled cells: free their overflow chains BEFORE the
                // cells go away (the chain pages would otherwise be
                // unreachable garbage). Same decode as delete_from_page.
                {
                    let page = self.pager.get_page(leaf)?;
                    let borrowed = page.lock();
                    let psz = borrowed.data.len();
                    for ci in 0..n_cells {
                        let cell_ptr = borrowed.cell_pointer(ci as u16) as usize;
                        let Ok(buf) = borrowed.cell_slice_checked(cell_ptr) else {
                            continue;
                        };
                        let Some((_, n_rid)) = varint::decode_signed(buf) else {
                            continue;
                        };
                        let Some((plen, n_plen)) = varint::decode(&buf[n_rid..]) else {
                            continue;
                        };
                        let local_len = overflow_local_len_for(plen as usize, psz);
                        if local_len < plen as usize {
                            let chain_off = n_rid + n_plen + local_len;
                            if chain_off + 4 <= buf.len() {
                                let chain = u32::from_be_bytes(
                                    buf[chain_off..chain_off + 4].try_into().unwrap_or([0; 4]),
                                );
                                if chain != 0 {
                                    spill_chains.push(chain);
                                }
                            }
                        }
                    }
                }
                for chain in spill_chains.drain(..) {
                    self.free_overflow_chain(chain)?;
                }
                {
                    let page = self.pager.get_page(leaf)?;
                    let mut borrowed = page.lock();
                    borrowed.set_n_cells(0);
                    borrowed.touch();
                }
                self.pager.note_dirty(leaf);
                deleted += n_cells as u64;
                // UNLINK + FREE the emptied leaf (SQLite's mass-delete
                // shape): the parent's separator cell is removed, the page
                // is freed, and — when the parent loses its LAST child —
                // the cascade recurses upward freeing childless interiors
                // so the file's tail pages all reclaim (tail truncation
                // below). Without the cascade, emptied interior orphans
                // sit above the freed leaves and block truncation.
                self.unlink_child_cascade(&path, leaf)?;
                sticky = None;
                i = run_end;
                continue;
            }
            // Partial run: per-cell deletes within this leaf.
            for &r in &rowids[i..run_end] {
                if self.delete_from_page(leaf, r)? {
                    deleted += 1;
                }
            }
            match leaf_rowid_bounds(self.pager, leaf)? {
                Some((a, b)) => {
                    lo = a;
                    hi = b;
                }
                None => sticky = None,
            }
            i = run_end;
        }
        // FILE-TAIL TRUNCATION: if the freed pages form a contiguous suffix
        // of the file (the append-insert + delete-tail shape — page ids
        // grow monotonically with sequential inserts, so freed tail leaves
        // are exactly the file's highest pages), drop them entirely: the
        // file shrinks, no WAL frames are written for them, and they never
        // pollute the freelist. This is SQLite's own truncate-on-mass-
        // delete optimization.
        if !self.freed_during_op.is_empty() {
            let _k = self.pager.truncate_tail(&self.freed_during_op)?;
        }
        self.collect_frees = false;
        self.freed_during_op.clear();
        Ok(deleted)
    }

    fn delete_from_page(&mut self, page_id: PageId, rowid: i64) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::Overflow => Err(Error::corruption(
                "overflow page reached as a btree node in delete_from_page",
            )),
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
                            let psz = borrowed.page_size();
                            match decode_index_cell(
                                borrowed.cell_slice_checked(cell_ptr)?,
                                false,
                                psz,
                            ) {
                                Some(v) => v.rowid,
                                None => return Err(Error::corruption("truncated index cell")),
                            }
                        } else {
                            match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
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
                        let psz = borrowed.page_size();
                        decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz)
                            .map(|v| v.rowid == rowid)
                            .unwrap_or(false)
                    } else {
                        varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?)
                            .map(|(k, _)| k == rowid)
                            .unwrap_or(false)
                    }
                };
                if !key_matches {
                    return Ok(false);
                }
                // Spilled row: free its overflow chain BEFORE removing the
                // cell (the chain pages are otherwise unreachable garbage).
                if pt == PageType::LeafTable {
                    let cell_ptr = {
                        let borrowed = page.lock();
                        borrowed.cell_pointer(pos) as usize
                    };
                    let cell = {
                        let borrowed = page.lock();
                        Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?
                    };
                    if let Cell::TableLeafOverflow { overflow, .. } = cell {
                        self.free_overflow_chain(overflow)?;
                    }
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
                    borrowed.touch();
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
                        // Only the separator key is needed for the binary
                        // search; the child pointer is re-decoded once the
                        // position is found below, so skip it here.
                        let sep = if pt == PageType::InteriorIndex {
                            let psz = borrowed.page_size();
                            match decode_index_cell(
                                borrowed.cell_slice_checked(cell_ptr)?,
                                true,
                                psz,
                            ) {
                                Some(v) => v.rowid,
                                None => {
                                    return Err(Error::corruption("truncated index interior cell"))
                                }
                            }
                        } else {
                            match decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                            {
                                Some(k) => k,
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
                            let psz = borrowed.page_size();
                            decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, true, psz)
                                .map(|v| v.left_child)
                                .unwrap_or_else(|| borrowed.right_most_pointer())
                        } else {
                            decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                                .unwrap_or_else(|| borrowed.right_most_pointer())
                        }
                    }
                };
                drop(page);
                if child_id == 0 {
                    // Unset rightmost on an emptied interior: not found
                    // (page 0 is the header/schema page, never a child).
                    return Ok(false);
                }
                let deleted = self.delete_from_page(child_id, rowid)?;
                if deleted {
                    // A leaf that just became empty is unlinked from this
                    // interior page and pushed onto the pager freelist, so
                    // future inserts reuse it instead of growing the file
                    // (mirrors SQLite's freelist; without it, delete-heavy
                    // churn grows the file forever).
                    let _ = self.maybe_recycle_empty_child(page_id, child_id)?;
                }
                Ok(deleted)
            }
        }
    }

    // Delete a cell from an interior page by index (used during B+tree
    // maintenance when a child splits and we need to replace a cell).
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
    //         borrowed.touch();
    //     }
    //     let _ = pt;
    //     Ok(())
    // }

    /// Scan all rows in a table B+tree, calling `f(rowid, payload)` for each.
    /// Stops early if `f` returns false.
    pub fn scan_table<F: FnMut(i64, &[u8]) -> bool>(&mut self, mut f: F) -> Result<()> {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        self.scan_subtree(self.root, &mut f).map(|_| ())
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
    /// `may_reenter`: does the visitor callback possibly call back into
    /// the pager (a correlated subquery inside a filter predicate, a
    /// table-valued function, ...)? Two leaf disciplines:
    /// - `false` (the default for every pure decode/transform visitor):
    ///   ZERO-COPY — the callback borrows payload bytes straight from
    ///   the page buffer under the leaf lock, exactly as before.
    /// - `true`: DEFERRED — cell payloads are batch-copied under the
    ///   lock and the callback runs lock-free (a re-entrant get_page on
    ///   this leaf would otherwise deadlock on the held Mutex; found by
    ///   the nested-correlation probe).
    pub fn scan_table_borrowed_opts<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        mut f: F,
        may_reenter: bool,
    ) -> Result<()> {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        if may_reenter {
            // ONE batch for the whole scan: the buffers grow on the
            // first leaf and are reused by every subsequent leaf.
            let mut batch = ScanBatch::default();
            self.scan_subtree_borrowed(self.root, &mut f, &mut batch)?;
            Ok(())
        } else {
            self.scan_subtree_borrowed_fast(self.root, &mut f)
                .map(|_| ())
        }
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
    /// back into the pager (no `get_page`, no `allocate_page`) — callers
    /// whose visitor evaluates a CORRELATED SUBQUERY (which re-enters the
    /// pager on this very leaf when the subquery scans the same table)
    /// must use `scan_table_borrowed_opts(_, true)` instead.
    pub fn scan_table_borrowed<F: FnMut(i64, &[u8]) -> bool>(&mut self, f: F) -> Result<()> {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        self.scan_table_borrowed_opts(f, false)
    }

    /// Returns `Ok(false)` when the visitor stopped the scan early —
    /// interior nodes propagate the stop so `false` unwinds the whole
    /// tree walk instead of visiting every remaining leaf.
    /// The ZERO-COPY leaf discipline (see `scan_table_borrowed_opts`):
    /// the visitor runs under the leaf lock, borrowing payload bytes
    /// straight from the page buffer. Only for visitors that CANNOT
    /// re-enter the pager.
    fn scan_subtree_borrowed_fast<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        f: &mut F,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
                // Single lock for the whole leaf iteration. Inside the
                // lock we read cell pointers and slice payload bytes
                // directly into the page's data buffer — no allocation.
                let borrowed = page.lock();
                prefetch_search_lines(&borrowed.data);
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
                    let buf = borrowed.cell_slice_checked(cell_ptr)?;
                    let (rowid, n1) = varint::decode_signed(buf).ok_or_else(|| {
                        Error::corruption("truncated leaf rowid in scan_borrowed")
                    })?;
                    let rest = &buf[n1..];
                    let (plen, n2) = varint::decode(rest).ok_or_else(|| {
                        Error::corruption("truncated leaf payload len in scan_borrowed")
                    })?;
                    let payload_start = n1 + n2;
                    let plen = plen as usize;
                    let page_size = psz;
                    let local_len = overflow_local_len_for(plen, page_size);
                    if local_len == plen {
                        // In-page payload. Corrupt files can carry huge
                        // payload-length varints: checked add, not
                        // `payload_start + plen` (which overflows usize and
                        // panics in debug builds).
                        if plen
                            .checked_add(payload_start)
                            .map_or(true, |end| end > buf.len())
                        {
                            return Err(Error::corruption(
                                "truncated leaf payload in scan_borrowed",
                            ));
                        }
                        let payload = &buf[payload_start..payload_start + plen];
                        if !f(rowid, payload) {
                            return Ok(false);
                        }
                    } else {
                        // Overflow cell: local prefix + 4-byte chain head.
                        // Assemble the payload while still holding this
                        // leaf's lock — the lock order leaf → overflow is
                        // global (chains are only entered from leaf cells).
                        if local_len + 4 > buf.len() - payload_start {
                            return Err(Error::corruption(
                                "truncated overflow cell in scan_borrowed",
                            ));
                        }
                        let local = &buf[payload_start..payload_start + local_len];
                        let chain = u32::from_be_bytes(
                            buf[payload_start + local_len..payload_start + local_len + 4]
                                .try_into()
                                .unwrap(),
                        );
                        // Reuse one assembly buffer across all overflow
                        // rows in this scan. The callback receives the
                        // buffer's contents and must NOT retain the slice
                        // (it is reused for the next row) — the scan
                        // callbacks copy or decode synchronously, so this
                        // is safe.
                        thread_local! {
                            static ASSEMBLE_BUF: std::cell::RefCell<Vec<u8>> =
                                std::cell::RefCell::new(Vec::with_capacity(1 << 16));
                        }
                        let cont = ASSEMBLE_BUF.with(|b| {
                            let mut buf = b.borrow_mut();
                            self.assemble_overflow_payload_into(local, plen as u64, chain, &mut buf)
                                .map(|_| f(rowid, &buf))
                        })?;
                        if !cont {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            }
            PageType::InteriorTable => {
                let n = page.lock().n_cells();
                let right = page.lock().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    if right != 0 {
                        v.push(right);
                    }
                    v
                };
                drop(page);
                for child in cells {
                    if !self.scan_subtree_borrowed_fast(child, f)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in scan_borrowed: {:?}",
                pt
            ))),
        }
    }

    fn scan_subtree_borrowed<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        f: &mut F,
        batch: &mut ScanBatch,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
                // DEFERRED-visitor leaf: phase 1 copies every cell's
                // payload bytes into the batch under ONE lock (no
                // allocation, no visitor code running); phase 2 runs the
                // visitor with the lock RELEASED. The old design called
                // the visitor under the lock (zero-copy borrows into the
                // page buffer) — which DEADLOCKED any re-entrant scan of
                // the same table: a correlated subquery evaluated inside
                // the visitor re-enters get_page on this very leaf and
                // blocks on the held Mutex forever (nested-correlation
                // probe). The per-leaf batch copy (~1 memcpy of the
                // leaf's payload bytes, reused buffer) is the price of
                // re-entrancy safety; the visitor contract is unchanged
                // (payload slices must not be retained past the call —
                // the same contract the overflow path always had).
                {
                    batch.clear();
                    let borrowed = page.lock();
                    prefetch_search_lines(&borrowed.data);
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
                        // copy the payload bytes into the batch.
                        let buf = borrowed.cell_slice_checked(cell_ptr)?;
                        let (rowid, n1) = varint::decode_signed(buf).ok_or_else(|| {
                            Error::corruption("truncated leaf rowid in scan_borrowed")
                        })?;
                        let rest = &buf[n1..];
                        let (plen, n2) = varint::decode(rest).ok_or_else(|| {
                            Error::corruption("truncated leaf payload len in scan_borrowed")
                        })?;
                        let payload_start = n1 + n2;
                        let plen = plen as usize;
                        let page_size = psz;
                        let local_len = overflow_local_len_for(plen, page_size);
                        if local_len == plen {
                            // In-page payload. Corrupt files can carry huge
                            // payload-length varints: checked add, not
                            // `payload_start + plen` (which overflows usize
                            // and panics in debug builds).
                            if plen
                                .checked_add(payload_start)
                                .map_or(true, |end| end > buf.len())
                            {
                                return Err(Error::corruption(
                                    "truncated leaf payload in scan_borrowed",
                                ));
                            }
                            batch.push_in_page(rowid, &buf[payload_start..payload_start + plen]);
                        } else {
                            // Overflow cell: local prefix + 4-byte chain
                            // head, copied now; the chain is assembled in
                            // phase 2 (no leaf lock held — a nested scan
                            // assembling the same chain would deadlock on
                            // the overflow pages otherwise).
                            if local_len + 4 > buf.len() - payload_start {
                                return Err(Error::corruption(
                                    "truncated overflow cell in scan_borrowed",
                                ));
                            }
                            let local = &buf[payload_start..payload_start + local_len];
                            let chain = u32::from_be_bytes(
                                buf[payload_start + local_len..payload_start + local_len + 4]
                                    .try_into()
                                    .unwrap(),
                            );
                            batch.push_overflow(rowid, local, plen as u64, chain);
                        }
                    }
                }
                drop(page);
                batch.visit(self, f)
            }
            PageType::InteriorTable => {
                let n = page.lock().n_cells();
                let right = page.lock().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    // 0 = "no rightmost child" (an interior page keeps
                    // this after a split); descending into 0 would scan the
                    // header/schema-root page and yield its row as a table
                    // row. Range scans already guard this; these full-scan
                    // and count paths must too.
                    if right != 0 {
                        v.push(right);
                    }
                    v
                };
                drop(page);
                for child in cells {
                    if !self.scan_subtree_borrowed(child, f, batch)? {
                        return Ok(false);
                    }
                }
                Ok(true)
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

    /// Count rows with rowid in [start, end] WITHOUT decoding any cell
    /// payloads — the rowid range analog of `count_rows`. Mirrors the
    /// page-descent structure of `scan_range_subtree_borrowed`: binary
    /// search each leaf for the first rowid >= start, then count cells
    /// until the first rowid > end. For `SELECT COUNT(*) FROM t WHERE
    /// id BETWEEN ? AND ?` this turns a full materialize-and-count of N
    /// rows (N payload decodes + N Value allocs + the row Vec) into
    /// ~2 binary searches per leaf page.
    pub fn count_rows_range(&mut self, start: i64, end: i64) -> Result<u64> {
        self.count_range_subtree(self.root, start, end)
    }

    // ================================================================
    // OVERFLOW-AWARE SELECTIVE SCAN (the lazy payload contract)
    // ================================================================

    /// Append `payload[start..end]` (offsets into the FULL payload) to
    /// `out`, gathering from the local prefix and the overflow chain ONLY
    /// the pages that actually overlap the range. A projection that never
    /// reads a wide column's bytes never walks the chain at all.
    ///
    /// `chain` is the first overflow page (0 when the payload is fully
    /// local — then only `local` is consulted). Chain pages hold
    /// `payload[local_len..total]` sequentially.
    pub(crate) fn gather_payload_range_append(
        &mut self,
        local: &[u8],
        total: usize,
        chain: PageId,
        start: usize,
        end: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let end = end.min(total);
        if start >= end {
            return Ok(());
        }
        // Part served by the local prefix.
        let local_part_end = end.min(local.len());
        if start < local_part_end {
            out.extend_from_slice(&local[start..local_part_end]);
            if local_part_end == end {
                return Ok(());
            }
        }
        if chain == 0 {
            // No chain: a range past the local prefix on a fully-local
            // payload is a corrupt length header.
            return Err(Error::corruption(format!(
                "payload range [{}, {}) past local prefix with no overflow chain",
                start, end
            )));
        }
        // Chain part: ONE cache read-guard for the whole resident run
        // (see Pager::gather_overflow_chain_range) instead of a
        // `get_page` (RwLock + Arc clone + atomic) per chain page.
        let _ = self
            .pager
            .gather_overflow_chain_range(chain, local.len(), start, end, out)?;
        Ok(())
    }

    /// Full-scan with the projection FUSED into the walk (the overflow-
    /// aware selective decode). For every row the callback receives the
    /// rowid plus an OWNED `Vec<Value>` holding exactly `wanted` columns
    /// in the caller's order (duplicates included, rowid-alias
    /// materialized, short rows NULL-padded).
    ///
    /// vs `scan_table_borrowed` + `decode_row_selective`:
    /// - In-page rows: identical selective decode, zero assembly.
    /// - Overflow rows: ONLY the wanted columns' byte ranges are gathered
    ///   from the chain — `SELECT id FROM blobs` reads zero chain pages
    ///   (the old contract assembled every 64 KB payload first), and a
    ///   projected wide column is copied from its chain pages DIRECTLY
    ///   into the value's own buffer (one copy, not assemble+copy).
    ///
    /// The walk hands each row to `f`; `false` stops the scan.
    pub fn scan_table_selective<F>(
        &mut self,
        n_cols: usize,
        wanted: &[usize],
        rowid_alias: Option<usize>,
        f: F,
    ) -> Result<()>
    where
        F: FnMut(i64, Vec<Value>) -> bool,
    {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        self.scan_table_range_selective(i64::MIN, i64::MAX, n_cols, wanted, rowid_alias, f)
    }

    /// Rowid-range variant of `scan_table_selective` (resumable streaming:
    /// the statement driver resumes each batch at `last_rowid + 1`).
    /// Inclusive bounds on both ends; cells sorted by rowid, so the first
    /// rowid past `end` stops the walk.
    pub fn scan_table_range_selective<F>(
        &mut self,
        start: i64,
        end: i64,
        n_cols: usize,
        wanted: &[usize],
        rowid_alias: Option<usize>,
        f: F,
    ) -> Result<()>
    where
        F: FnMut(i64, Vec<Value>) -> bool,
    {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        // Pool-free entry (executor fused walks + parallel workers):
        // an empty scratch vec behaves exactly like the fresh-allocation
        // contract always did.
        let mut no_pool: Vec<Vec<Value>> = Vec::new();
        self.scan_table_range_selective_pooled(
            start,
            end,
            n_cols,
            wanted,
            rowid_alias,
            &mut no_pool,
            f,
        )
    }

    /// [`Self::scan_table_range_selective`] with a caller-owned ROW POOL:
    /// every in-page row handed to `f` is popped from `pool` (cleared,
    /// capacity retained) instead of freshly allocated, and the caller
    /// returns rows to the pool after their consumer is done with them.
    /// The streaming `Statement::step` path recycles its served rows
    /// this way — one heap allocation per ROW SLOT for the whole
    /// statement lifetime instead of one per row (the torture-S06 shape:
    /// 100k-row range drains cost ~30% of their step time in small-alloc
    /// churn).
    pub fn scan_table_range_selective_pooled<F>(
        &mut self,
        start: i64,
        end: i64,
        n_cols: usize,
        wanted: &[usize],
        rowid_alias: Option<usize>,
        row_pool: &mut Vec<Vec<Value>>,
        mut f: F,
    ) -> Result<()>
    where
        F: FnMut(i64, Vec<Value>) -> bool,
    {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        // Trusted TEXT decode for in-memory pagers (payloads are this
        // process's own encoder output — see types::value). Saved +
        // restored so nested scans on other pagers behave correctly.
        let saved_trust = crate::types::value::text_decode_trusted();
        let trusted = self.pager.is_memory();
        if trusted != saved_trust {
            crate::types::value::set_text_decode_trusted(trusted);
        }
        let r = self
            .scan_range_subtree_selective(
                self.root,
                start,
                end,
                n_cols,
                wanted,
                rowid_alias,
                row_pool,
                &mut f,
            )
            .map(|_| ());
        if trusted != saved_trust {
            crate::types::value::set_text_decode_trusted(saved_trust);
        }
        r
    }

    /// Raw-record cell scan: the rowid-range twin of
    /// [`Self::scan_table_range_selective_pooled`] that copies each cell's
    /// record BYTES into `arena` and notes `(rowid, off, len)` entries —
    /// NO value decoding here. The statement's step path decodes each row
    /// into ONE reused serve buffer (see `statement.rs`'s cell mode), so a
    /// 100k-row range drain pays a per-row record memcpy + one fixed
    /// decode instead of pool-pop / per-row Vec / VecDeque / pool-return
    /// churn. Overflow cells are reassembled into the arena (rare, and
    /// bounded by `max_arena_bytes`).
    ///
    /// Resume contract mirrors the pooled scan: stops at `budget` entries
    /// or `max_arena_bytes` cumulative arena bytes WITHOUT copying the
    /// stopped-on cell; `last_rowid` is the last COPIED row's rowid (the
    /// next call passes `start = last_rowid + 1`, which lands ON the
    /// stopped row). `hit_end` stays true when the scan ran off the range
    /// end or EOF (caller's eof = `hit_end && entries.len() < budget`).
    pub fn scan_table_range_cells(
        &mut self,
        start: i64,
        end: i64,
        budget: usize,
        max_arena_bytes: usize,
        arena: &mut Vec<u8>,
        entries: &mut Vec<CellEntry>,
    ) -> Result<CellScanOutcome> {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        // Caller clears per pull; keep the invariant local so a stale
        // caller cannot corrupt offsets.
        arena.clear();
        entries.clear();
        let mut walk = CellsWalk {
            arena,
            entries,
            budget: budget.max(1),
            max_bytes: max_arena_bytes.max(1),
            hit_end: true,
            overflow_stop: false,
            last: i64::MIN,
        };
        self.scan_range_subtree_cells(self.root, start, end, &mut walk)?;
        Ok(CellScanOutcome {
            hit_end: walk.hit_end,
            overflow_stop: walk.overflow_stop,
            last_rowid: walk.last,
        })
    }

    fn scan_range_subtree_cells(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        walk: &mut CellsWalk<'_>,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let borrowed = page.lock();
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                // Binary search for the first cell with rowid >= start
                // (same resumable-batch skip as the selective scan).
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                        Some((rowid, _)) if rowid < start => lo = mid + 1,
                        _ => hi = mid,
                    }
                }
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    let buf = borrowed.cell_slice_checked(cell_ptr)?;
                    let (rowid, n1) = varint::decode_signed(buf)
                        .ok_or_else(|| Error::corruption("truncated leaf rowid in scan_cells"))?;
                    if rowid > end {
                        // Sorted: everything after is past the range too
                        // (hit_end stays true — the RANGE ended, not the
                        // budget).
                        return Ok(false);
                    }
                    let rest = &buf[n1..];
                    let (plen, n2) = varint::decode(rest).ok_or_else(|| {
                        Error::corruption("truncated leaf payload len in scan_cells")
                    })?;
                    let payload_start = n1 + n2;
                    let plen = plen as usize;
                    // Budget stop BEFORE copying: the resume (last+1) must
                    // land ON this row, so it must NOT be in this batch.
                    if walk.full() {
                        walk.hit_end = false;
                        return Ok(false);
                    }
                    if plen > u32::MAX as usize {
                        // A >4 GiB record cannot be represented by the
                        // u32 (off, len) entry contract — and no valid
                        // engine/SQlite row is that large. Corruption.
                        return Err(Error::corruption(format!(
                            "record payload {} too large for cell serving",
                            plen
                        )));
                    }
                    let local_len = overflow_local_len_for(plen, psz);
                    if local_len == plen {
                        // In-page payload: one slice copy into the arena.
                        if payload_start
                            .checked_add(plen)
                            .map_or(true, |end| end > buf.len())
                        {
                            return Err(Error::corruption("truncated leaf payload in scan_cells"));
                        }
                        let payload = &buf[payload_start..payload_start + plen];
                        walk.copy(rowid, payload);
                    } else {
                        // Overflow cell: STOP the walk without copying it
                        // (see CellScanOutcome::overflow_stop). The
                        // materialized path's decode_overflow_selective
                        // gathers only the WANTED column spans directly
                        // into the value's own buffer — ONE payload copy;
                        // arena-copying would pay chain reassembly +
                        // arena + decode (three). The resume
                        // (last_rowid + 1) lands ON this row.
                        walk.hit_end = false;
                        walk.overflow_stop = true;
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let key = decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                        .ok_or_else(|| {
                            Error::corruption("truncated interior cell in cells scan")
                        })?;
                    if key < start {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let mut children: Vec<PageId> = Vec::with_capacity((n - lo) as usize + 1);
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if let Some(child) =
                        decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                    {
                        children.push(child);
                    }
                }
                if right != 0 {
                    children.push(right);
                }
                drop(borrowed);
                drop(page);
                for child in children {
                    if !self.scan_range_subtree_cells(child, start, end, walk)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in scan_cells: {:?}",
                pt
            ))),
        }
    }

    fn scan_range_subtree_selective<F>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        n_cols: usize,
        wanted: &[usize],
        rowid_alias: Option<usize>,
        row_pool: &mut Vec<Vec<Value>>,
        f: &mut F,
    ) -> Result<bool>
    where
        F: FnMut(i64, Vec<Value>) -> bool,
    {
        let page = self.pager.get_page(page_id)?;
        // ONE lock per page: type check + leaf/interior work under the
        // same guard (the leaf cell loop borrows payload slices from it).
        let borrowed = page.lock();
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                // Binary search for the first cell with rowid >= start
                // (mirrors scan_range_subtree_borrowed — resumable batches
                // with `start = last_rowid + 1` skip the consumed prefix).
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                        Some((rowid, _)) if rowid < start => lo = mid + 1,
                        _ => hi = mid,
                    }
                }
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    let buf = borrowed.cell_slice_checked(cell_ptr)?;
                    let (rowid, n1) = varint::decode_signed(buf).ok_or_else(|| {
                        Error::corruption("truncated leaf rowid in scan_selective")
                    })?;
                    if rowid > end {
                        // Sorted: everything after is past the range too.
                        return Ok(false);
                    }
                    let rest = &buf[n1..];
                    let (plen, n2) = varint::decode(rest).ok_or_else(|| {
                        Error::corruption("truncated leaf payload len in scan_selective")
                    })?;
                    let payload_start = n1 + n2;
                    let plen = plen as usize;
                    let local_len = overflow_local_len_for(plen, psz);
                    if local_len == plen {
                        // In-page payload: the existing selective decoder
                        // directly over the page bytes (zero-copy slice).
                        if payload_start
                            .checked_add(plen)
                            .map_or(true, |end| end > buf.len())
                        {
                            return Err(Error::corruption(
                                "truncated leaf payload in scan_selective",
                            ));
                        }
                        let payload = &buf[payload_start..payload_start + plen];
                        let mut row: Vec<Value> = match row_pool.pop() {
                            Some(mut r) => {
                                r.clear();
                                r
                            }
                            None => Vec::with_capacity(wanted.len().max(1)),
                        };
                        // An undecodable in-page record is corruption:
                        // the walk fails (it used to skip the row).
                        crate::storage::row_codec::decode_row_selective(
                            payload,
                            n_cols,
                            wanted,
                            rowid,
                            rowid_alias,
                            &mut row,
                        )?;
                        if !f(rowid, row) {
                            return Ok(false);
                        }
                    } else {
                        // Overflow cell: decode wanted column ranges from
                        // the local prefix; gather only what extends past.
                        if local_len + 4 > buf.len() - payload_start {
                            return Err(Error::corruption(
                                "truncated overflow cell in scan_selective",
                            ));
                        }
                        let local = &buf[payload_start..payload_start + local_len];
                        let chain = u32::from_be_bytes(
                            buf[payload_start + local_len..payload_start + local_len + 4]
                                .try_into()
                                .unwrap(),
                        );
                        let row = self.decode_overflow_selective(
                            local,
                            plen,
                            chain,
                            n_cols,
                            wanted,
                            rowid,
                            rowid_alias,
                        );
                        match row {
                            Ok(row) => {
                                if !f(rowid, row) {
                                    return Ok(false);
                                }
                            }
                            Err(crate::storage::row_codec::LazyError::HeaderPastLocal) => {
                                // Pathological: the record header itself
                                // spills past the local prefix. Fall back
                                // to full assembly + the ordinary
                                // selective decode (identical semantics).
                                let mut full = Vec::new();
                                self.assemble_overflow_payload_into(
                                    local,
                                    plen as u64,
                                    chain,
                                    &mut full,
                                )?;
                                let mut row: Vec<Value> = match row_pool.pop() {
                                    Some(mut r) => {
                                        r.clear();
                                        r
                                    }
                                    None => Vec::with_capacity(wanted.len().max(1)),
                                };
                                crate::storage::row_codec::decode_row_selective(
                                    &full,
                                    n_cols,
                                    wanted,
                                    rowid,
                                    rowid_alias,
                                    &mut row,
                                )?;
                                if !f(rowid, row) {
                                    return Ok(false);
                                }
                            }
                            Err(crate::storage::row_codec::LazyError::Corrupt(e)) => {
                                return Err(e);
                            }
                        }
                    }
                }
                Ok(true)
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                // Binary search for the FIRST child whose separator >=
                // start (children with separator < start hold only rowids
                // < start): resumable batches skip the consumed prefix
                // instead of re-locking every leaf below `start`.
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let key = decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                        .ok_or_else(|| {
                            Error::corruption("truncated interior cell in selective scan")
                        })?;
                    if key < start {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let mut children: Vec<PageId> = Vec::with_capacity((n - lo) as usize + 1);
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if let Some(child) =
                        decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                    {
                        children.push(child);
                    }
                }
                // 0 = unset rightmost (post-split); descending into 0
                // would scan the header/schema page (phantom rows).
                if right != 0 {
                    children.push(right);
                }
                drop(borrowed);
                drop(page);
                for child in children {
                    if !self.scan_range_subtree_selective(
                        child,
                        start,
                        end,
                        n_cols,
                        wanted,
                        rowid_alias,
                        row_pool,
                        f,
                    )? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in scan_selective: {:?}",
                pt
            ))),
        }
    }

    /// Decode the wanted columns of one OVERFLOW cell without assembling
    /// the payload: walk the value tags on the local prefix, decode wanted
    /// columns that fit locally, and gather (directly into the value's own
    /// buffer) only the ranges that spill into the chain.
    #[allow(clippy::too_many_arguments)]
    fn decode_overflow_selective(
        &mut self,
        local: &[u8],
        total: usize,
        chain: PageId,
        n_cols: usize,
        wanted: &[usize],
        rowid: i64,
        rowid_alias: Option<usize>,
    ) -> std::result::Result<Vec<Value>, crate::storage::row_codec::LazyError> {
        use crate::storage::row_codec::{column_spans_local, decode_span_value, LazyError};

        // 1. Layout walk on the local prefix (spans up to the highest
        //    wanted column; None = short-row column past the payload).
        let max_col = wanted.iter().copied().max().unwrap_or(0);
        let spans = column_spans_local(local, total, n_cols, max_col)?;

        // 2. Resolve each output slot directly from its column's span —
        //    no single-pass walk needed: the spans table gives O(1)
        //    per-column lookups, duplicates re-decode locally (cheap),
        //    and the SELECT order is preserved by construction.
        let mut out: Vec<Value> = vec![Value::Null; wanted.len()];
        for (slot, &col) in wanted.iter().enumerate() {
            if col >= n_cols {
                continue; // stays NULL (defensive: schema walk guarantees < n_cols)
            }
            if rowid_alias == Some(col) {
                out[slot] = Value::Integer(rowid);
                continue;
            }
            match spans.get(col).copied().flatten() {
                None => {} // short row: NULL
                Some(sp) if sp.end() <= local.len() => {
                    // Fully local: decode straight from the page bytes.
                    out[slot] = decode_span_value(&local[sp.off as usize..sp.end()])
                        .map_err(LazyError::Corrupt)?;
                }
                Some(sp) => {
                    // Spills into the chain: gather the byte range DIRECTLY
                    // into the value's own buffer (single copy).
                    out[slot] = self
                        .decode_span_gathered(sp, local, total, chain)
                        .map_err(LazyError::Corrupt)?;
                }
            }
        }
        Ok(out)
    }

    /// Decode one value whose byte span spills into the overflow chain,
    /// copying bytes DIRECTLY into the value's own buffer: for a 64 KB
    /// blob this is ONE copy (chain pages -> the Vec backing the Value),
    /// vs the old assemble-then-decode's two. Text and Blob bodies are
    /// gathered page-by-page straight into the final allocation; the tag
    /// + length header (always page-local) is parsed separately.
    fn decode_span_gathered(
        &mut self,
        span: crate::storage::row_codec::ColSpan,
        local: &[u8],
        total: usize,
        chain: PageId,
    ) -> Result<Value> {
        use crate::types::value::decode_uvarint;

        let off = span.off as usize;
        // The span's tag byte and length header are guaranteed local
        // (`column_spans_local` only returns spans whose headers fit).
        let tag = local[off];
        let rest = &local[off + 1..];
        match tag {
            0x08 => {
                // Blob: [tag][varint len][body]. Gather the body straight
                // into the Value's own Vec — zero intermediate copies.
                let (len, n) = decode_uvarint(rest)
                    .map_err(|e| Error::corruption(format!("lazy blob header: {}", e)))?;
                let body_start = off + 1 + n;
                let body_end = body_start + len as usize;
                if body_end > total {
                    return Err(Error::corruption("lazy blob body past payload end"));
                }
                let mut v = Vec::with_capacity(len as usize);
                self.gather_payload_range_append(
                    local, total, chain, body_start, body_end, &mut v,
                )?;
                if v.len() != len as usize {
                    return Err(Error::corruption("lazy blob gather short"));
                }
                Ok(Value::Blob(v))
            }
            0x0B => {
                // TEXT holding invalid UTF-8 (the value codec's raw tag).
                let (len, n) = decode_uvarint(rest)
                    .map_err(|e| Error::corruption(format!("lazy text header: {}", e)))?;
                let body_start = off + 1 + n;
                let body_end = body_start + len as usize;
                if body_end > total {
                    return Err(Error::corruption("lazy text body past payload end"));
                }
                let mut v = Vec::with_capacity(len as usize);
                self.gather_payload_range_append(
                    local, total, chain, body_start, body_end, &mut v,
                )?;
                if v.len() != len as usize {
                    return Err(Error::corruption("lazy text gather short"));
                }
                Ok(Value::Text(crate::types::text::Text::from_invalid_bytes(
                    &v,
                )))
            }
            0x07 => {
                // Text: same single-copy gather, then UTF-8 validation
                // (String::from_utf8 reuses the buffer — no second copy).
                let (len, n) = decode_uvarint(rest)
                    .map_err(|e| Error::corruption(format!("lazy text header: {}", e)))?;
                let body_start = off + 1 + n;
                let body_end = body_start + len as usize;
                if body_end > total {
                    return Err(Error::corruption("lazy text body past payload end"));
                }
                let mut v = Vec::with_capacity(len as usize);
                self.gather_payload_range_append(
                    local, total, chain, body_start, body_end, &mut v,
                )?;
                if v.len() != len as usize {
                    return Err(Error::corruption("lazy text gather short"));
                }
                // Trusted mode (in-memory pagers) skips the redundant
                // validation pass — mirrors Value::decode's Text arm: the
                // payloads are this process's own encoder output, written
                // from String values (valid UTF-8 by construction). A
                // 2 KB body otherwise pays a full validation scan per row.
                if crate::types::value::text_decode_trusted() {
                    // SAFETY: trusted mode is armed only for in-memory
                    // pagers whose payloads this process encoded from
                    // String values — valid UTF-8 by construction.
                    Ok(Value::Text(
                        unsafe { String::from_utf8_unchecked(v) }.into(),
                    ))
                } else {
                    match String::from_utf8(v) {
                        Ok(s) => Ok(Value::Text(s.into())),
                        Err(_) => Err(Error::corruption("invalid utf8 in lazy text gather")),
                    }
                }
            }
            _ => {
                // Fixed-width value whose bytes straddle the boundary:
                // gather the whole span and decode through the generic
                // path (fixed types are <= 9 bytes; at most one chain
                // page is touched).
                let mut buf = Vec::with_capacity(span.len as usize);
                self.gather_payload_range_append(local, total, chain, off, span.end(), &mut buf)?;
                crate::storage::row_codec::decode_span_value(&buf)
            }
        }
    }

    fn count_range_subtree(&mut self, page_id: PageId, start: i64, end: i64) -> Result<u64> {
        let page = self.pager.get_page(page_id)?;
        let borrowed = page.lock();
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                // Binary search: first cell with rowid >= start.
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                        Some((rowid, _)) if rowid < start => lo = mid + 1,
                        _ => hi = mid,
                    }
                }
                // Count cells from lo while rowid <= end.
                let mut count: u64 = 0;
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                        Some((rowid, _)) => {
                            if rowid > end {
                                break;
                            }
                            count += 1;
                        }
                        None => {
                            return Err(Error::corruption("truncated leaf rowid in count_range"))
                        }
                    }
                }
                Ok(count)
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                // Binary search for the FIRST child whose separator >=
                // start; children to the left hold only rowids < start.
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let key = decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                        .ok_or_else(|| {
                            Error::corruption("truncated interior cell in count_range")
                        })?;
                    if key < start {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let mut children: Vec<PageId> = Vec::with_capacity((n - lo) as usize + 1);
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if let Some(child) =
                        decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                    {
                        children.push(child);
                    }
                }
                if right != 0 {
                    children.push(right);
                }
                drop(borrowed);
                drop(page);
                let mut total: u64 = 0;
                for child in children {
                    total += self.count_range_subtree(child, start, end)?;
                    // If any child reports zero rows in range AND we've
                    // already passed the range (children are in rowid
                    // order), the rest are past `end` — but the per-leaf
                    // early break already handles the common case; a zero
                    // count alone can also mean an empty sub-range, so we
                    // keep descending only while children may overlap.
                }
                Ok(total)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in count_range: {:?}",
                pt
            ))),
        }
    }

    fn count_subtree(&mut self, page_id: PageId) -> Result<u64> {
        let page = self.pager.get_page(page_id)?;
        let (pt, n, right) = {
            let borrowed = page.lock();
            (
                borrowed.page_type()?,
                borrowed.n_cells(),
                borrowed.right_most_pointer(),
            )
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
                        let c = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    // 0 = "no rightmost child" (an interior page keeps
                    // this after a split); descending into 0 would scan the
                    // header/schema-root page and yield its row as a table
                    // row. Range scans already guard this; these full-scan
                    // and count paths must too.
                    if right != 0 {
                        v.push(right);
                    }
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
    ) -> Result<bool> {
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
                    let cell = Cell::decode(
                        borrowed.cell_slice_checked(cell_ptr)?,
                        pt,
                        borrowed.page_size(),
                    )?;
                    match cell {
                        Cell::TableLeaf { rowid, payload } => {
                            if !f(rowid, &payload) {
                                return Ok(false);
                            }
                        }
                        Cell::TableLeafOverflow {
                            rowid,
                            local,
                            total,
                            overflow,
                            ..
                        } => {
                            let payload =
                                self.assemble_overflow_payload(&local, total, overflow)?;
                            if !f(rowid, &payload) {
                                return Ok(false);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(true)
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                let cells: Vec<PageId> = {
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    // 0 = "no rightmost child" (an interior page keeps
                    // this after a split); descending into 0 would scan the
                    // header/schema-root page and yield its row as a table
                    // row. Range scans already guard this; these full-scan
                    // and count paths must too.
                    if right != 0 {
                        v.push(right);
                    }
                    v
                };
                drop(borrowed);
                drop(page);
                for child in cells {
                    if !self.scan_subtree(child, f)? {
                        return Ok(false);
                    }
                }
                Ok(true)
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
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
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
        f: F,
    ) -> Result<()> {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        self.scan_table_range_borrowed_opts(start, end, f, false)
    }

    /// The range twin of `scan_table_borrowed_opts`: `may_reenter`
    /// visitors (correlated subqueries in range predicates) get the
    /// DEFERRED leaf discipline (batch copy under the lock, visitor
    /// lock-free); pure decode visitors keep the zero-copy fast path.
    pub fn scan_table_range_borrowed_opts<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        start: i64,
        end: i64,
        mut f: F,
        may_reenter: bool,
    ) -> Result<()> {
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        if may_reenter {
            let mut batch = ScanBatch::default();
            self.scan_range_subtree_borrowed_deferred(self.root, start, end, &mut f, &mut batch)?;
            Ok(())
        } else {
            self.scan_range_subtree_borrowed(self.root, start, end, &mut f)?;
            Ok(())
        }
    }

    /// DEFERRED-visitor range recursion (see `scan_table_borrowed_opts`
    /// for why the visitor must not run under the leaf lock).
    fn scan_range_subtree_borrowed_deferred<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        f: &mut F,
        batch: &mut ScanBatch,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let pt = page.lock().page_type()?;
        match pt {
            PageType::LeafTable => {
                batch.clear();
                let mut stop = false;
                // Phase 1: binary-search the range start + copy the
                // in-range cells under ONE lock (visitor never runs here).
                {
                    let borrowed = page.lock();
                    prefetch_search_lines(&borrowed.data);
                    let n = borrowed.n_cells();
                    let psz = borrowed.data.len();
                    let mut lo: u16 = 0;
                    let mut hi: u16 = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        if cell_ptr >= psz {
                            return Err(Error::corruption(format!(
                                "cell pointer {} out of range",
                                cell_ptr
                            )));
                        }
                        match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                            Some((rowid, _)) if rowid < start => lo = mid + 1,
                            _ => hi = mid,
                        }
                    }
                    for i in lo..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        if cell_ptr >= psz {
                            return Err(Error::corruption(format!(
                                "cell pointer {} out of range",
                                cell_ptr
                            )));
                        }
                        let buf = borrowed.cell_slice_checked(cell_ptr)?;
                        let (rowid, n1) = varint::decode_signed(buf).ok_or_else(|| {
                            Error::corruption("truncated leaf rowid in range_borrowed")
                        })?;
                        if rowid > end {
                            stop = true;
                            break;
                        }
                        if rowid >= start {
                            let rest = &buf[n1..];
                            let (plen, n2) = varint::decode(rest).ok_or_else(|| {
                                Error::corruption("truncated payload len in range_borrowed")
                            })?;
                            let payload_start = n1 + n2;
                            let plen_usize = plen as usize;
                            let local_len = overflow_local_len_for(plen_usize, psz);
                            if local_len == plen_usize {
                                if payload_start + plen_usize > buf.len() {
                                    return Err(Error::corruption(
                                        "truncated payload in range_borrowed",
                                    ));
                                }
                                batch.push_in_page(
                                    rowid,
                                    &buf[payload_start..payload_start + plen_usize],
                                );
                            } else {
                                if local_len + 4 > buf.len() - payload_start {
                                    return Err(Error::corruption(
                                        "truncated overflow cell in range_borrowed",
                                    ));
                                }
                                let local = &buf[payload_start..payload_start + local_len];
                                let chain = u32::from_be_bytes(
                                    buf[payload_start + local_len..payload_start + local_len + 4]
                                        .try_into()
                                        .unwrap(),
                                );
                                batch.push_overflow(rowid, local, plen, chain);
                            }
                        }
                    }
                }
                drop(page);
                // Phase 2: visitor with NO leaf lock held. The cells copied
                // from THIS leaf are visited even when it holds the range
                // end — returning before the visit (the old early `stop`
                // exit) silently skipped every in-range row of the last
                // leaf: a re-entrant `UPDATE t SET x = (correlated) WHERE
                // id <= 2` touched nothing.
                let cont = batch.visit(self, f)?;
                Ok(cont && !stop)
            }
            PageType::InteriorTable => {
                let n = page.lock().n_cells();
                let right = page.lock().right_most_pointer();
                let cells: Vec<PageId> = {
                    let borrowed = page.lock();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        let c = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
                        if let Cell::TableInterior { left_child, .. } = c {
                            v.push(left_child);
                        }
                    }
                    if right != 0 {
                        v.push(right);
                    }
                    v
                };
                drop(page);
                for child in cells {
                    if !self.scan_range_subtree_borrowed_deferred(child, start, end, f, batch)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in range_borrowed: {:?}",
                pt
            ))),
        }
    }

    /// FUSED scan+patch: walk the rowid range handing each cell's payload
    /// to the callback as a MUTABLE slice. Callback contract:
    ///   - return `true`  → keep scanning (whether or not the row was
    ///     patched; the caller tracks that in its own captured state);
    ///   - return `false` → stop the walk entirely.
    ///
    /// A cell whose payload lives on overflow pages CANNOT be patched
    /// through its local prefix — its rowid is pushed into
    /// `fallback_rowids` and the callback is NOT called for it (the
    /// caller processes those rows through the ordinary update path).
    ///
    /// Safety invariant: the caller may only OVERWRITE payload bytes with
    /// SAME-SIZE content (the UPDATE patch path guarantees this — any
    /// size change must go to the fallback path instead). Same-size
    /// in-place writes never move cells, split leaves, or invalidate the
    /// cell-pointer array, so the leaf iteration state stays valid
    /// throughout the walk. Patched pages are marked dirty exactly like
    /// every other in-cache page mutation.
    pub fn scan_table_range_patch<F: FnMut(i64, &mut [u8]) -> bool>(
        &mut self,
        start: i64,
        end: i64,
        mut f: F,
        fallback_rowids: &mut Vec<i64>,
    ) -> Result<()> {
        self.scan_range_subtree_patch(self.root, start, end, &mut f, fallback_rowids)?;
        Ok(())
    }

    fn scan_range_subtree_patch<F: FnMut(i64, &mut [u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        f: &mut F,
        fallback_rowids: &mut Vec<i64>,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let mut borrowed = page.lock();
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                // Binary search for the first cell with rowid >= start
                // (mirrors scan_range_subtree_borrowed).
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                        Some((rowid, _)) if rowid < start => lo = mid + 1,
                        _ => hi = mid,
                    }
                }
                let mut page_dirty = false;
                let mut stop = false;
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    // Phase A: parse the cell header (rowid, payload span,
                    // local length) under an immutable borrow that ENDS
                    // before the mutable payload borrow begins.
                    let parsed = {
                        let buf = borrowed.cell_slice_checked(cell_ptr)?;
                        let (rowid, n1) = varint::decode_signed(buf).ok_or_else(|| {
                            Error::corruption("truncated leaf rowid in range_patch")
                        })?;
                        if rowid > end {
                            // Cells are sorted: everything after is past
                            // the range too — stop the whole walk.
                            stop = true;
                            None
                        } else if rowid < start {
                            None
                        } else {
                            let (plen, n2) = varint::decode(&buf[n1..]).ok_or_else(|| {
                                Error::corruption("truncated payload len in range_patch")
                            })?;
                            let payload_start = n1 + n2;
                            let plen_usize = plen as usize;
                            let local_len = overflow_local_len_for(plen_usize, psz);
                            Some((rowid, payload_start, local_len, plen_usize))
                        }
                    };
                    if stop {
                        break;
                    }
                    let Some((rowid, payload_start, local_len, plen_usize)) = parsed else {
                        continue;
                    };
                    if local_len == plen_usize {
                        // Local (non-overflow) payload: hand the mutable
                        // region to the callback. NOTE: payload_start is
                        // CELL-relative (parsed from the cell slice that
                        // begins at cell_ptr) — the page-relative span is
                        // [cell_ptr + payload_start, + plen_usize).
                        let payload_start = cell_ptr + payload_start;
                        if payload_start + plen_usize > psz {
                            return Err(Error::corruption("truncated payload in range_patch"));
                        }
                        let payload = &mut borrowed.data[payload_start..payload_start + plen_usize];
                        let keep = f(rowid, payload);
                        // CONSERVATIVE dirty marking: the callback may have
                        // patched bytes even when it asks to stop; a page
                        // mutated without its dirty flag would silently
                        // miss the next flush.
                        page_dirty = true;
                        if !keep {
                            stop = true;
                            break;
                        }
                    } else {
                        // Overflow cell: the payload continues on overflow
                        // pages — cannot be patched in place here.
                        fallback_rowids.push(rowid);
                    }
                }
                if page_dirty {
                    borrowed.touch();
                }
                drop(borrowed);
                if page_dirty {
                    // Same bookkeeping every other page mutation does: the
                    // dirty flag for the flusher (set above, under the
                    // guard), note_dirty for the O(dirty) flush set.
                    self.pager.note_dirty(page_id);
                }
                Ok(!stop) // false = caller must not descend further right
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let key = decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                        .ok_or_else(|| {
                            Error::corruption("truncated interior cell in range_patch")
                        })?;
                    if key < start {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let mut children: Vec<PageId> = Vec::with_capacity((n - lo) as usize + 1);
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if let Some(child) =
                        decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                    {
                        children.push(child);
                    }
                }
                if right != 0 {
                    children.push(right);
                }
                drop(borrowed);
                drop(page);
                for child in children {
                    if !self.scan_range_subtree_patch(child, start, end, f, fallback_rowids)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(Error::corruption(format!(
                "unexpected page type in range patch scan: {:?}",
                pt
            ))),
        }
    }

    /// Walk the rowid range [start, end] in order. Returns Ok(false) when
    /// the walk should STOP (a leaf's smallest rowid exceeded `end`, or the
    /// callback asked to stop) — the caller must not descend into further
    /// right siblings. The old version visited every leaf to the right of
    /// the range, paying a page fetch + lock per leaf for a first-cell
    /// check (~500 ns on a 10k-row table for a 1-row range).
    fn scan_range_subtree_borrowed<F: FnMut(i64, &[u8]) -> bool>(
        &mut self,
        page_id: PageId,
        start: i64,
        end: i64,
        f: &mut F,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        // ONE lock per page: type check + leaf work under the same guard.
        let borrowed = page.lock();
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafTable => {
                let n = borrowed.n_cells();
                let psz = borrowed.data.len();
                // Binary search for the first cell with rowid >= start
                // (the old code linearly skipped every cell below the
                // range start — for `WHERE id BETWEEN 1000 AND 1009` that
                // walked hundreds of cells per leaf before the first hit).
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    match varint::decode_signed(borrowed.cell_slice_checked(cell_ptr)?) {
                        Some((rowid, _)) if rowid < start => lo = mid + 1,
                        _ => hi = mid,
                    }
                }
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if cell_ptr >= psz {
                        return Err(Error::corruption(format!(
                            "cell pointer {} out of range",
                            cell_ptr
                        )));
                    }
                    let buf = borrowed.cell_slice_checked(cell_ptr)?;
                    let (rowid, n1) = varint::decode_signed(buf).ok_or_else(|| {
                        Error::corruption("truncated leaf rowid in range_borrowed")
                    })?;
                    if rowid > end {
                        // Cells are sorted: everything after is past the
                        // range too — stop the whole walk.
                        return Ok(false);
                    }
                    if rowid >= start {
                        let rest = &buf[n1..];
                        let (plen, n2) = varint::decode(rest).ok_or_else(|| {
                            Error::corruption("truncated payload len in range_borrowed")
                        })?;
                        let payload_start = n1 + n2;
                        let plen_usize = plen as usize;
                        let local_len = overflow_local_len_for(plen_usize, psz);
                        if local_len == plen_usize {
                            if payload_start + plen_usize > buf.len() {
                                return Err(Error::corruption(
                                    "truncated payload in range_borrowed",
                                ));
                            }
                            let payload = &buf[payload_start..payload_start + plen_usize];
                            if !f(rowid, payload) {
                                return Ok(false);
                            }
                        } else {
                            // Overflow cell: local prefix + chain head.
                            // Same zero-copy assembly buffer as the full
                            // scan path (see scan_subtree_borrowed).
                            if local_len + 4 > buf.len() - payload_start {
                                return Err(Error::corruption(
                                    "truncated overflow cell in range_borrowed",
                                ));
                            }
                            let local = &buf[payload_start..payload_start + local_len];
                            let chain = u32::from_be_bytes(
                                buf[payload_start + local_len..payload_start + local_len + 4]
                                    .try_into()
                                    .unwrap(),
                            );
                            thread_local! {
                                static ASSEMBLE_BUF_RANGE: std::cell::RefCell<Vec<u8>> =
                                    std::cell::RefCell::new(Vec::with_capacity(1 << 16));
                            }
                            let cont = ASSEMBLE_BUF_RANGE.with(|b| {
                                let mut abuf = b.borrow_mut();
                                self.assemble_overflow_payload_into(local, plen, chain, &mut abuf)
                                    .map(|_| f(rowid, &abuf))
                            })?;
                            if !cont {
                                return Ok(false);
                            }
                        }
                    }
                }
                Ok(true) // leaf exhausted within the range — continue right
            }
            PageType::InteriorTable => {
                let n = borrowed.n_cells();
                let right = borrowed.right_most_pointer();
                // Binary search for the FIRST child whose separator >=
                // start (children with separator < start hold only rowids
                // < start). Then descend only into children [lo..n] plus
                // the right-most pointer — view decodes, no allocations,
                // no left-side scans.
                let mut lo: u16 = 0;
                let mut hi: u16 = n;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let cell_ptr = borrowed.cell_pointer(mid) as usize;
                    let key = decode_table_interior_key(borrowed.cell_slice_checked(cell_ptr)?)
                        .ok_or_else(|| {
                            Error::corruption("truncated interior cell in range scan")
                        })?;
                    if key < start {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let mut children: Vec<PageId> = Vec::with_capacity((n - lo) as usize + 1);
                for i in lo..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    if let Some(child) =
                        decode_table_interior_child(borrowed.cell_slice_checked(cell_ptr)?)
                    {
                        children.push(child);
                    }
                }
                if right != 0 {
                    children.push(right);
                }
                drop(borrowed);
                drop(page);
                for child in children {
                    if !self.scan_range_subtree_borrowed(child, start, end, f)? {
                        return Ok(false);
                    }
                }
                Ok(true)
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
                    let cell = Cell::decode(
                        borrowed.cell_slice_checked(cell_ptr)?,
                        pt,
                        borrowed.page_size(),
                    )?;
                    match cell {
                        Cell::TableLeaf { rowid, payload } => {
                            if rowid > end {
                                return Ok(());
                            }
                            if rowid >= start && !f(rowid, &payload) {
                                return Ok(());
                            }
                        }
                        Cell::TableLeafOverflow {
                            rowid,
                            local,
                            total,
                            overflow,
                            ..
                        } => {
                            if rowid > end {
                                return Ok(());
                            }
                            if rowid >= start {
                                let payload =
                                    self.assemble_overflow_payload(&local, total, overflow)?;
                                if !f(rowid, &payload) {
                                    return Ok(());
                                }
                            }
                        }
                        _ => {}
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
                        let c = Cell::decode(
                            borrowed.cell_slice_checked(cell_ptr)?,
                            pt,
                            borrowed.page_size(),
                        )?;
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
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.insert_index_inner(key, rowid);
        if r.is_ok() {
            if !self.journal_suppress {
                self.pager
                    .note_row_write(root, true, JournalKind::Insert, rowid, key, &[]);
            }
        } else {
            self.pager.note_journal_invalidated();
        }
        r
    }

    fn insert_index_inner(&mut self, key: &[u8], rowid: i64) -> Result<()> {
        // Notify the pager that a write is about to happen.
        self.pager.note_write();
        // Index entries are sorted by (key, rowid): the cell's btree order
        // is the byte-encoded key first, then the rowid as tiebreaker.
        // Oversized keys (larger than one page's payload budget) spill to
        // an overflow chain — SQLite parity: indexing a >page TEXT/BLOB is
        // legal. The cell keeps the deterministic local prefix + chain
        // head; the split point is a pure function of the key length, so
        // every reader derives it identically.
        let psz = self.pager.page_size() as usize;
        let cell = if key.len() > index_max_local(psz) {
            let local_len = index_local_len_v6(key.len(), psz);
            let chain = self.build_overflow_chain(key, local_len)?;
            self.pager.note_index_v6_cell()?;
            Cell::IndexLeafOverflow {
                rowid,
                total: key.len() as u64,
                local: key[..local_len].to_vec(),
                overflow: chain,
                v6: true,
            }
        } else {
            Cell::IndexLeaf {
                key: key.to_vec(),
                rowid,
            }
        };
        match self.insert_into_page(self.root, cell)? {
            InsertResult::Done => Ok(()),
            InsertResult::Duplicate => Err(Error::corruption(
                "index placement reported a duplicate table rowid",
            )),
            InsertResult::Split {
                new_page,
                split_key,
                split_key_bytes,
                extra_splits,
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
                // Oversized separators get their own chain copy.
                let cell = self.index_interior_separator(
                    old_root,
                    split_key_bytes.as_deref().unwrap_or_default(),
                    split_key,
                )?;
                self.insert_cell_into_page(new_root, &cell)?;
                // Additional boundaries (3+-way split): each separator is
                // the exact max entry of the page to its LEFT; oversized
                // separators get their own chain copies.
                let mut left = new_page;
                for ex in &extra_splits {
                    let cell = self.index_interior_separator(
                        left,
                        ex.split_key_bytes.as_deref().unwrap_or_default(),
                        ex.split_key,
                    )?;
                    self.insert_cell_into_page(new_root, &cell)?;
                    left = ex.new_page;
                }
                // The last sibling takes over the right-most slot.
                self.pager
                    .get_page(new_root)?
                    .lock()
                    .set_right_most_pointer(left);
                self.pager.note_root_split(old_root, new_root);
                if crate::executor::dbg_index_trace() {
                    eprintln!(
                        "[root-split:idx] old_root={} new_root={} self.root(was {})={}",
                        old_root, new_root, self.root, new_root
                    );
                }
                self.root = new_root;
                Ok(())
            }
        }
    }

    /// Delete a (key, rowid) pair from an index B+tree.
    /// The key is required because index pages are sorted by (key, rowid).
    pub fn delete_index(&mut self, key: &[u8], rowid: i64) -> Result<bool> {
        let _structural = self.pager.structural_scope();
        let root = self.root;
        let r = self.delete_index_inner(key, rowid);
        match r {
            Ok(did) => {
                if !self.journal_suppress {
                    self.pager
                        .note_row_write(root, true, JournalKind::Delete, rowid, key, &[]);
                    self.pager.note_row_outcome(did);
                }
                Ok(did)
            }
            Err(e) => {
                self.pager.note_journal_invalidated();
                Err(e)
            }
        }
    }

    fn delete_index_inner(&mut self, key: &[u8], rowid: i64) -> Result<bool> {
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
                    let psz = borrowed.page_size();
                    let mut lo = 0;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let cell = borrowed.cell_slice_checked(cell_ptr)?;
                        // Fast probe (no rowid decode unless keys tie).
                        match index_cell_lt(cell, false, psz, key, rowid) {
                            Some(Some(true)) => {
                                lo = mid + 1;
                                continue;
                            }
                            Some(Some(false)) => {
                                hi = mid;
                                continue;
                            }
                            _ => {}
                        }
                        let Some(v) = decode_index_cell(cell, false, psz) else {
                            return Err(Error::corruption("truncated index leaf cell"));
                        };
                        // Total order: overflow cells compare by full key.
                        let go_right = if v.overflow != 0 {
                            let full = self.index_view_key(&v)?;
                            (full.as_slice(), v.rowid) < (key, rowid)
                        } else {
                            (v.key, v.rowid) < (key, rowid)
                        };
                        if go_right {
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
                // Verify the exact (key, rowid) — full key for overflow
                // cells — and capture the chain to free on removal.
                let (key_matches, dead_chain) = {
                    let borrowed = page.lock();
                    let psz = borrowed.page_size();
                    let cell_ptr = borrowed.cell_pointer(pos) as usize;
                    match decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz) {
                        Some(v) => {
                            let matches = if v.overflow != 0 {
                                let full = self.index_view_key(&v)?;
                                full == key && v.rowid == rowid
                            } else {
                                v.key == key && v.rowid == rowid
                            };
                            (matches, v.overflow)
                        }
                        None => (false, 0),
                    }
                };
                if !key_matches {
                    return Ok(false);
                }
                drop(page);
                let removed = self.remove_cell_at(page_id, pos, n)?;
                if removed && dead_chain != 0 {
                    // The removed cell spilled: its chain pages die too.
                    self.free_overflow_chain(dead_chain)?;
                }
                Ok(removed)
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
                    let (_, child) = self.find_index_child(
                        data,
                        n,
                        cp,
                        borrowed.right_most_pointer(),
                        key,
                        rowid,
                    );
                    child
                };
                drop(page);
                let deleted = self.delete_index_from_page(child_id, key, rowid)?;
                if deleted {
                    let _ = self.maybe_recycle_empty_child(page_id, child_id)?;
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
            borrowed.touch();
        }
        self.pager.note_dirty(page_id);
        Ok(true)
    }

    // ========================================================================
    // SORTED BATCH INDEX MAINTENANCE — the pinned-leaf sweep.
    // ========================================================================
    //
    // A per-row maintenance op (delete_index / insert_index) pays a full
    // root-to-leaf descent per op — ~3 us per touched row at 100M scale,
    // and a band UPDATE of an indexed cyclic key is pure scatter (the
    // probe_index_maint calibration: 86% of the statement is index
    // maintenance, 3.36x SQLite on the same statement). A SORTED batch —
    // the same op multiset ordered by (key, rowid) — sweeps the tree left
    // to right instead: one descent per leaf boundary, then direct cell
    // writes into the pinned leaf.
    //
    // The pin carries the page's `(serial, epoch)` object-state proof
    // (refreshed post-touch after the pin's own writes); any structural
    // change by another path re-establishes it by descent. The two public
    // entry points take strict caller contracts:
    //
    //   * `entries` are ASCENDING by (key bytes, rowid) — byte order IS
    //     the tree's total order (the order-preserving key encoding);
    //   * non-unique indexes only (the executor gates unique indexes to
    //     the per-row immediate path — violations surface mid-statement);
    //   * delete batches tolerate absent entries (the per-op API's
    //     Ok(false) twin — the executor's membership gating is the
    //     contract).
    //
    // Journaling: one `note_row_write` per applied op, byte-identical to
    // the records the per-op APIs emit — the concurrent-merge replay
    // re-derives the batch op by op. Interior pages visited while
    // (re-)establishing a pin are marked as decision reads, the same
    // discipline as the per-op descents.

    /// Descend (read-only; routing interiors marked as decision reads)
    /// to the index leaf whose range covers `(key, rowid)` and pin it.
    /// `Ok(None)` means "not routable this way" — the caller falls back
    /// to the full per-op path (which surfaces real corruption itself).
    fn find_index_leaf_pinned(&mut self, key: &[u8], rowid: i64) -> Result<Option<IndexSweepPin>> {
        let mut parent: PageId = 0;
        let mut page_id = self.root;
        // Depth guard: a healthy tree is logarithmic; 64 levels is far
        // beyond any page-count reality (a cycle is corruption, and the
        // full path reports it better than a hang here).
        for _ in 0..64 {
            let page = match self.pager.get_page(page_id) {
                Ok(p) => p,
                Err(_) => return Ok(None),
            };
            let pt = match page.lock().page_type() {
                Ok(t) => t,
                Err(_) => return Ok(None),
            };
            match pt {
                PageType::LeafIndex => {
                    let mut pin = IndexSweepPin {
                        page: page_id,
                        parent,
                        serial: 0,
                        epoch: 0,
                        max_key: Vec::new(),
                        max_rowid: 0,
                        empty: true,
                        pos_hint: 0,
                    };
                    let n = {
                        let b = page.lock();
                        pin.serial = b.serial;
                        pin.epoch = b.epoch;
                        b.n_cells()
                    };
                    if n > 0 {
                        let b = page.lock();
                        let psz = b.page_size();
                        let cell_ptr = b.cell_pointer(n - 1) as usize;
                        let slice = match b.cell_slice_checked(cell_ptr) {
                            Ok(s) => s,
                            Err(_) => {
                                drop(b);
                                return Ok(None);
                            }
                        };
                        match decode_index_cell(slice, false, psz) {
                            Some(v) if v.overflow == 0 => {
                                pin.max_key = v.key.to_vec();
                                pin.max_rowid = v.rowid;
                                pin.empty = false;
                            }
                            Some(v) => {
                                // Spilled last key: reassemble the FULL
                                // key — the local prefix is not a fence.
                                match self.index_view_key(&v) {
                                    Ok(full) => {
                                        pin.max_key = full;
                                        pin.max_rowid = v.rowid;
                                        pin.empty = false;
                                    }
                                    Err(_) => {
                                        drop(b);
                                        return Ok(None);
                                    }
                                }
                            }
                            None => {
                                drop(b);
                                return Ok(None);
                            }
                        }
                    }
                    return Ok(Some(pin));
                }
                PageType::InteriorIndex => {
                    self.pager.note_decision_read(page_id);
                    let child_id = {
                        let b = page.lock();
                        let n = b.n_cells();
                        let cp = |i: u16| b.cell_pointer(i);
                        let (_, child) = self.find_index_child(
                            &b.data,
                            n,
                            cp,
                            b.right_most_pointer(),
                            key,
                            rowid,
                        );
                        child
                    };
                    if child_id == 0 {
                        return Ok(None);
                    }
                    parent = page_id;
                    page_id = child_id;
                }
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Bracketed galloping search for a sorted stream: find the first
    /// cell >= (key, rowid) starting from `hint` (the previous op's
    /// position). Probes forward in doubling steps to bracket the
    /// target, then binary-searches the bracket. Any overflow cell
    /// encountered mid-gallop falls back to the COLD binary search
    /// (full-key comparison needs the chain walk, which the cold path
    /// already performs). Returns the position in `0..=n`.
    fn sweep_gallop_search(
        &mut self,
        b: &Page,
        key: &[u8],
        rowid: i64,
        hint: u16,
        n: u16,
        psz: u32,
    ) -> Result<u16> {
        // Cell (key, rowid) at index i, as an ordering tuple; None
        // flags an overflow cell (caller falls back).
        let cell_order = |i: u16| -> Result<Option<(&[u8], i64)>> {
            let cell_ptr = b.cell_pointer(i) as usize;
            let Some(v) = decode_index_cell(b.cell_slice_checked(cell_ptr)?, false, psz) else {
                return Err(Error::corruption("truncated index leaf cell"));
            };
            if v.overflow != 0 {
                return Ok(None);
            }
            Ok(Some((v.key, v.rowid)))
        };
        // Cold binary search (overflow-aware, reassembling chains).
        let cold = |s: &mut Self, lo0: u16, hi0: u16| -> Result<u16> {
            let mut lo = lo0;
            let mut hi = hi0;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let cell_ptr = b.cell_pointer(mid) as usize;
                let Some(v) = decode_index_cell(b.cell_slice_checked(cell_ptr)?, false, psz) else {
                    return Err(Error::corruption("truncated index leaf cell"));
                };
                let go_right = if v.overflow != 0 {
                    let full = s.index_view_key(&v)?;
                    (full.as_slice(), v.rowid) < (key, rowid)
                } else {
                    (v.key, v.rowid) < (key, rowid)
                };
                if go_right {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            Ok(lo)
        };
        let hint = hint.min(n);
        if hint >= n {
            return Ok(n); // the stream is past every cell
        }
        // Defensive hint check (one probe): the cell BEFORE the hint
        // must be < the target. A stale or unsorted-stream hint that
        // skips past the true position would otherwise place the op in
        // the wrong slot — this turns any violation into a cold full
        // search (correct results, just slower).
        if hint > 0 {
            match cell_order(hint - 1)? {
                Some(cell) if cell < (key, rowid) => {}
                _ => return cold(self, 0, n),
            }
        }
        // Gallop forward from the hint.
        let mut step: u16 = 1;
        let mut lo = hint;
        let mut hi = n;
        loop {
            let probe = lo.saturating_add(step).min(n);
            match cell_order(probe.saturating_sub(1))? {
                Some(cell) if cell < (key, rowid) => {
                    // Target is at or after `probe`.
                    if probe >= n {
                        return Ok(n);
                    }
                    lo = probe;
                    step = step.saturating_mul(2);
                }
                Some(_) => {
                    // Target is in (lo - step_before ..= probe - 1]...
                    // bracket found: [lo_prev, probe).
                    hi = probe;
                    break;
                }
                None => {
                    // Overflow cell inside the bracket — cold search the
                    // whole (already narrowed) range.
                    return cold(self, lo, hi);
                }
            }
        }
        cold(self, lo, hi)
    }

    /// Direct insert of `(key, rowid)` into the pinned leaf — ONE lock
    /// for validate + search + mutate (the try_append shape), direct
    /// cell bytes from a stack buffer. The caller guarantees the entry
    /// belongs in the leaf's range (carried pin: within the fence; fresh
    /// pin: the descent just routed THIS entry here — append-past-last
    /// is the normal band shape). In-page keys only; a full leaf
    /// declines to the per-op path (its split machinery owns the
    /// structural work).
    fn sweep_insert_in_pinned(
        &mut self,
        pin: &mut IndexSweepPin,
        key: &[u8],
        rowid: i64,
    ) -> Result<SweepTry> {
        if key.len() > index_max_local(self.pager.page_size() as usize) {
            return Ok(SweepTry::Declined);
        }
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
        let page = match self.pager.get_page(pin.page) {
            Ok(p) => p,
            Err(_) => return Ok(SweepTry::Declined),
        };
        let page_id = pin.page;
        let root = self.root;
        let mut b = page.lock();
        if !matches!(b.page_type(), Ok(PageType::LeafIndex)) {
            return Ok(SweepTry::Declined);
        }
        if b.serial != pin.serial || b.epoch != pin.epoch {
            return Ok(SweepTry::Declined);
        }
        let n = b.n_cells();
        let psz = b.page_size();
        let free = b.free_space();
        if (free as usize) < cell_len + 2 {
            return Ok(SweepTry::Declined); // full — the split path owns it
        }
        // Galloping search from the previous op's position (the stream
        // is sorted — positions are monotone non-decreasing): probe
        // forward in doubling steps, then binary-search the bracket.
        // Falls back to the cold binary search when the hint is stale
        // or a probed cell is an overflow cell (full-key compare needs
        // the chain walk — the cold path already reassembles).
        let pos = self.sweep_gallop_search(&b, key, rowid, pin.pos_hint, n, psz)? as usize;
        let n_usize = n as usize;
        // Write the cell at the content frontier and open its pointer
        // slot (one memmove) — insert_cell_into_ref's tail, inlined.
        let new_content_start = b.cell_content_start() - cell_len as u32;
        let off = new_content_start as usize;
        if off + cell_len > b.data.len() {
            return Err(Error::corruption(format!(
                "cell content area out of range in sweep insert (page {page_id})"
            )));
        }
        b.data[off..off + cell_len].copy_from_slice(cell);
        b.set_cell_content_start(new_content_start);
        let header_offset = if page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
        b.data.copy_within(
            ptr_array_start + pos * 2..ptr_array_start + n_usize * 2,
            ptr_array_start + (pos + 1) * 2,
        );
        let dst = ptr_array_start + pos * 2;
        b.data[dst..dst + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());
        b.set_n_cells(n + 1);
        b.touch();
        // Post-touch proof refresh (the pin certifies the state it left
        // behind, not the state it found — the append path's rule). The
        // next op of the sorted stream lands at or after this position.
        pin.serial = b.serial;
        pin.epoch = b.epoch;
        pin.pos_hint = pos as u16 + 1;
        drop(b);
        self.pager.note_dirty(page_id);
        self.pager
            .note_row_write(root, true, JournalKind::Insert, rowid, key, &[]);
        Ok(SweepTry::Done { pin_alive: true })
    }

    /// Direct delete of `(key, rowid)` from the pinned leaf — ONE lock
    /// for validate + search + mutate, the same tolerated-miss contract
    /// as `delete_index`'s `Ok(false)`. When the removal empties the
    /// leaf, the leaf is recycled from its parent right there — the
    /// per-row path's discipline (a mass DELETE's 0-cell leaves must not
    /// linger as file bloat).
    #[allow(clippy::too_many_lines)]
    fn sweep_delete_in_pinned(
        &mut self,
        pin: &mut IndexSweepPin,
        key: &[u8],
        rowid: i64,
    ) -> Result<SweepTry> {
        let page = match self.pager.get_page(pin.page) {
            Ok(p) => p,
            Err(_) => return Ok(SweepTry::Declined),
        };
        let page_id = pin.page;
        let root = self.root;
        let mut b = page.lock();
        if !matches!(b.page_type(), Ok(PageType::LeafIndex)) {
            return Ok(SweepTry::Declined);
        }
        if b.serial != pin.serial || b.epoch != pin.epoch {
            return Ok(SweepTry::Declined);
        }
        let n = b.n_cells();
        let psz = b.page_size();
        // Galloping search from the previous op's position (sorted
        // stream — positions monotone non-decreasing).
        let lo = self.sweep_gallop_search(&b, key, rowid, pin.pos_hint, n, psz)?;
        if lo >= n {
            // Past every cell — absent (the fence said inside; the entry
            // is not there): a tolerated miss.
            return Ok(SweepTry::Done { pin_alive: true });
        }
        let cell_ptr = b.cell_pointer(lo) as usize;
        let (matches, dead_chain) =
            match decode_index_cell(b.cell_slice_checked(cell_ptr)?, false, psz) {
                Some(v) => {
                    let matches = if v.overflow != 0 {
                        let full = self.index_view_key(&v)?;
                        full == key && v.rowid == rowid
                    } else {
                        v.key == key && v.rowid == rowid
                    };
                    (matches, v.overflow)
                }
                None => (false, 0),
            };
        if !matches {
            return Ok(SweepTry::Done { pin_alive: true }); // absent — tolerated
        }
        // Remove the cell (pointer shift only) and refresh the fence
        // from the post-delete state — all under this one lock.
        let header_offset = if page_id == 0 {
            crate::storage::page::DB_HEADER_SIZE as usize
        } else {
            0
        };
        let ptr_array_start = header_offset + PAGE_HEADER_SIZE as usize;
        let pos_usize = lo as usize;
        let n_usize = n as usize;
        b.data.copy_within(
            ptr_array_start + (pos_usize + 1) * 2..ptr_array_start + n_usize * 2,
            ptr_array_start + pos_usize * 2,
        );
        b.set_n_cells(n - 1);
        b.touch();
        pin.serial = b.serial;
        pin.epoch = b.epoch;
        // The stream's next op lands at or after this position (the
        // cells shifted down by one here).
        pin.pos_hint = lo;
        let n_now = (n - 1) as usize;
        let mut became_empty = false;
        if n_now == 0 {
            became_empty = true;
        } else {
            let last_ptr = b.cell_pointer(n_now as u16 - 1) as usize;
            match b
                .cell_slice_checked(last_ptr)
                .ok()
                .and_then(|s| decode_index_cell(s, false, psz))
            {
                Some(v) if v.overflow == 0 => {
                    pin.max_key.clear();
                    pin.max_key.extend_from_slice(v.key);
                    pin.max_rowid = v.rowid;
                }
                Some(v) => {
                    if let Ok(full) = self.index_view_key(&v) {
                        pin.max_key = full;
                        pin.max_rowid = v.rowid;
                    } else {
                        became_empty = true; // fence unreadable: drop the pin
                    }
                }
                None => became_empty = true,
            }
        }
        drop(b);
        self.pager.note_dirty(page_id);
        if dead_chain != 0 {
            self.free_overflow_chain(dead_chain)?;
        }
        self.pager
            .note_row_write(root, true, JournalKind::Delete, rowid, key, &[]);
        if became_empty {
            if pin.parent != 0 {
                let _ = self.maybe_recycle_empty_child(pin.parent, pin.page)?;
            }
            return Ok(SweepTry::Done { pin_alive: false });
        }
        Ok(SweepTry::Done { pin_alive: true })
    }

    /// Apply a `(key, rowid)`-ascending batch of index DELETES as one
    /// pinned-leaf sweep. Every entry that shares a leaf with its
    /// predecessor costs one locked cell removal — no descent; boundary
    /// crossings re-pin by descent. Absent entries are tolerated misses.
    pub fn delete_index_sorted(&mut self, entries: &[(Vec<u8>, i64)]) -> Result<()> {
        let _structural = self.pager.structural_scope();
        if entries.is_empty() {
            return Ok(());
        }
        self.pager.note_write();
        let r = self.delete_index_sorted_inner(entries);
        if r.is_err() {
            self.pager.note_journal_invalidated();
        }
        r
    }

    fn delete_index_sorted_inner(&mut self, entries: &[(Vec<u8>, i64)]) -> Result<()> {
        let mut pin: Option<IndexSweepPin> = None;
        for (key, rowid) in entries {
            // Pinned fast path: the fence covers the op.
            let mut handled = false;
            if let Some(mut p) = pin.take() {
                if !p.empty && (&key[..], *rowid) <= (&p.max_key[..], p.max_rowid) {
                    match self.sweep_delete_in_pinned(&mut p, key, *rowid)? {
                        SweepTry::Done { pin_alive: true } => {
                            pin = Some(p);
                            handled = true;
                        }
                        SweepTry::Done { pin_alive: false } => {
                            handled = true; // leaf emptied + recycled; op applied
                        }
                        SweepTry::Declined => {} // stale — re-pin below
                    }
                }
            }
            if handled {
                continue;
            }
            // Re-pin by descent — a FRESH pin routed THIS op, so the
            // fence check is unnecessary (append-past-max is legal).
            if let Some(mut p) = self.find_index_leaf_pinned(key, *rowid)? {
                if !p.empty {
                    match self.sweep_delete_in_pinned(&mut p, key, *rowid)? {
                        SweepTry::Done { pin_alive: true } => {
                            pin = Some(p);
                            continue;
                        }
                        SweepTry::Done { pin_alive: false } => continue,
                        SweepTry::Declined => {}
                    }
                }
            }
            let _ = self.delete_index(key, *rowid)?;
        }
        Ok(())
    }

    /// Apply a `(key, rowid)`-ascending batch of index INSERTS as one
    /// pinned-leaf sweep. Splits (full leaves) and oversized keys fall
    /// back to the per-op path op by op — the amortized descent cost
    /// stays one per leaf boundary plus one per split.
    pub fn insert_index_sorted(&mut self, entries: &[(Vec<u8>, i64)]) -> Result<()> {
        let _structural = self.pager.structural_scope();
        if entries.is_empty() {
            return Ok(());
        }
        self.pager.note_write();
        let r = self.insert_index_sorted_inner(entries);
        if r.is_err() {
            self.pager.note_journal_invalidated();
        }
        r
    }

    fn insert_index_sorted_inner(&mut self, entries: &[(Vec<u8>, i64)]) -> Result<()> {
        let mut pin: Option<IndexSweepPin> = None;
        for (key, rowid) in entries {
            let mut handled = false;
            if let Some(mut p) = pin.take() {
                if !p.empty && (&key[..], *rowid) <= (&p.max_key[..], p.max_rowid) {
                    match self.sweep_insert_in_pinned(&mut p, key, *rowid)? {
                        SweepTry::Done { pin_alive: true } => {
                            pin = Some(p);
                            handled = true;
                        }
                        SweepTry::Done { pin_alive: false } => handled = true,
                        SweepTry::Declined => {}
                    }
                }
            }
            if handled {
                continue;
            }
            if let Some(mut p) = self.find_index_leaf_pinned(key, *rowid)? {
                if !p.empty {
                    match self.sweep_insert_in_pinned(&mut p, key, *rowid)? {
                        SweepTry::Done { pin_alive: true } => {
                            pin = Some(p);
                            continue;
                        }
                        SweepTry::Done { pin_alive: false } => continue,
                        SweepTry::Declined => {}
                    }
                }
            }
            self.insert_index(key, *rowid)?;
        }
        Ok(())
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
        let mut out = Vec::new();
        self.lookup_index_into(key, &mut out)?;
        Ok(out)
    }

    /// `lookup_index` into a caller-provided buffer (cleared first) — lets
    /// hot paths reuse one allocation across many lookups instead of paying
    /// a fresh Vec malloc + free (~25-30 ns) per query.
    pub fn lookup_index_into(&mut self, key: &[u8], out: &mut Vec<i64>) -> Result<()> {
        out.clear();
        // Concurrent regime: an equality probe reads exactly the entries
        // under `key` — tracked at key granularity (concurrent::sem_keys).
        let _probe = self.pager.key_probe_scope(self.root, key);
        // --- Hint probe: if the key falls inside the remembered leaf's
        // bounds, search that leaf directly. All matches must be collected
        // from this leaf only if the leaf's LAST cell doesn't itself match
        // the prefix (otherwise duplicates may continue into the right
        // sibling — fall back to the full scan for exactness).
        // --- Hint probe --------------------------------------------------
        // Struct-local FIRST (~2 ns), then the thread-local map (~40 ns):
        // a hoisted B+tree handle probing scattered keys misses both, but
        // the struct probe makes the miss nearly free.
        let epoch = self.pager.write_epoch();
        let hinted: Option<(PageRef, u32)> = self
            .probe_struct_index_hint(key, epoch)
            .or_else(|| index_hint_page(self.root, key, epoch));
        if let Some((page_ref, bias)) = hinted {
            if let Ok((true, cell)) = self.lookup_index_leaf(&page_ref, key, out, bias) {
                self.note_index_cell(cell, epoch);
                return Ok(());
            }
        }
        // The hint attempt may have pushed a PARTIAL run before deciding
        // it couldn't prove exactness (right-spill) — discard it; the full
        // scan re-collects every matching entry from the true start.
        out.clear();
        self.scan_index_from(key, |cell_rowid, cell_key| {
            if cell_key.starts_with(key) {
                out.push(cell_rowid);
                true // keep scanning (duplicates / composite prefix)
            } else {
                false // past the prefix range — stop
            }
        })?;
        Ok(())
    }

    /// Search the hinted index leaf for all cells whose key starts with
    /// `key`, appending their rowids to `out`. The epoch was checked by
    /// the caller, so the remembered bounds are exact and content
    /// unchanged. Returns `Ok((false, _))` to signal "hint unusable — do
    /// the full scan" (wrong page type, or a prefix run that may continue
    /// into either sibling); `out` may then hold a partial run which the
    /// caller must clear before re-scanning. On success returns
    /// `(true, run_start_cell)` — the caller stores the cell as the next
    /// probe's search bias.
    ///
    /// `bias` is the remembered cell index of the last successful probe —
    /// the search probes it first (SQLite's cursor-ix bias), then falls
    /// back to an exact lower-bound search. Sequential key access
    /// (fixed-parameter loops, INLJ inner probes with ordered outer keys)
    /// resolves in 1-2 probes instead of log2(cells).
    /// Key of leaf cell `i` for total-order comparisons: the borrowed
    /// in-page bytes, or the reassembled full key for overflow cells.
    /// (Reassembly walks the chain — only oversized-key indexes pay it.)
    #[inline]
    fn leaf_key_cow<'b>(
        &mut self,
        borrowed: &'b crate::storage::page::Page,
        i: usize,
    ) -> Result<std::borrow::Cow<'b, [u8]>> {
        let ptr = borrowed.cell_pointer(i as u16) as usize;
        let psz = borrowed.page_size();
        let Some(v) = decode_index_cell(borrowed.cell_slice_checked(ptr)?, false, psz) else {
            return Err(Error::corruption("corrupt index cell in biased search"));
        };
        if v.overflow == 0 {
            Ok(std::borrow::Cow::Borrowed(v.key))
        } else {
            let full = self.index_view_key(&v)?;
            Ok(std::borrow::Cow::Owned(full))
        }
    }

    fn lookup_index_leaf(
        &mut self,
        page_ref: &PageRef,
        key: &[u8],
        out: &mut Vec<i64>,
        bias: u32,
    ) -> Result<(bool, u32)> {
        let borrowed = page_ref.lock();
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        if pt != PageType::LeafIndex {
            return Ok((false, 0));
        }
        let n = borrowed.n_cells() as usize;
        if n == 0 {
            return Ok((false, 0));
        }
        // The caller already verified key ∈ [first, last] via the hint
        // bounds; with no mutation since, that check stands.
        // Read the key of cell i (borrow-only view).
        // Key reads go through self.leaf_key_cow (borrowed slice for
        // in-page cells, reassembled full key for overflow cells).
        // Lower-bound search for the first cell with key >= `key`,
        // biased at `bias`. Invariant: cells [0, lo) have key < `key`;
        // cells [hi, n) have key > `key`.
        let bias = (bias as usize).min(n - 1);
        let bk = self.leaf_key_cow(&borrowed, bias)?;
        let lo: usize = if bk.as_ref() >= key {
            // Target is at/left of the bias: binary search [0, bias],
            // then walk left over any equal-key run (bias may sit inside
            // the run — its key equals `key` exactly).
            let mut l = 0usize;
            let mut h = bias + 1;
            while l < h {
                let mid = (l + h) / 2;
                let k = self.leaf_key_cow(&borrowed, mid)?;
                if k.as_ref() < key {
                    l = mid + 1;
                } else {
                    h = mid;
                }
            }
            l
        } else {
            // Target is right of the bias: probe the immediately-next cell
            // (the sequential +1 pattern — ordered outer keys in join inner
            // loops — resolves in ONE more probe), then a bounded binary
            // search over the remaining bracket.
            let mut lo_b = bias + 1;
            let mut hi_b = n;
            if lo_b < hi_b {
                let k = self.leaf_key_cow(&borrowed, lo_b)?;
                if k.as_ref() < key {
                    lo_b += 1;
                } else {
                    hi_b = lo_b;
                }
            }
            // Bounded binary search in [lo_b, hi_b) for the first
            // key >= `key`.
            let mut l = lo_b;
            let mut h = hi_b;
            while l < h {
                let mid = (l + h) / 2;
                let k = self.leaf_key_cow(&borrowed, mid)?;
                if k.as_ref() < key {
                    l = mid + 1;
                } else {
                    h = mid;
                }
            }
            l
        };
        // LEFT-SPILL guard: if the run starts at the leaf's very first
        // cell, equal keys may ALSO exist in the left sibling — the hint
        // bounds only prove the key is present in THIS leaf. (The hint-hit
        // check guarantees key >= first-cell-key; lo == 0 then implies the
        // first cell's key equals the key, so the run may extend left.)
        // Only the full scan is exact. Found via the CREATE INDEX warm-tap
        // seeding the rightmost leaf: `COUNT(*) WHERE cat = 'b'` then saw
        // only the right leaf's half of a leaf-spanning duplicate run.
        if lo == 0 {
            return Ok((false, 0));
        }
        // Collect the prefix run. Overflow cells: the FULL key must
        // start with the search key — reassemble into one scratch.
        let mut i = lo;
        let mut scratch: Vec<u8> = Vec::new();
        while i < n {
            let cell_ptr = borrowed.cell_pointer(i as u16) as usize;
            let psz = borrowed.page_size();
            let Some(v) = decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz)
            else {
                break;
            };
            let matched = if v.overflow != 0 {
                scratch.clear();
                match self.assemble_overflow_payload_into(v.key, v.total, v.overflow, &mut scratch)
                {
                    Ok(()) => scratch.starts_with(key),
                    Err(_) => false,
                }
            } else {
                v.key.starts_with(key)
            };
            if matched {
                out.push(v.rowid);
            } else {
                break;
            }
            i += 1;
        }
        if i == n && lo < n {
            // The run reached the END of the leaf — duplicates may continue
            // into the right sibling; only the full scan is exact.
            return Ok((false, 0));
        }
        Ok((true, lo as u32))
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
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        let mut f = f;
        self.scan_index_range_subtree(self.root, start_key, &mut f, &mut false)?;
        Ok(())
    }

    /// Walk index entries in (key, rowid) order — ascending, or
    /// descending when `desc` — over the keys `lo <= key < hi` (`None` =
    /// unbounded). Calls `f(rowid, key)` and stops when it returns false.
    /// Unlike `scan_index_from`, `f` runs with NO page latch held: each
    /// leaf's in-range cells are copied out first, so the callback may
    /// read other trees (the index-order `ORDER BY … LIMIT` path fetches
    /// each row from inside the walk). Children outside the bounds are
    /// pruned through the interior separators — a child's entries are
    /// bounded by its neighbouring separators, the invariant
    /// `scan_index_from` already descends by.
    pub fn walk_index_ordered<F: FnMut(i64, &[u8]) -> Result<bool>>(
        &mut self,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        desc: bool,
        mut f: F,
    ) -> Result<()> {
        // Pure-read walk: suspend savepoint undo pre-image capture.
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        self.walk_index_ordered_rec(self.root, lo, hi, desc, &mut f)?;
        Ok(())
    }

    /// Index of the first cell of `pg` whose key (leaf) / separator
    /// (interior) is `>= bound` — `n_cells` when none is. Overflow keys
    /// compare by their reassembled full key.
    fn index_first_ge(&mut self, pg: &Page, interior: bool, bound: &[u8]) -> Result<u16> {
        let psz = pg.page_size();
        let (mut lo, mut hi) = (0u16, pg.n_cells());
        while lo < hi {
            let mid = (lo + hi) / 2;
            let ptr = pg.cell_pointer(mid) as usize;
            let Some(v) = decode_index_cell(pg.cell_slice_checked(ptr)?, interior, psz) else {
                return Err(Error::corruption("truncated index cell in ordered walk"));
            };
            let less = if v.overflow != 0 {
                self.index_view_key(&v)?.as_slice() < bound
            } else {
                v.key < bound
            };
            if less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    fn walk_index_ordered_rec<F: FnMut(i64, &[u8]) -> Result<bool>>(
        &mut self,
        page_id: PageId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        desc: bool,
        f: &mut F,
    ) -> Result<bool> {
        let page = self.pager.get_page(page_id)?;
        let guard = page.lock();
        let pg: &Page = &guard;
        let n = pg.n_cells();
        let interior = match pg.page_type()? {
            PageType::LeafIndex => false,
            PageType::InteriorIndex => true,
            other => {
                return Err(Error::corruption(format!(
                    "index walk reached a {:?} page",
                    other
                )))
            }
        };
        let begin = match lo {
            Some(b) => self.index_first_ge(pg, interior, b)?,
            None => 0,
        };
        let end = match hi {
            Some(b) => self.index_first_ge(pg, interior, b)?,
            None => n,
        };
        if !interior {
            // Copy the in-range cells, then release the latch.
            let psz = pg.page_size();
            let mut cells: Vec<(i64, Vec<u8>)> =
                Vec::with_capacity(end.saturating_sub(begin) as usize);
            for i in begin..end {
                let ptr = pg.cell_pointer(i) as usize;
                let Some(v) = decode_index_cell(pg.cell_slice_checked(ptr)?, false, psz) else {
                    return Err(Error::corruption(
                        "truncated index leaf cell in ordered walk",
                    ));
                };
                let key = self.index_view_key(&v)?;
                cells.push((v.rowid, key));
            }
            drop(guard);
            drop(page);
            if desc {
                for (rowid, key) in cells.iter().rev() {
                    if !f(*rowid, key)? {
                        return Ok(false);
                    }
                }
            } else {
                for (rowid, key) in &cells {
                    if !f(*rowid, key)? {
                        return Ok(false);
                    }
                }
            }
            return Ok(true);
        }
        // Interior: children `begin ..= end` (index n = right-most) can
        // hold in-range keys — those before `begin` end below `lo` (their
        // separators are < lo), those after `end` start at or above `hi`.
        let psz = pg.page_size();
        let mut children: Vec<PageId> = Vec::with_capacity((end - begin) as usize + 1);
        for i in begin..=end.min(n) {
            if i == n {
                children.push(pg.right_most_pointer());
            } else {
                let ptr = pg.cell_pointer(i) as usize;
                let Some(v) = decode_index_cell(pg.cell_slice_checked(ptr)?, true, psz) else {
                    return Err(Error::corruption(
                        "truncated index interior cell in ordered walk",
                    ));
                };
                children.push(v.left_child);
            }
        }
        drop(guard);
        drop(page);
        if desc {
            children.reverse();
        }
        for child in children {
            if child != 0 && !self.walk_index_ordered_rec(child, lo, hi, desc, f)? {
                return Ok(false);
            }
        }
        Ok(true)
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
        prefetch_search_lines(&borrowed.data);
        let pt = borrowed.page_type()?;
        match pt {
            PageType::LeafIndex => {
                let n = borrowed.n_cells();
                let psz = borrowed.page_size();
                let begin = if *started {
                    0
                } else {
                    // Binary search for the first cell with key >= start_key
                    // (allocation-free views; (key, MIN) ordering means a
                    // cell is "less" iff its key is strictly less).
                    // Overflow cells compare by their reassembled full key.
                    let mut lo = 0u16;
                    let mut hi = n;
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let cell_ptr = borrowed.cell_pointer(mid) as usize;
                        let Some(v) =
                            decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz)
                        else {
                            return Err(Error::corruption("truncated index leaf cell in scan"));
                        };
                        let go_right = if v.overflow != 0 {
                            let full = self.index_view_key(&v)?;
                            full.as_slice() < start_key
                        } else {
                            v.key < start_key
                        };
                        if go_right {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    lo
                };
                // Record the leaf hint (exact live bounds) for future point
                // lookups on this tree. MUST happen before the cell loop:
                // a point lookup's callback returns false at the first
                // non-matching cell, and the resulting early return skipped
                // this — so every indexed point lookup paid a full root->
                // interior->leaf descent forever (measured hit rate 0.2%).
                // Overflow-bound keys are skipped (a truncated local prefix
                // as a hint bound would misroute lookups).
                if n > 0 {
                    if let (Some(a), Some(b)) = (
                        borrowed
                            .cell_slice(0)
                            .ok()
                            .and_then(|s| decode_index_cell(s, false, psz))
                            .filter(|c| c.overflow == 0),
                        borrowed
                            .cell_slice(n - 1)
                            .ok()
                            .and_then(|s| decode_index_cell(s, false, psz))
                            .filter(|c| c.overflow == 0),
                    ) {
                        self.record_index_hint(&page, a.key, b.key, self.pager.write_epoch());
                    }
                }
                // Iterate the leaf under the SAME lock, borrowing key slices
                // straight from the page buffer (Cell::decode allocated a
                // key Vec per cell here). Overflow cells reassemble into a
                // reusable scratch — the callback sees the FULL key.
                let mut key_scratch: Vec<u8> = Vec::new();
                for i in begin..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let Some(v) =
                        decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz)
                    else {
                        continue;
                    };
                    if v.overflow != 0 {
                        key_scratch.clear();
                        self.assemble_overflow_payload_into(
                            v.key,
                            v.total,
                            v.overflow,
                            &mut key_scratch,
                        )?;
                        if !f(v.rowid, &key_scratch) {
                            return Ok(false);
                        }
                    } else if !f(v.rowid, v.key) {
                        return Ok(false);
                    }
                }
                *started = true;
                Ok(true)
            }
            PageType::InteriorIndex => {
                // Find the first child that can contain entries >= start_key.
                // Under the SAME guard, also read that first child's pointer —
                // for point lookups the descent visits exactly one child, so
                // this avoids a second lock of the parent per level (the old
                // loop re-locked the parent for every child iteration).
                let (n, first_cell_idx, first_child, right_most) = {
                    let n = borrowed.n_cells();
                    // When the start key is already behind us (started),
                    // EVERY child of this page is fully in range — begin at
                    // cell 0. (The old code began at `n` here, visiting only
                    // the right-most child and silently skipping all left
                    // children of interior pages entered mid-scan — a latent
                    // bug reachable in 3+ level index trees whose range
                    // crosses an interior-sibling boundary.)
                    let mut first_cell_idx = if *started { 0 } else { n };
                    if !*started {
                        let psz = borrowed.page_size();
                        let mut lo = 0u16;
                        let mut hi = n;
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            let cell_ptr = borrowed.cell_pointer(mid) as usize;
                            let Some(v) = decode_index_cell(
                                borrowed.cell_slice_checked(cell_ptr)?,
                                true,
                                psz,
                            ) else {
                                break;
                            };
                            // Overflow separators compare by full key.
                            let go_right = if v.overflow != 0 {
                                match self.index_view_key(&v) {
                                    Ok(full) => full.as_slice() < start_key,
                                    Err(_) => true, // corrupt chain: descend right
                                }
                            } else {
                                v.key < start_key
                            };
                            if go_right {
                                lo = mid + 1;
                            } else {
                                hi = mid;
                            }
                        }
                        first_cell_idx = lo;
                    }
                    // Read the FIRST child to descend under this same lock.
                    let first_child = if first_cell_idx < n {
                        let psz = borrowed.page_size();
                        let cell_ptr = borrowed.cell_pointer(first_cell_idx) as usize;
                        decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, true, psz)
                            .map(|v| v.left_child)
                            .unwrap_or(0)
                    } else {
                        0 // sentinel: descend right_most directly
                    };
                    (
                        n,
                        first_cell_idx,
                        first_child,
                        borrowed.right_most_pointer(),
                    )
                };
                // Release the parent page before recursing into children.
                drop(borrowed);
                drop(page);
                // Descend the first (usually only) child without re-locking
                // the parent.
                if first_child != 0
                    && !self.scan_index_range_subtree(first_child, start_key, f, started)?
                {
                    return Ok(false);
                }
                // Remaining children (rare: the range spans siblings).
                for i in (first_cell_idx + if first_child != 0 { 1 } else { 0 })..n {
                    let page = self.pager.get_page(page_id)?;
                    let borrowed = page.lock();
                    let psz = borrowed.page_size();
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let child = match decode_index_cell(
                        borrowed.cell_slice_checked(cell_ptr)?,
                        true,
                        psz,
                    ) {
                        Some(v) => v.left_child,
                        None => break,
                    };
                    drop(borrowed);
                    drop(page);
                    if !self.scan_index_range_subtree(child, start_key, f, started)? {
                        return Ok(false);
                    }
                }
                if right_most != 0
                    && !self.scan_index_range_subtree(right_most, start_key, f, started)?
                {
                    return Ok(false);
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
        // Pure-read walk: suspend savepoint undo pre-image capture
        // for this scan's own fetches (see storage::pager's TLS docs).
        let _undo_scan = crate::storage::pager::suspend_undo_capture();
        self.scan_index_subtree(self.root, &mut f)
    }

    /// Env-gated (RUSTQLITE_DBG_IDX) topology dump: every page of the
    /// tree (type, n_cells, right-most pointer, children) — diagnosing
    /// orphaned-page corruption.
    pub fn debug_dump_tree(&mut self, label: &str) {
        if !crate::executor::dbg_index_trace() {
            return;
        }
        eprintln!("[dump] {label} root={}", self.root);
        let mut queue: Vec<(PageId, usize)> = vec![(self.root, 0)];
        let mut seen = std::collections::HashSet::new();
        while let Some((pid, depth)) = queue.pop() {
            if !seen.insert(pid) {
                eprintln!("[dump]   page {} (depth {}) REVISITED", pid, depth);
                continue;
            }
            let Ok(page) = self.pager.get_page(pid) else {
                eprintln!("[dump]   page {} (depth {}) UNREADABLE", pid, depth);
                continue;
            };
            let b = page.lock();
            let Ok(pt) = b.page_type() else {
                eprintln!("[dump]   page {} (depth {}) BAD-TYPE", pid, depth);
                continue;
            };
            let n = b.n_cells();
            let rm = b.right_most_pointer();
            let psz = b.page_size();
            let first = if n > 0 {
                let p = b.cell_pointer(0) as usize;
                decode_index_cell(&b.data[p..], false, psz)
                    .map(|v| {
                        format!(
                            "rowid={} key={:02X?}",
                            v.rowid,
                            v.key.iter().take(6).copied().collect::<Vec<u8>>()
                        )
                    })
                    .unwrap_or_else(|| "?".into())
            } else {
                "-".into()
            };
            let last = if n > 0 {
                let p = b.cell_pointer(n - 1) as usize;
                decode_index_cell(&b.data[p..], false, psz)
                    .map(|v| {
                        format!(
                            "rowid={} key={:02X?}",
                            v.rowid,
                            v.key.iter().take(6).copied().collect::<Vec<u8>>()
                        )
                    })
                    .unwrap_or_else(|| "?".into())
            } else {
                "-".into()
            };
            eprintln!(
                "[dump]   page {} (depth {}) {:?} n={} rm={} first[{}] last[{}]",
                pid, depth, pt, n, rm, first, last
            );
            match pt {
                PageType::InteriorIndex => {
                    for i in 0..n {
                        let p = b.cell_pointer(i) as usize;
                        if let Some(v) = decode_index_cell(&b.data[p..], true, psz) {
                            queue.push((v.left_child, depth + 1));
                        }
                    }
                    if rm != 0 {
                        queue.push((rm, depth + 1));
                    }
                }
                PageType::LeafIndex => {}
                _ => {}
            }
        }
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
                // Overflow cells reassemble into a reusable scratch so the
                // callback still sees the FULL key.
                let borrowed = page.lock();
                let n = borrowed.n_cells();
                let psz = borrowed.page_size();
                let mut key_scratch: Vec<u8> = Vec::new();
                for i in 0..n {
                    let cell_ptr = borrowed.cell_pointer(i) as usize;
                    let Some(v) =
                        decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, false, psz)
                    else {
                        continue;
                    };
                    if v.overflow != 0 {
                        key_scratch.clear();
                        self.assemble_overflow_payload_into(
                            v.key,
                            v.total,
                            v.overflow,
                            &mut key_scratch,
                        )?;
                        if !f(v.rowid, &key_scratch) {
                            return Ok(());
                        }
                    } else if !f(v.rowid, v.key) {
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
                    let psz = borrowed.page_size();
                    let mut v = Vec::with_capacity(n as usize + 1);
                    for i in 0..n {
                        let cell_ptr = borrowed.cell_pointer(i) as usize;
                        if let Some(c) =
                            decode_index_cell(borrowed.cell_slice_checked(cell_ptr)?, true, psz)
                        {
                            v.push(c.left_child);
                        }
                    }
                    // 0 = "no rightmost child" (an interior page keeps
                    // this after a split); descending into 0 would scan the
                    // header/schema-root page and yield its row as a table
                    // row. Range scans already guard this; these full-scan
                    // and count paths must too.
                    if right != 0 {
                        v.push(right);
                    }
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

// Helper: safely extract the key from any cell.
// fn child_key_safe(c: &Cell) -> i64 {
//     c.key()
// }

/// First/last cell rowids of a table leaf (None when the leaf is empty).
/// Used by `delete_rowids_inorder`'s sticky-leaf bounds.
fn leaf_rowid_bounds(pager: &Pager, leaf: PageId) -> Result<Option<(i64, i64)>> {
    let page = pager.get_page(leaf)?;
    let borrowed = page.lock();
    if borrowed.page_type()? != PageType::LeafTable {
        return Ok(None);
    }
    let n = borrowed.n_cells() as usize;
    if n == 0 {
        return Ok(None);
    }
    let first = borrowed
        .cell_slice_checked(borrowed.cell_pointer(0) as usize)
        .ok()
        .and_then(|c| varint::decode_signed(c).map(|(k, _)| k));
    let last = borrowed
        .cell_slice_checked(borrowed.cell_pointer((n - 1) as u16) as usize)
        .ok()
        .and_then(|c| varint::decode_signed(c).map(|(k, _)| k));
    match (first, last) {
        (Some(a), Some(b)) => Ok(Some((a, b))),
        _ => Ok(None),
    }
}

/// Collect EVERY page of the b-tree rooted at `root` — the DROP TABLE /
/// DROP INDEX reclaim walk. Interiors + leaves via a cycle-guarded DFS,
/// plus every cell's overflow chain. The root itself is included. Page
/// 0 is never collected (the catalog root is permanent).
///
/// The historical DROP freed ONLY the root page: a multi-page table's
/// whole subtree leaked (orphaned interiors/leaves/overflow chains,
/// still contentful in the file but unreachable — pinned by
/// sqlite_dbdata: a 5-page DROP left freelist_count at 1 with the
/// leaves decoding full rows). Collect FIRST, free AFTER: an error
/// mid-walk (corrupt tree) frees nothing.
pub fn collect_tree_pages(pager: &Pager, root: PageId, out: &mut Vec<PageId>) -> Result<()> {
    let psz = pager.page_size();
    let n_pages = pager.n_pages();
    let mut visited: std::collections::HashSet<PageId> = std::collections::HashSet::new();
    let mut stack: Vec<PageId> = Vec::new();
    if root != 0 {
        stack.push(root);
    }
    while let Some(pid) = stack.pop() {
        if pid == 0 || pid >= n_pages {
            return Err(Error::corruption(format!(
                "tree walk references page {pid} outside the file ({n_pages} pages)"
            )));
        }
        if !visited.insert(pid) {
            return Err(Error::corruption(format!(
                "tree walk revisits page {pid} (cycle?)"
            )));
        }
        if visited.len() > n_pages as usize {
            return Err(Error::corruption("tree walk exceeded the file"));
        }
        let page = pager.get_page(pid)?;
        let (pt, ncell, right) = {
            let p = page.lock();
            (p.page_type()?, p.n_cells(), p.right_most_pointer())
        };
        for i in 0..ncell {
            let cell = {
                let p = page.lock();
                let buf = p.cell_slice(i)?;
                Cell::decode(buf, pt, psz)?
            };
            match cell {
                Cell::TableInterior { left_child, .. } | Cell::IndexInterior { left_child, .. } => {
                    stack.push(left_child)
                }
                Cell::TableLeafOverflow { overflow, .. }
                | Cell::IndexLeafOverflow { overflow, .. } => {
                    collect_overflow_chain(pager, overflow, &mut visited, out)?;
                }
                Cell::IndexInteriorOverflow {
                    left_child,
                    overflow,
                    ..
                } => {
                    stack.push(left_child);
                    collect_overflow_chain(pager, overflow, &mut visited, out)?;
                }
                Cell::TableLeaf { .. } | Cell::IndexLeaf { .. } => {}
            }
        }
        if let PageType::InteriorIndex | PageType::InteriorTable = pt {
            if right != 0 {
                stack.push(right);
            }
        }
        out.push(pid);
    }
    Ok(())
}

/// Follow one overflow chain, collecting its pages (cycle-guarded like
/// the tree walk). `visited` is shared with the tree walk so a chain
/// that loops back into the tree trips the same guard.
fn collect_overflow_chain(
    pager: &Pager,
    head: PageId,
    visited: &mut std::collections::HashSet<PageId>,
    out: &mut Vec<PageId>,
) -> Result<()> {
    let n_pages = pager.n_pages();
    let mut cur = head;
    let mut hops = 0usize;
    while cur != 0 {
        if cur >= n_pages {
            return Err(Error::corruption(format!(
                "overflow chain references page {cur} outside the file"
            )));
        }
        if !visited.insert(cur) {
            return Err(Error::corruption(format!(
                "overflow chain revisits page {cur} (cycle?)"
            )));
        }
        hops += 1;
        if hops > n_pages as usize {
            return Err(Error::corruption("overflow chain exceeded the file"));
        }
        let page = pager.get_page(cur)?;
        let next = page.lock().overflow_next();
        out.push(cur);
        cur = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;
    use tempfile::NamedTempFile;

    fn open_pager() -> Pager {
        let tmp = NamedTempFile::new().unwrap();
        Pager::open(tmp.path(), 256).unwrap()
    }

    // ── sorted batch index maintenance (the pinned-leaf sweep) ──

    /// Collect the tree's full entry list in scan order (the tree's own
    /// total order — the comparison oracle for batch-vs-per-op tests).
    fn scan_index_entries(bt: &mut Btree<'_>) -> Vec<(Vec<u8>, i64)> {
        let mut out = Vec::new();
        bt.scan_index(|rowid, key| {
            out.push((key.to_vec(), rowid));
            true
        })
        .unwrap();
        out
    }

    /// The sweep must be invisible: a batch of deletes+inserts applied
    /// through the sorted APIs leaves the tree with EXACTLY the entry
    /// multiset the per-op path produces, in tree order, with the same
    /// page hygiene (emptied leaves recycled).
    ///
    /// Deterministic construction of a scattered-key index tree +
    /// its entry list (a nested fn so the returned Btree can borrow the
    /// returned Pager).
    fn mk_scattered_tree(pager: &Pager, seed: u64) -> (Btree<'_>, Vec<(Vec<u8>, i64)>) {
        let mut s = seed;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut bt = Btree::create(pager, true).unwrap();
        let mut ops: Vec<(Vec<u8>, i64)> = Vec::new();
        for i in 1..=4000i64 {
            let k = (next() % 1500) as i64;
            let key = Value::Integer(k).encode_order_key();
            bt.insert_index(&key, i).unwrap();
            ops.push((key, i));
        }
        (bt, ops)
    }

    #[test]
    fn index_sorted_batch_matches_per_op_random() {
        // The maintenance multiset: half the rowids get their keys
        // shifted, a quarter are deleted outright.
        fn maintenance(next: &mut dyn FnMut() -> u64) -> Vec<(u64, i64, bool)> {
            let mut v = Vec::new();
            for rid in 1..=4000i64 {
                match next() % 4 {
                    0 | 1 => v.push((next() % 1500, rid, true)), // shift key
                    2 => v.push((0, rid, false)),                // delete only
                    _ => {}
                }
            }
            v
        }
        fn run(batch: bool) -> Vec<(Vec<u8>, i64)> {
            let pager = Pager::open_memory(512).unwrap();
            let (mut bt, existing) = mk_scattered_tree(&pager, 0xdead_beef);
            let mut s2 = 0x1234_5678_9abc_def0u64;
            let mut next2 = move || {
                s2 ^= s2 << 13;
                s2 ^= s2 >> 7;
                s2 ^= s2 << 17;
                s2
            };
            let ops = maintenance(&mut next2);
            // The per-op reference applies each op immediately; the
            // batch path sorts dels and ins and applies two sweeps.
            let mut dels: Vec<(Vec<u8>, i64)> = Vec::new();
            let mut ins: Vec<(Vec<u8>, i64)> = Vec::new();
            for &(k, rid, shift) in &ops {
                let old = existing
                    .iter()
                    .find(|(_, r)| *r == rid)
                    .map(|(k, _)| k.clone())
                    .unwrap();
                dels.push((old, rid));
                if shift {
                    ins.push((Value::Integer(k as i64).encode_order_key(), rid));
                }
                if batch {
                    continue;
                }
                for d in dels.drain(..) {
                    bt.delete_index(&d.0, d.1).unwrap();
                }
                for i2 in ins.drain(..) {
                    bt.insert_index(&i2.0, i2.1).unwrap();
                }
            }
            if batch {
                dels.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
                ins.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
                bt.delete_index_sorted(&dels).unwrap();
                bt.insert_index_sorted(&ins).unwrap();
            }
            scan_index_entries(&mut bt)
        }
        let per_op = run(false);
        let batched = run(true);
        assert_eq!(per_op, batched, "batch sweep diverged from per-op");
        assert!(!per_op.is_empty());
    }

    /// The band shape itself: contiguous key ranges moved across the
    /// cycle (the marathon M9 UPDATE form), large enough to cross many
    /// leaf boundaries, empty whole leaves (the recycle path), and
    /// refill target leaves past their fence (split-heavy stretches).
    #[test]
    fn index_sorted_batch_band_shape() {
        fn mk(pager: &Pager) -> Btree<'_> {
            let cycle = 20_000i64;
            let rows = 40_000i64;
            let mut bt = Btree::create(pager, true).unwrap();
            for i in 1..=rows {
                let key = Value::Integer(i % cycle).encode_order_key();
                bt.insert_index(&key, i).unwrap();
            }
            bt
        }
        let cycle = 20_000i64;
        let shift = 7_000i64;
        // Band: rowids 1..=5000 move their keys +shift; 5001..=6000 are
        // deleted outright.
        let mut dels: Vec<(Vec<u8>, i64)> = Vec::new();
        let mut ins: Vec<(Vec<u8>, i64)> = Vec::new();
        for i in 1..=6000i64 {
            dels.push((Value::Integer(i % cycle).encode_order_key(), i));
            if i <= 5000 {
                ins.push((
                    Value::Integer((i % cycle + shift) % cycle).encode_order_key(),
                    i,
                ));
            }
        }
        dels.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
        ins.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));

        let p1 = Pager::open_memory(512).unwrap();
        let mut bt_per_op = mk(&p1);
        for d in &dels {
            bt_per_op.delete_index(&d.0, d.1).unwrap();
        }
        for i in &ins {
            bt_per_op.insert_index(&i.0, i.1).unwrap();
        }
        let expect = scan_index_entries(&mut bt_per_op);

        let p2 = Pager::open_memory(512).unwrap();
        let mut bt_batch = mk(&p2);
        bt_batch.delete_index_sorted(&dels).unwrap();
        bt_batch.insert_index_sorted(&ins).unwrap();
        let got = scan_index_entries(&mut bt_batch);

        assert_eq!(expect, got, "band-shape batch sweep diverged");
    }

    /// Emptied leaves must not linger: a sorted delete pass that clears
    /// whole leaves recycles them, keeping the page count at (or below)
    /// the per-op path's.
    #[test]
    fn index_sorted_delete_recycles_emptied_leaves() {
        fn mk(pager: &Pager) -> Btree<'_> {
            let mut bt = Btree::create(pager, true).unwrap();
            for i in 1..=30_000i64 {
                let key = Value::Integer(i).encode_order_key();
                bt.insert_index(&key, i).unwrap();
            }
            bt
        }
        // Delete a contiguous range covering many whole leaves.
        let dels: Vec<(Vec<u8>, i64)> = (5_000..=25_000)
            .map(|i| (Value::Integer(i).encode_order_key(), i))
            .collect();
        let p1 = Pager::open_memory(512).unwrap();
        let mut bt1 = mk(&p1);
        for d in &dels {
            bt1.delete_index(&d.0, d.1).unwrap();
        }
        let per_op_pages = p1.n_pages();
        let per_op_entries = scan_index_entries(&mut bt1);

        let p2 = Pager::open_memory(512).unwrap();
        let mut bt2 = mk(&p2);
        bt2.delete_index_sorted(&dels).unwrap();
        let batch_pages = p2.n_pages();
        let batch_entries = scan_index_entries(&mut bt2);

        assert_eq!(per_op_entries, batch_entries);
        assert!(
            batch_pages <= per_op_pages,
            "batch left {batch_pages} pages vs per-op {per_op_pages} (empty leaves not recycled)"
        );
    }

    /// Oversized (overflow-chain) keys in the batch decline to the full
    /// per-op path — correctness over speed, verified end to end.
    #[test]
    fn index_sorted_batch_handles_oversized_keys() {
        let pager = Pager::open_memory(512).unwrap();
        let mut bt = Btree::create(&pager, true).unwrap();
        let big = |i: i64| {
            Value::Text(crate::types::Text::new(&format!("{i:06}").repeat(120))).encode_order_key()
        };
        for i in 1..=200i64 {
            bt.insert_index(&big(i), i).unwrap();
        }
        // Move every other oversized key to a new value, batched.
        let mut dels: Vec<(Vec<u8>, i64)> = Vec::new();
        let mut ins: Vec<(Vec<u8>, i64)> = Vec::new();
        for i in 1..=200i64 {
            if i % 2 == 0 {
                dels.push((big(i), i));
                ins.push((big(i + 10_000), i));
            }
        }
        dels.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
        ins.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
        bt.delete_index_sorted(&dels).unwrap();
        bt.insert_index_sorted(&ins).unwrap();

        let mut expect: Vec<(Vec<u8>, i64)> = Vec::new();
        for i in 1..=200i64 {
            if i % 2 == 0 {
                expect.push((big(i + 10_000), i));
            } else {
                expect.push((big(i), i));
            }
        }
        expect.sort_unstable_by(|a, b| (&a.0[..], a.1).cmp(&(&b.0[..], b.1)));
        assert_eq!(scan_index_entries(&mut bt), expect);
    }

    // Temporary perf probe (not a correctness test): raw walk costs on an
    // in-memory pager with 2KB-wide rows, mirroring torture S08.
    #[test]
    fn probe_wide_scan_costs() {
        let iters = 3;
        for &n_rows in &[3_750usize, 25_000usize] {
            let pager = Pager::open_memory(2048).unwrap();
            let mut bt = Btree::create(&pager, false).unwrap();
            let mut payload: Vec<u8> = Vec::with_capacity(2050);
            let wide: String = "WIDE-DATA-".repeat(200);
            for i in 1..=n_rows as i64 {
                // Row shape = (id INTEGER PRIMARY KEY [rowid marker],
                // b TEXT) — encode_row_aliased handles the marker.
                let row: Vec<Value> = vec![
                    Value::Integer(i),
                    Value::Text(crate::types::Text::new(&wide)),
                ];
                crate::storage::row_codec::encode_row_aliased_into(&row, Some(0), &mut payload);
                bt.insert_table(i, &payload).unwrap();
            }
            let pages = pager.n_pages();
            println!(
                "n_rows={n_rows} pages={pages} misses_total={}",
                pager.cache_misses()
            );

            // (1) count-only walk (no payload decode)
            let t = std::time::Instant::now();
            let mut acc = 0u64;
            for _ in 0..iters {
                acc += bt.count_rows().unwrap();
            }
            let count_ms = t.elapsed().as_secs_f64() * 1000.0;

            // (2) borrowed scan, callback sees payload slice, no decode
            let t = std::time::Instant::now();
            let mut acc2 = 0usize;
            for _ in 0..iters {
                bt.scan_table_borrowed(|_rid, p| {
                    acc2 = acc2.wrapping_add(p.len());
                    true
                })
                .unwrap();
            }
            let walk_ms = t.elapsed().as_secs_f64() * 1000.0;

            // (3) selective decode: id only (rowid alias col 0)
            let t = std::time::Instant::now();
            let mut acc3 = 0i64;
            for _ in 0..iters {
                bt.scan_table_selective(2, &[0], Some(0), |rid, row| {
                    acc3 = acc3.wrapping_add(rid);
                    if let Some(Value::Integer(v)) = row.first() {
                        acc3 = acc3.wrapping_add(*v);
                    }
                    true
                })
                .unwrap();
            }
            let sel_id_ms = t.elapsed().as_secs_f64() * 1000.0;

            // (4) selective decode: b (2KB text materialization)
            let t = std::time::Instant::now();
            let mut acc4 = 0usize;
            for _ in 0..iters {
                bt.scan_table_selective(2, &[1], None, |_rid, row| {
                    if let Some(Value::Text(x)) = row.first() {
                        acc4 = acc4.wrapping_add(x.len());
                    }
                    true
                })
                .unwrap();
            }
            let sel_b_ms = t.elapsed().as_secs_f64() * 1000.0;

            let per = |ms: f64| ms * 1e6 / (n_rows as f64 * iters as f64);
            println!(
                "  count={acc} acc2={acc2} acc3={acc3} acc4={acc4} | (1) count_rows: {:.1} ns/row | (2) borrowed walk: {:.1} ns/row | (3) sel id: {:.1} ns/row | (4) sel b(2KB): {:.1} ns/row",
                per(count_ms), per(walk_ms), per(sel_id_ms), per(sel_b_ms)
            );
            drop(bt);
            let _ = acc;
        }
    }

    // Bulk mass-delete: unlinked leaves + file-tail truncation (S14 shape).
    #[test]
    fn probe_bulk_delete_truncate() {
        let tmp = NamedTempFile::new().unwrap();
        let pager = Pager::open(tmp.path(), 2048).unwrap();
        let mut bt = Btree::create(&pager, false).unwrap();
        let n = 60_000i64;
        let mut payload: Vec<u8> = Vec::with_capacity(64);
        for i in 1..=n {
            payload.clear();
            Value::Integer(i).encode_into(&mut payload);
            Value::Integer(i).encode_into(&mut payload);
            bt.insert_table(i, &payload).unwrap();
        }
        let pages_before = pager.n_pages();
        let rowids: Vec<i64> = ((n / 10 + 1)..=n).collect();
        bt.delete_rowids_inorder(&rowids).unwrap();
        let pages_after = pager.n_pages();
        pager.flush().unwrap();
        println!(
            "pages {pages_before} -> {pages_after}, freelist_count={}, file_len={}",
            pager.freelist_count(),
            std::fs::metadata(tmp.path()).unwrap().len()
        );
        let mut cnt = 0u64;
        bt.scan_table_borrowed(|_rid, _p| {
            cnt += 1;
            true
        })
        .unwrap();
        println!("scan-after count = {cnt} (expect {})", n / 10);
        assert_eq!(cnt, (n / 10) as u64);
        // The tail should have truncated: far fewer pages than before.
        assert!(
            pages_after < pages_before / 2,
            "expected major truncation: {pages_before} -> {pages_after}"
        );
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
        // With mid splits this would be ~2x the append-split count. Scale
        // the bound with the page size (the test ran at 16 KiB pages
        // historically; the default is now 8 KiB, so ~2x the pages).
        let bound = (50 * 16384) / pager.page_size() as usize;
        let bound = bound as u32;
        assert!(
            pages < bound,
            "expected < {} pages for sequential inserts (page size {}), got {}",
            bound,
            pager.page_size(),
            pages
        );
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
            assert!(matches!(
                bt.lookup_table(i).unwrap(),
                LookupResult::Found(_)
            ));
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
            assert!(matches!(
                bt.lookup_table(i).unwrap(),
                LookupResult::Found(_)
            ));
        }
        for i in 9_000..=10_000i64 {
            assert!(matches!(
                bt.lookup_table(i).unwrap(),
                LookupResult::NotFound
            ));
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
        let pager = open_pager();
        // Create a new B+tree rooted at page 1 (allocate).
        let mut bt = Btree::create(&pager, false).unwrap();
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
        let pager = open_pager();
        let mut bt = Btree::create(&pager, false).unwrap();
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
        let pager = open_pager();
        let mut bt = Btree::create(&pager, false).unwrap();
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
        let pager = open_pager();
        let mut bt = Btree::create(&pager, false).unwrap();
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
        let pager = open_pager();
        let mut bt = Btree::create(&pager, true).unwrap();
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
        let pager = open_pager();
        let mut bt = Btree::create(&pager, true).unwrap();
        // Simple LCG shuffle of 1..=600.
        let mut vals: Vec<i64> = (1..=600).collect();
        let mut seed: u64 = 0x2545F4914F6CDD1D;
        for i in (1..vals.len()).rev() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
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

    /// walk_index_ordered visits exactly the entries in [lo, hi) — in
    /// (key, rowid) order, or its reverse — across a multi-level tree with
    /// duplicate keys and overflow-length keys, and honors early stop.
    #[test]
    fn index_ordered_walk_matches_naive_scan() {
        let pager = open_pager();
        let mut bt = Btree::create(&pager, true).unwrap();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        let mut entries: Vec<(Vec<u8>, i64)> = Vec::new();
        for rowid in 1..=3000i64 {
            let v = (next() % 400) as i64; // ~7 duplicates per key
            let key = if rowid % 97 == 0 {
                // Overflow-length TEXT key (sorts with the text family).
                Value::Text(format!("{:05}{}", v, "x".repeat(5000)).into()).encode_order_key()
            } else {
                Value::Integer(v).encode_order_key()
            };
            bt.insert_index(&key, rowid).unwrap();
            entries.push((key, rowid));
        }
        entries.sort();
        let bound = |v: i64| Value::Integer(v).encode_order_key();
        type Bounds = (Option<Vec<u8>>, Option<Vec<u8>>);
        let cases: Vec<Bounds> = vec![
            (None, None),
            (Some(bound(17)), None),
            (None, Some(bound(250))),
            (Some(bound(100)), Some(bound(101))),
            (Some(bound(399)), Some(bound(399))),
            (Some(bound(-5)), Some(bound(1000))),
            (Some(Value::Text("00200".into()).encode_order_key()), None),
        ];
        for (lo, hi) in &cases {
            let want: Vec<i64> = entries
                .iter()
                .filter(|(k, _)| {
                    lo.as_ref().map_or(true, |l| k.as_slice() >= l.as_slice())
                        && hi.as_ref().map_or(true, |h| k.as_slice() < h.as_slice())
                })
                .map(|(_, r)| *r)
                .collect();
            for desc in [false, true] {
                let mut got = Vec::new();
                bt.walk_index_ordered(lo.as_deref(), hi.as_deref(), desc, |rowid, _| {
                    got.push(rowid);
                    Ok(true)
                })
                .unwrap();
                let mut want = want.clone();
                if desc {
                    want.reverse();
                }
                assert_eq!(got, want, "lo={lo:?} hi={hi:?} desc={desc}");
                // Early stop after 5 entries.
                let mut first5 = Vec::new();
                bt.walk_index_ordered(lo.as_deref(), hi.as_deref(), desc, |rowid, _| {
                    first5.push(rowid);
                    Ok(first5.len() < 5)
                })
                .unwrap();
                assert_eq!(first5, want.iter().copied().take(5).collect::<Vec<_>>());
            }
        }
    }

    /// scan_index_from: range scans start at the first key >= start.
    #[test]
    fn index_range_scan_from() {
        let pager = open_pager();
        let mut bt = Btree::create(&pager, true).unwrap();
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
        let pager = open_pager();
        let mut bt = Btree::create(&pager, true).unwrap();
        // Insert a single entry.
        let key = Value::Integer(42).encode_order_key();
        bt.insert_index(&key, 7).unwrap();
        // Delete it.
        assert!(bt.delete_index(&key, 7).unwrap());
        // Re-insert with the same key+rowid.
        bt.insert_index(&key, 7).unwrap();
        // Lookup should return exactly one rowid, not two.
        let matches = bt.lookup_index(&key).unwrap();
        assert_eq!(
            matches,
            vec![7],
            "duplicate index entries after delete+reinsert"
        );
    }
}
