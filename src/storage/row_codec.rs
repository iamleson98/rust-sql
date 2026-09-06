//! Row codec: encode/decode rows (Vec<Value>) into B+tree payloads.
//!
//! ## Storage codec v2 (compact)
//!
//! Each value is encoded via `Value::encode_into` (size-classed integers,
//! varint lengths — see its docs). On top of that, the ROW encoder elides
//! the **rowid-alias column** (`id INTEGER PRIMARY KEY`): its position is
//! written as a single `0x09` marker byte and the value is never stored —
//! exactly like SQLite, where the INTEGER PRIMARY KEY column is the B+tree
//! key itself and the record holds a NULL for it. Decoders take the cell's
//! rowid and the alias column index and materialize `Integer(rowid)` at
//! that position.
//!
//! Per-row savings for the canonical OLTP row `(id INTEGER PRIMARY KEY,
//! name TEXT, val INTEGER, score REAL)`: 41 bytes → ~24 bytes, which
//! together with append-mode B+tree splits closes the ~3.5x DB-file-size
//! gap vs SQLite.

use crate::error::Result;
use crate::types::{Affinity, Row, Value};

/// Tag byte marking the rowid-alias column (decoded from the B+tree cell
/// key, never stored in the payload).
pub const ROWID_MARKER: u8 = 0x09;

/// Encode a row into a byte vector (no rowid-alias elision — the schema
/// table and other internal rows use this).
pub fn encode_row(row: &Row) -> Vec<u8> {
    let mut out = Vec::with_capacity(estimate_row_size(row));
    for v in row {
        v.encode_into(&mut out);
    }
    out
}

/// Encode a row into a caller-provided buffer (zero-allocation fast path).
/// The buffer is cleared first (capacity retained).
pub fn encode_row_into(row: &Row, out: &mut Vec<u8>) {
    out.clear();
    // Exact up-front reservation: push-driven growth pays a realloc
    // doubling cascade for wide rows (a 64 KB blob over a fresh small
    // buffer = ~13 reallocs, ~2x the payload in extra memcpy).
    let mut need = 0usize;
    for v in row {
        need += v.encoded_size();
    }
    out.reserve(need);
    for v in row {
        v.encode_into(out);
    }
}

/// Encode a row, eliding the rowid-alias column (if any) to a single
/// `0x09` marker byte. This is the table-row encoder: the alias column's
/// value lives in the B+tree cell key, so storing it again is pure waste
/// (9 bytes per row in the old fixed-width format).
pub fn encode_row_aliased_into(row: &Row, alias: Option<usize>, out: &mut Vec<u8>) {
    out.clear();
    // Exact up-front reservation (see encode_row_into): the alias column
    // contributes its 1-byte marker, everything else its encoded_size.
    let mut need = 0usize;
    for (i, v) in row.iter().enumerate() {
        need += if alias == Some(i) {
            1
        } else {
            v.encoded_size()
        };
    }
    out.reserve(need);
    match alias {
        Some(a) if a < row.len() => {
            for (i, v) in row.iter().enumerate() {
                if i == a {
                    out.push(ROWID_MARKER);
                } else {
                    v.encode_into(out);
                }
            }
        }
        _ => {
            for v in row {
                v.encode_into(out);
            }
        }
    }
}

/// Encode a row, eliding the rowid-alias column. Allocating convenience
/// wrapper around `encode_row_aliased_into`.
pub fn encode_row_aliased(row: &Row, alias: Option<usize>) -> Vec<u8> {
    let mut out = Vec::with_capacity(estimate_row_size(row));
    encode_row_aliased_into(row, alias, &mut out);
    out
}

/// Exact encoded payload size (sum of per-value sizes; the alias column
/// collapses to its 1-byte marker, so the estimate is an upper bound
/// there). Lets callers `Vec::with_capacity` ONCE: a 64 KB blob over the
/// old `row.len() * 4` initial capacity paid a ~13-realloc doubling
/// cascade (~128 KB extra memcpy) per insert.
#[inline]
fn estimate_row_size(row: &Row) -> usize {
    let mut n = 0usize;
    for v in row {
        n += v.encoded_size();
    }
    // 16-byte floor: empty/tiny rows keep a little slack so the hot OLTP
    // path doesn't allocate 4-6 byte buffers (mimalloc size classes make
    // sub-16-byte allocations the same cost anyway).
    n.max(16)
}

// ============================================================================
// Spill direct-write shape (blob-table INSERT fast path)
// ============================================================================
//
// A spilled cell's payload is `[local prefix][overflow chain]`. For the
// canonical blob-table shape — small leading columns plus ONE trailing
// huge BLOB/TEXT — the payload is exactly `[small prefix][huge body]`,
// and the body is already contiguous in the source `Value`'s own
// buffer. Detecting that shape lets the INSERT path hand the B+tree the
// body slice directly: the overflow chain is written straight from the
// value's storage and the full payload NEVER materializes in an
// intermediate encode buffer (one full-body memcpy per row saved, plus
// the buffer's allocation).

/// Maximum prefix the direct-write path accepts (bytes): everything
/// before the huge body must be genuinely small so the in-cell prefix
/// write stays a single cheap copy and the shape check stays O(columns).
pub const SPILL_MAX_PREFIX: usize = 512;

/// Detect the direct-write spill shape: the row's encoded payload
/// exceeds `max_cell` AND its LAST non-alias column is a single huge
/// BLOB/TEXT whose body dominates the payload.
///
/// Returns `(tail_col, prefix_len, total)` where the full payload is
/// exactly `[prefix][body]`: `prefix` = the encodings of all earlier
/// columns (alias elided to its marker) + the huge column's
/// tag-and-length varints, and `body` = the huge column's bytes
/// (available as a slice from the `Value` itself).
pub fn spill_tail_shape(
    row: &Row,
    alias: Option<usize>,
    max_cell: usize,
) -> Option<(usize, usize, usize)> {
    let tail = row.len().checked_sub(1)?;
    if alias == Some(tail) {
        return None; // trailing column is the rowid marker — no body to hand off
    }
    let body = match &row[tail] {
        Value::Blob(b) => b.len(),
        Value::Text(t) => t.as_bytes().len(),
        _ => return None,
    };
    // Conservative spill test: the body alone overflowing the in-cell
    // max guarantees the payload spills (total >= body).
    if body <= max_cell {
        return None;
    }
    let mut total = 0usize;
    for (i, v) in row.iter().enumerate() {
        total += if alias == Some(i) {
            1
        } else {
            v.encoded_size()
        };
    }
    let prefix_len = total - body;
    if prefix_len > SPILL_MAX_PREFIX {
        return None; // many/wide leading columns: keep the buffered path
    }
    Some((tail, prefix_len, total))
}

/// Encode ONLY the payload prefix of a spill-shaped row: every column's
/// encoding except the tail column's body, with the tail column reduced
/// to its tag + length varints. Concatenating `[out][tail body]` yields
/// exactly the full `encode_row_aliased` payload.
pub fn encode_row_spill_prefix_into(
    row: &Row,
    alias: Option<usize>,
    tail_col: usize,
    out: &mut Vec<u8>,
) {
    use crate::types::value::encode_uvarint;
    out.clear();
    out.reserve(SPILL_MAX_PREFIX + 16);
    for (i, v) in row.iter().enumerate() {
        if alias == Some(i) {
            out.push(ROWID_MARKER);
            continue;
        }
        if i == tail_col {
            match v {
                Value::Blob(b) => {
                    out.push(0x08);
                    encode_uvarint(b.len() as u64, out);
                }
                Value::Text(t) => {
                    out.push(0x07);
                    encode_uvarint(t.as_bytes().len() as u64, out);
                }
                _ => v.encode_into(out),
            }
        } else {
            v.encode_into(out);
        }
    }
}

/// Fill `out[alias]` with `Integer(rowid)` — the rowid-alias column's
/// value comes from the B+tree cell key, not the payload.
#[inline]
fn materialize_alias(out: &mut [Value], alias: Option<usize>, rowid: i64) {
    if let Some(a) = alias {
        if a < out.len() {
            out[a] = Value::Integer(rowid);
        }
    }
}

/// Decode a row from a byte slice.
///
/// `rowid` is the B+tree cell key; `alias` (when `Some`) is the index of
/// the rowid-alias column, whose payload position holds a `0x09` marker
/// (or is absent from a short row) and whose value is materialized from
/// `rowid`.
pub fn decode_row(buf: &[u8], n_cols: usize, rowid: i64, alias: Option<usize>) -> Result<Row> {
    let mut row = Vec::with_capacity(n_cols);
    let mut pos = 0;
    while pos < buf.len() && row.len() < n_cols {
        let (v, n) = Value::decode(&buf[pos..])
            .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
        row.push(v);
        pos += n;
    }
    // Pad with NULLs if the row was truncated (e.g. ALTER TABLE ADD COLUMN).
    while row.len() < n_cols {
        row.push(Value::Null);
    }
    materialize_alias(&mut row, alias, rowid);
    Ok(row)
}

/// Decode a row into a caller-provided buffer (zero-allocation fast path).
/// The buffer is cleared first; after the call it contains exactly
/// `n_cols` values (padded with NULLs if the encoded row was truncated).
///
/// See `decode_row` for the `rowid`/`alias` parameters.
pub fn decode_row_into(
    buf: &[u8],
    n_cols: usize,
    rowid: i64,
    alias: Option<usize>,
    out: &mut Vec<Value>,
) -> Result<()> {
    out.clear();
    let mut pos = 0;
    while pos < buf.len() && out.len() < n_cols {
        let (v, n) = Value::decode(&buf[pos..])
            .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
        out.push(v);
        pos += n;
    }
    while out.len() < n_cols {
        out.push(Value::Null);
    }
    materialize_alias(out, alias, rowid);
    Ok(())
}

/// Apply column affinities to a row in place.
pub fn apply_affinities(row: &mut Row, affinities: &[Affinity]) {
    for (v, aff) in row.iter_mut().zip(affinities.iter()) {
        if let Some(coerced) = affinity_apply_opt(*aff, v.clone()) {
            *v = coerced;
        }
    }
}

/// Sentinel `col_indices` entry meaning "the rowid pseudo-column"
/// (`SELECT rowid` / `_rowid_` / `oid`): the value is the B+tree cell
/// key, not a payload column, so the selective decoders fill those
/// slots with `Integer(rowid)` after the payload walk. Sorting places
/// it after every real column (usize::MAX), so the walk naturally
/// skips past all payload columns first.
pub const ROWID_PROJ: usize = usize::MAX;

/// Decode only a subset of columns from a row payload.
///
/// `col_indices` is a sorted list of column indices to extract (e.g.
/// `[2, 4]` extracts columns 2 and 4). The result is placed in `out`,
/// cleared and resized to `col_indices.len()`. Columns that don't exist
/// in the encoded payload (short row) are filled with NULL.
///
/// If the rowid-alias column is among `col_indices`, its slot is filled
/// with `Integer(rowid)` (the payload holds only a marker byte there).
/// A `ROWID_PROJ` sentinel entry decodes to `Integer(rowid)` as well
/// (the pseudo-column; the same value the alias column would carry).
///
/// Cost: O(K + N_skip) where K = number of wanted columns and N_skip is
/// the total encoded bytes of the skipped columns.
pub fn decode_row_selective(
    buf: &[u8],
    n_cols_total: usize,
    col_indices: &[usize],
    rowid: i64,
    alias: Option<usize>,
    out: &mut Vec<Value>,
) -> Result<()> {
    out.clear();
    out.resize(col_indices.len(), Value::Null);

    if col_indices.is_empty() {
        return Ok(());
    }

    // The single-cursor column walk below requires `col_indices` to be in
    // ascending order. Projections arrive in SELECT order — `SELECT val,
    // name` on (id, name, val) is [2, 1] — so a non-ascending list is
    // decoded through a sorted copy plus a slot permutation. Real
    // projections are small (<= 16 columns covers everything except
    // pathological schemas), so the permutation lives on the STACK:
    // the old path allocated two Vecs (and sorted one) PER DECODED ROW —
    // ~40-60 ns of heap traffic that dominated the per-row cost of
    // reordered projections like `SELECT name, id ... WHERE id = ?`.
    // (Dedup happens implicitly: duplicate columns hit the run
    // placement below, same as before.)
    const SMALL: usize = 16;
    let mut ascending = true;
    for w in 1..col_indices.len() {
        if col_indices[w - 1] > col_indices[w] {
            ascending = false;
            break;
        }
    }

    let mut sorted_stack = [usize::MAX; SMALL];
    let mut order_stack = [usize::MAX; SMALL];
    // Initial `None` is only ever replaced in the >SMALL branch below.
    #[allow(unused_assignments)]
    let mut perm_heap: Option<(Vec<usize>, Vec<usize>)> = None;

    // Sorted index list + slot permutation for the walk. `slot_of(k)`
    // gives the output position for sorted position k.
    let indices: &[usize];
    let mut slots_ref: &[usize] = &[];
    if ascending {
        indices = col_indices;
    } else if col_indices.len() <= SMALL {
        let n = col_indices.len();
        sorted_stack[..n].copy_from_slice(col_indices);
        for (k, o) in order_stack.iter_mut().enumerate().take(n) {
            *o = k;
        }
        // Paired insertion sort: (sorted[k], order[k]) move together.
        for k in 1..n {
            let sk = sorted_stack[k];
            let ok = order_stack[k];
            let mut j = k;
            while j > 0 && sorted_stack[j - 1] > sk {
                sorted_stack[j] = sorted_stack[j - 1];
                order_stack[j] = order_stack[j - 1];
                j -= 1;
            }
            sorted_stack[j] = sk;
            order_stack[j] = ok;
        }
        indices = &sorted_stack[..n];
        slots_ref = &order_stack[..n];
    } else {
        // > 16 projected columns: the rare wide-schema case; heap perm.
        let mut order: Vec<usize> = (0..col_indices.len()).collect();
        order.sort_unstable_by_key(|&slot| col_indices[slot]);
        let sorted: Vec<usize> = order.iter().map(|&slot| col_indices[slot]).collect();
        perm_heap = Some((sorted, order));
        let (s, o) = perm_heap.as_ref().unwrap();
        indices = s;
        slots_ref = o;
    }
    // Output slot for sorted position `k` (identity when ascending).
    #[inline]
    fn slot_of(ascending: bool, slots: &[usize], k: usize) -> usize {
        if ascending {
            k
        } else {
            slots[k]
        }
    }

    // Walk through the encoded columns once. For each column index that's
    // in `indices`, decode the value and place it in the right slot of
    // `out`. For other columns, skip the bytes.
    let mut pos = 0usize;
    let mut col = 0usize;
    let mut wanted_idx = 0usize;

    // Pseudo-rowid slots (ROWID_PROJ sentinel): the sentinel sorts past
    // every payload column, so the walk below never targets it (and must
    // not try: it is not a payload column) — fill it up front instead.
    for (k, &ci) in indices.iter().enumerate() {
        if ci == ROWID_PROJ {
            out[slot_of(ascending, slots_ref, k)] = Value::Integer(rowid);
        }
    }

    while pos < buf.len() && col < n_cols_total && wanted_idx < indices.len() {
        // Advance `wanted_idx` past any indices < col.
        while wanted_idx < indices.len() && indices[wanted_idx] < col {
            wanted_idx += 1;
        }
        if wanted_idx >= indices.len() {
            break;
        }
        let target = indices[wanted_idx];

        if col == target {
            // How many sorted positions target this same column (handles
            // duplicate projections like `SELECT val, val`).
            let mut run_end = wanted_idx;
            while run_end < indices.len() && indices[run_end] == col {
                run_end += 1;
            }
            if alias == Some(col) {
                // Rowid-alias column: payload holds the 0x09 marker (1
                // byte); the value is the cell key.
                if buf[pos] != ROWID_MARKER {
                    return Err(crate::error::Error::corruption(
                        "rowid-alias column must hold the rowid marker",
                    ));
                }
                for k in wanted_idx..run_end {
                    out[slot_of(ascending, slots_ref, k)] = Value::Integer(rowid);
                }
                pos += 1;
            } else {
                let (v, n) = Value::decode(&buf[pos..])
                    .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
                if run_end - wanted_idx == 1 {
                    // Common case: one slot — move the value, no clone.
                    out[slot_of(ascending, slots_ref, wanted_idx)] = v;
                } else {
                    for k in wanted_idx..run_end {
                        out[slot_of(ascending, slots_ref, k)] = v.clone();
                    }
                }
                pos += n;
            }
            wanted_idx = run_end;
        } else if alias == Some(col) {
            // Skipping over the rowid-alias column: 1 marker byte.
            pos += 1;
        } else {
            // Skip this value: read the tag and length without allocating.
            let n = value_encoded_len(&buf[pos..])?;
            pos += n;
        }
        col += 1;
    }
    Ok(())
}

/// Compute the encoded length of a value at `buf[0..]` without allocating
/// a `Value`. Returns the total number of bytes consumed by this value
/// (tag + payload). Used by `decode_row_selective` to skip unwanted
/// columns in O(1) per-column time without heap traffic.
fn value_encoded_len(buf: &[u8]) -> Result<usize> {
    if buf.is_empty() {
        return Err(crate::error::Error::corruption("empty value"));
    }
    let tag = buf[0];
    Ok(match tag {
        0x00 | 0x01 | 0x09 => 1, // Null / zero / rowid marker
        0x02 => 2,               // i8
        0x03 => 3,               // i16
        0x04 => 5,               // i32
        0x05 | 0x06 => 9,        // i64 / f64
        0x0A => {
            // Integral REAL as zigzag varint: tag + varint.
            let rest = &buf[1..];
            let (_, n) = crate::types::value::decode_uvarint(rest)
                .map_err(crate::error::Error::corruption)?;
            1 + n
        }
        0x07 | 0x08 => {
            // Text / Blob: 1 (tag) + varint length + body.
            let rest = &buf[1..];
            let (len, n) = crate::types::value::decode_uvarint(rest)
                .map_err(crate::error::Error::corruption)?;
            1 + n + len as usize
        }
        _ => return Err(crate::error::Error::corruption("unknown value tag")),
    })
}

/// Selective decode for callers that GUARANTEE a sorted, deduplicated
/// `col_indices` (the fused hash join constructs its wanted lists that
/// way). Semantically identical to [`decode_row_selective`] — same
/// NULL-filling for short rows, same rowid-alias handling — but skips
/// the general decoder's per-call ascending probe, permutation
/// machinery, and duplicate-run handling: the per-row walk is the fused
/// join's dominant decode cost, so every branch matters.
///
/// # Panics / errors
/// Returns a corruption error on truncated or malformed payloads (the
/// caller decides whether to skip the row or fail the query).
pub fn decode_row_selective_sorted(
    buf: &[u8],
    n_cols_total: usize,
    col_indices: &[usize],
    rowid: i64,
    alias: Option<usize>,
    out: &mut Vec<Value>,
) -> Result<()> {
    debug_assert!(col_indices.windows(2).all(|w| w[0] < w[1]));
    debug_assert!(!col_indices[..col_indices.len() - 1].contains(&ROWID_PROJ));
    out.clear();
    out.resize(col_indices.len(), Value::Null);
    if col_indices.is_empty() {
        return Ok(());
    }
    // Pseudo-rowid slot: the cell key, not a payload column.
    if col_indices[col_indices.len() - 1] == ROWID_PROJ {
        out[col_indices.len() - 1] = Value::Integer(rowid);
        if col_indices.len() == 1 {
            return Ok(());
        }
    }
    let mut pos = 0usize;
    let mut wi = 0usize; // index into col_indices of the next wanted column
    for col in 0..n_cols_total {
        if wi >= col_indices.len() {
            break;
        }
        // Short row (fewer encoded columns than declared — ALTER ADD
        // COLUMN territory): remaining wanted columns stay NULL.
        if pos >= buf.len() {
            break;
        }
        let target = col_indices[wi];
        if col < target {
            // Unwanted column: length probe only.
            if alias == Some(col) {
                pos += 1; // rowid-alias marker byte
            } else {
                pos += value_encoded_len(&buf[pos..])?;
            }
            continue;
        }
        // col == target (col increments by one, so it cannot jump past a
        // target it was below; sorted+dedup guarantees no repeats).
        if alias == Some(col) {
            if buf[pos] != ROWID_MARKER {
                return Err(crate::error::Error::corruption(
                    "rowid-alias column must hold the rowid marker",
                ));
            }
            out[wi] = Value::Integer(rowid);
            pos += 1;
        } else {
            let (v, n) = Value::decode(&buf[pos..])
                .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
            out[wi] = v;
            pos += n;
        }
        wi += 1;
    }
    Ok(())
}

/// Decode only the wanted columns into a FULL-WIDTH row buffer
/// (`out.len() == n_cols_total`), leaving every non-wanted column as
/// `Value::Null`. This is the companion of `decode_row_selective` for
/// compiled positional expressions: they index by table column position
/// (identity layout), so the decoded slice must be full-width — but the
/// decode cost stays proportional to the wanted columns (skipped columns
/// cost only a length probe, and their slots are never read by
/// construction: the expression compiler derived `wanted` from exactly
/// the columns the expressions reference).
///
/// `wanted` must be ascending and deduplicated.
pub fn decode_row_selective_wide(
    buf: &[u8],
    n_cols_total: usize,
    wanted: &[usize],
    rowid: i64,
    alias: Option<usize>,
    out: &mut Vec<Value>,
) -> Result<()> {
    // Reset to all-Null, full width. Reuses the Vec's allocation; the
    // previous values' drops are free for Integer/Real/Null and for
    // SSO Text (no heap free), so the reset is a memset + branch per slot.
    out.clear();
    out.resize(n_cols_total, Value::Null);

    if wanted.is_empty() {
        return Ok(());
    }

    let mut pos = 0usize;
    let mut col = 0usize;
    let mut wanted_idx = 0usize;

    while pos < buf.len() && col < n_cols_total && wanted_idx < wanted.len() {
        while wanted_idx < wanted.len() && wanted[wanted_idx] < col {
            wanted_idx += 1;
        }
        if wanted_idx >= wanted.len() {
            break;
        }
        let target = wanted[wanted_idx];
        if col == target {
            if alias == Some(col) {
                if buf[pos] != ROWID_MARKER {
                    return Err(crate::error::Error::corruption(
                        "rowid-alias column must hold the rowid marker",
                    ));
                }
                out[col] = Value::Integer(rowid);
                pos += 1;
            } else {
                let (v, n) = Value::decode(&buf[pos..])
                    .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
                out[col] = v;
                pos += n;
            }
            wanted_idx += 1;
        } else if alias == Some(col) {
            pos += 1;
        } else {
            let n = value_encoded_len(&buf[pos..])?;
            pos += n;
        }
        col += 1;
    }
    Ok(())
}

/// Walk the payload's column layout, writing `(offset, encoded_len)` per
/// column into `out` (cleared first; the rowid-alias column's region is
/// its 1-byte marker). Returns false when the payload is truncated or
/// encodes fewer columns than `n_cols` (caller decides whether the
/// missing columns matter). Used by the UPDATE payload-patch fast path.
pub fn row_column_regions_into(
    payload: &[u8],
    n_cols: usize,
    alias: Option<usize>,
    out: &mut Vec<(u32, u32)>,
) -> bool {
    out.clear();
    out.reserve(n_cols);
    let mut pos = 0usize;
    for col in 0..n_cols {
        if pos >= payload.len() {
            return false; // missing column(s)
        }
        if alias == Some(col) {
            if payload[pos] != ROWID_MARKER {
                return false;
            }
            out.push((pos as u32, 1));
            pos += 1;
        } else {
            let n = match value_encoded_len(&payload[pos..]) {
                Ok(n) => n,
                Err(_) => return false,
            };
            if pos + n > payload.len() {
                return false;
            }
            out.push((pos as u32, n as u32));
            pos += n;
        }
    }
    pos == payload.len()
}

fn affinity_apply_opt(aff: Affinity, v: Value) -> Option<Value> {
    match (aff, v) {
        (Affinity::None, v) => Some(v),
        (aff, v) => Some(aff.coerce(v)),
    }
}

// ============================================================================
// Overflow-aware lazy decode support
// ============================================================================
//
// The scan family hands these helpers the PAGE-RESIDENT prefix of an
// overflow cell (the "local" bytes). The layout walker computes every
// column's byte span from that prefix alone — a projection can then skip
// the wide columns' chain pages entirely, and a projected wide column can
// be gathered DIRECTLY from its chain pages into the value's own buffer
// (one copy instead of assemble-then-decode's two).

/// Byte span of one column inside the FULL payload: `[off, off+len)`
/// covers tag + body. `off` is an offset into the payload (the local
/// prefix shares the payload's starting offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColSpan {
    /// Offset of the value's TAG byte within the payload.
    pub off: u32,
    /// Total encoded bytes (tag + header + body).
    pub len: u32,
}

impl ColSpan {
    #[inline]
    pub fn end(&self) -> usize {
        self.off as usize + self.len as usize
    }
}

/// Why a lazy decode could not proceed from the local prefix alone.
#[derive(Debug)]
pub enum LazyError {
    /// The record header walk ran past the local prefix (a record with an
    /// enormous tag run — pathological, but possible). The caller falls
    /// back to full assembly.
    HeaderPastLocal,
    /// Structurally corrupt payload.
    Corrupt(crate::error::Error),
}

/// Walk the value tags on the LOCAL prefix of a payload, laying out the
/// byte spans of columns `0..=max_col` (the caller passes its highest
/// wanted column index):
/// - `Some(span)` — the column's tag AND length header are fully local.
///   The BODY may still spill into the overflow chain (`span.end() >
///   local.len()`): the caller decides whether to gather it.
/// - `None` — the column position is past the encoded payload (short row:
///   ALTER TABLE ADD COLUMN) → NULL.
///
/// `Err(HeaderPastLocal)` — a tag needed to continue the walk sits past
/// the local prefix (a column positioned AFTER a spilled wide column).
/// The caller must assemble the full payload to decode those columns.
/// `Err(Corrupt)` — malformed tag/varint inside the local prefix.
///
/// Position arithmetic is exact: each span's length comes from the tag
/// header (in local), so walking PAST a spilled body needs no body bytes —
/// only the NEXT column's tag must be locally readable.
pub fn column_spans_local(
    local: &[u8],
    total: usize,
    n_cols_total: usize,
    max_col: usize,
) -> std::result::Result<Vec<Option<ColSpan>>, LazyError> {
    let max_col = max_col.min(n_cols_total.saturating_sub(1));
    let mut spans = Vec::with_capacity(max_col + 1);
    let mut pos = 0usize;
    for _col in 0..=max_col {
        if pos >= total {
            // Past the encoded payload: short row (NULL).
            spans.push(None);
            continue;
        }
        if pos >= local.len() {
            // The tag for this column sits inside the overflow chain: the
            // walk cannot continue locally. Only an error when a wanted
            // column needs it (the caller's max_col is wanted-derived, so
            // reaching here means one does).
            return Err(LazyError::HeaderPastLocal);
        }
        let tag = local[pos];
        // Body/header length from the tag (mirrors `value_encoded_len`).
        let (hdr, body): (usize, usize) = match tag {
            0x00 | 0x01 | 0x09 => (0, 0),
            0x02 => (0, 1),
            0x03 => (0, 2),
            0x04 => (0, 4),
            0x05 | 0x06 => (0, 8),
            0x0A => {
                let (_, n) =
                    crate::types::value::decode_uvarint(&local[pos + 1..]).map_err(|_| {
                        LazyError::Corrupt(crate::error::Error::corruption(
                            "truncated varint in lazy layout walk",
                        ))
                    })?;
                (n, 0)
            }
            0x07 | 0x08 => {
                let (len, n) =
                    crate::types::value::decode_uvarint(&local[pos + 1..]).map_err(|_| {
                        LazyError::Corrupt(crate::error::Error::corruption(
                            "truncated length varint in lazy layout walk",
                        ))
                    })?;
                (n, len as usize)
            }
            _ => {
                return Err(LazyError::Corrupt(crate::error::Error::corruption(
                    format!("unknown value tag {tag:#x} in lazy layout walk"),
                )))
            }
        };
        // The tag + length header must be local for the span to be usable;
        // the BODY may spill (that's the overflow case).
        let header_end = pos + 1 + hdr;
        if header_end > local.len() {
            return Err(LazyError::HeaderPastLocal);
        }
        let len = 1 + hdr + body;
        spans.push(Some(ColSpan {
            off: pos as u32,
            len: len as u32,
        }));
        pos += len;
    }
    Ok(spans)
}

/// Decode the single value occupying `bytes` (tag + body, contiguous).
/// A gathered span's bytes are contiguous by construction; a local span's
/// bytes are a subslice of the page-resident prefix.
pub fn decode_span_value(bytes: &[u8]) -> crate::error::Result<Value> {
    let (v, _) = Value::decode(bytes)
        .map_err(|e| crate::error::Error::corruption(format!("lazy span decode: {}", e)))?;
    Ok(v)
}

// ============================================================================
// Fused single-column probe (the range-filter scan's inner loop)
// ============================================================================

/// A probed column value: the INTEGER class decodes exactly; REAL keeps
/// its f64 (the caller compares it against i64 bounds with the exact
/// helpers below — SQLite's own mixed INTEGER/REAL comparison is exact
/// across the whole i64 range, so the probe must be too).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProbeNum {
    Int(i64),
    Real(f64),
}

/// Exact `REAL v >= INTEGER b` under SQLite's numeric comparison order.
/// For an integer `b`, `v >= b` ⟺ `floor(v) >= b`; every finite f64 in
/// [-2^63, 2^63) floors to an exactly-representable i64. NaN never
/// matches; ±inf saturate.
#[inline]
pub(crate) fn real_ge_int(v: f64, b: i64) -> bool {
    if v.is_nan() {
        return false;
    }
    if v == f64::INFINITY {
        return true;
    }
    if v == f64::NEG_INFINITY {
        return false;
    }
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    let f = v.floor();
    if f >= TWO_POW_63 {
        return true; // v numerically exceeds i64::MAX >= b
    }
    if f < -TWO_POW_63 {
        return false; // v numerically below i64::MIN <= b
    }
    (f as i64) >= b
}

/// Exact `REAL v <= INTEGER b`: the mirror of [`real_ge_int`] via the
/// ceil (`v <= b` ⟺ `ceil(v) <= b` for integer `b`).
#[inline]
pub(crate) fn real_le_int(v: f64, b: i64) -> bool {
    if v.is_nan() {
        return false;
    }
    if v == f64::NEG_INFINITY {
        return true;
    }
    if v == f64::INFINITY {
        return false;
    }
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    let c = v.ceil();
    if c >= TWO_POW_63 {
        return false; // v numerically exceeds i64::MAX >= b
    }
    if c < -TWO_POW_63 {
        return true; // v numerically below i64::MIN <= b
    }
    (c as i64) <= b
}

/// Total encoded length of the value starting at `bytes[0]` (tag first).
/// Mirrors Value::encode_into's layout; returns None on an unknown tag
/// (the caller treats the row as un-probeable). NOTE: Text/Blob/zigzag
/// lengths are the VALUE CODEC's LEB128 uvarint (types::value), NOT the
/// B+tree's SQLite-style big-endian varint — mixing them misparses every
/// multi-byte length.
fn encoded_span(bytes: &[u8]) -> Option<usize> {
    let tag = *bytes.first()?;
    match tag {
        0x00 | 0x01 | 0x09 => Some(1),
        0x02 => Some(2),
        0x03 => Some(3),
        0x04 => Some(5),
        0x05 | 0x06 => Some(9),
        0x07 | 0x08 => {
            let (len, n) = crate::types::value::decode_uvarint(&bytes[1..]).ok()?;
            Some(1 + n + len as usize)
        }
        0x0A => {
            let (_, n) = crate::types::value::decode_uvarint(&bytes[1..]).ok()?;
            Some(1 + n)
        }
        _ => None,
    }
}

/// Probe column `col` (0-based, table-column space) of a row payload
/// WITHOUT building a Row: walks the leading [tag][body] values and
/// decodes just the target. The rowid-alias column is the rowid itself
/// (its stored form is the 1-byte 0x09 marker). Returns:
/// - `Some(ProbeNum)` for INTEGER-class tags (0x01-0x05, 0x0A) and REAL,
/// - `None` for NULL / TEXT / BLOB / absent columns / corrupt walks —
///   the caller skips the row (a two-sided INTEGER range can never match
///   them: SQLite's type order puts every number below TEXT/BLOB, and
///   NULL never matches).
///
/// This is the inner loop of the fused range-filter scan: ~3-8 ns per row
/// versus ~40-60 ns for selective-decode + predicate dispatch.
pub fn probe_int_column(
    payload: &[u8],
    col: usize,
    rowid: i64,
    rowid_alias: Option<usize>,
) -> Option<ProbeNum> {
    if rowid_alias == Some(col) {
        return Some(ProbeNum::Int(rowid));
    }
    let mut off = 0usize;
    for _ in 0..col {
        let span = encoded_span(&payload[off..])?;
        off += span;
    }
    let tag = *payload.get(off)?;
    let body = off + 1;
    let read_int = |n: usize, signed: bool| -> Option<i64> {
        let raw = payload.get(body..body + n)?;
        let mut v: i64 = 0;
        for (i, &b) in raw.iter().enumerate() {
            v |= (b as i64) << (8 * i); // little-endian bodies
        }
        if signed && n < 8 {
            let bits = 8 * n;
            if (v >> (bits - 1)) & 1 == 1 {
                v -= 1i64 << bits;
            }
        }
        Some(v)
    };
    match tag {
        0x01 => Some(ProbeNum::Int(0)),
        0x02 => Some(ProbeNum::Int(read_int(1, true)?)),
        0x03 => Some(ProbeNum::Int(read_int(2, true)?)),
        0x04 => Some(ProbeNum::Int(read_int(4, true)?)),
        0x05 => Some(ProbeNum::Int(read_int(8, true)?)),
        0x06 => {
            let raw = payload.get(body..body + 8)?;
            let mut b = [0u8; 8];
            b.copy_from_slice(raw);
            Some(ProbeNum::Real(f64::from_le_bytes(b)))
        }
        0x0A => {
            // Integral REAL stored as zigzag LEB128 uvarint (the VALUE
            // codec's format — not the B+tree's big-endian varint).
            let (zz, _) = crate::types::value::decode_uvarint(&payload[body..]).ok()?;
            let v = ((zz >> 1) as i64) ^ -((zz & 1) as i64);
            Some(ProbeNum::Int(v))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_int_column_all_tags() {
        // Column 1 of a 3-column row: every INTEGER width, the zigzag
        // integral-REAL, wide REAL, NULL, TEXT — probed WITHOUT a Row.
        let cases: Vec<(Value, Option<ProbeNum>)> = vec![
            (Value::Integer(0), Some(ProbeNum::Int(0))),
            (Value::Integer(127), Some(ProbeNum::Int(127))),
            (Value::Integer(-128), Some(ProbeNum::Int(-128))),
            (Value::Integer(32_767), Some(ProbeNum::Int(32_767))),
            (Value::Integer(-32_768), Some(ProbeNum::Int(-32_768))),
            (
                Value::Integer(2_147_483_647),
                Some(ProbeNum::Int(2_147_483_647)),
            ),
            (
                Value::Integer(-2_147_483_648),
                Some(ProbeNum::Int(-2_147_483_648)),
            ),
            (Value::Integer(i64::MAX), Some(ProbeNum::Int(i64::MAX))),
            (Value::Integer(i64::MIN), Some(ProbeNum::Int(i64::MIN))),
            // zigzag compact path decodes to Int (numerator-comparable)
            (Value::Real(42.0), Some(ProbeNum::Int(42))),
            (Value::Real(-42.0), Some(ProbeNum::Int(-42))),
            (Value::Real(2.0e12), Some(ProbeNum::Int(2_000_000_000_000))),
            (Value::Real(12.5), Some(ProbeNum::Real(12.5))),
            (Value::Null, None),
            (Value::Text("x".into()), None),
            (Value::Blob(vec![1u8]), None),
        ];
        for (val, want) in cases {
            let row = vec![Value::Integer(1), val.clone(), Value::Text("t".into())];
            let bytes = encode_row(&row);
            let got = probe_int_column(&bytes, 1, 7, None);
            assert_eq!(got, want, "probe({val:?})");
        }
        // Rowid-alias position: the payload holds the 0x09 marker; the
        // probed value is the cell key, not the marker.
        let row = vec![Value::Integer(1), Value::Null, Value::Text("t".into())];
        let bytes = encode_row_aliased(&row, Some(1)); // col 1 → 0x09 marker
        assert_eq!(
            probe_int_column(&bytes, 1, 555, Some(1)),
            Some(ProbeNum::Int(555))
        );
        // Col past the stored cells (short row): absent → None.
        let short = encode_row(&vec![Value::Integer(1)]);
        assert_eq!(probe_int_column(&short, 2, 0, None), None);
    }

    #[test]
    fn real_int_comparison_exact_edges() {
        // Bit-pattern constants: 2^63 and the nearest f64 below it
        // (2^63 - 1024; ulp is 2048 there) — exact, no literal rounding.
        const P63: f64 = f64::from_bits(0x43E0_0000_0000_0000);
        const P63_M1024: f64 = f64::from_bits(0x43DF_FFFF_FFFF_FFFF);
        // 2^63 as f64: numerically ABOVE i64::MAX — every comparison
        // against any i64 bound must saturate, never wrap.
        assert!(real_ge_int(P63, i64::MAX));
        assert!(!real_le_int(P63, i64::MAX));
        // `(i64::MAX - 1) as f64` rounds UP to exactly 2^63, which compares
        // ABOVE i64::MAX (correct, see line above). Pin the true-below case:
        // floor(P63_M1024) == i64::MAX - 1023.
        assert!(real_ge_int(P63_M1024, i64::MAX - 1023)); // floor == bound
        assert!(!real_ge_int(P63_M1024, i64::MAX - 1022));
        assert!(!real_ge_int(P63_M1024, i64::MAX)); // numerically below MAX
                                                    // Integrality: 8.5 ∈ [8, 16], 7.5 ∉ [8, 16].
        assert!(real_ge_int(8.5, 8) && real_le_int(8.5, 16));
        assert!(!real_ge_int(7.5, 8));
        // -0.5 <= 0, floor(-0.5) = -1.
        assert!(real_le_int(-0.5, 0));
        assert!(!real_ge_int(-0.5, 0));
        // NaN never matches; ±inf saturate.
        assert!(!real_ge_int(f64::NAN, 0) && !real_le_int(f64::NAN, 0));
        assert!(real_ge_int(f64::INFINITY, i64::MAX));
        assert!(real_le_int(f64::NEG_INFINITY, i64::MIN));
        // -(2^63 - 1024) is representable and sits 1024 above i64::MIN.
        assert!(real_ge_int(-P63_M1024, i64::MIN + 2));
        // And exactly i64::MIN-as-f64 still >= i64::MIN (floor == MIN).
        assert!(real_ge_int(-P63, i64::MIN));
    }

    #[test]
    fn row_roundtrip() {
        let row = vec![
            Value::Integer(42),
            Value::Text("hello".into()),
            Value::Real(1.5),
            Value::Null,
            Value::Blob(vec![1, 2, 3]),
        ];
        let bytes = encode_row(&row);
        let decoded = decode_row(&bytes, row.len(), 0, None).unwrap();
        assert_eq!(row, decoded);
    }

    #[test]
    fn row_decode_pads_missing_columns() {
        let short = vec![Value::Integer(1)];
        let bytes = encode_row(&short);
        let decoded = decode_row(&bytes, 3, 0, None).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], Value::Integer(1));
        assert_eq!(decoded[1], Value::Null);
        assert_eq!(decoded[2], Value::Null);
    }

    #[test]
    fn rowid_alias_elision_roundtrip() {
        // (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)
        let row = vec![
            Value::Integer(1234),
            Value::Text("abc".into()),
            Value::Integer(-7),
        ];
        let mut buf = Vec::new();
        encode_row_aliased_into(&row, Some(0), &mut buf);
        // marker(1) + text(1+1+3) + int(2) = 8 bytes
        assert_eq!(buf.len(), 8);
        let decoded = decode_row(&buf, 3, 1234, Some(0)).unwrap();
        assert_eq!(decoded, row);
    }

    #[test]
    fn compact_integer_sizes() {
        let mut b = Vec::new();
        Value::Integer(0).encode_into(&mut b);
        assert_eq!(b.len(), 1);
        b.clear();
        Value::Integer(127).encode_into(&mut b);
        assert_eq!(b.len(), 2);
        b.clear();
        Value::Integer(-128).encode_into(&mut b);
        assert_eq!(b.len(), 2);
        b.clear();
        Value::Integer(1_000_000).encode_into(&mut b);
        assert_eq!(b.len(), 5);
        b.clear();
        Value::Integer(i64::MAX).encode_into(&mut b);
        assert_eq!(b.len(), 9);
        let (v, n) = Value::decode(&b).unwrap();
        assert_eq!(v, Value::Integer(i64::MAX));
        assert_eq!(n, 9);
    }

    #[test]
    fn compact_text_length_prefix() {
        let mut b = Vec::new();
        Value::Text("hello".into()).encode_into(&mut b);
        // tag(1) + varint len(1) + 5 = 7 bytes (was 10 with fixed u32)
        assert_eq!(b.len(), 7);
        let (v, n) = Value::decode(&b).unwrap();
        assert_eq!(v, Value::Text("hello".into()));
        assert_eq!(n, 7);
    }

    #[test]
    fn selective_decode_with_alias() {
        // Row: [id (alias), name, val] — decode only id and val.
        let row = vec![
            Value::Integer(77),
            Value::Text("xy".into()),
            Value::Integer(9),
        ];
        let mut buf = Vec::new();
        encode_row_aliased_into(&row, Some(0), &mut buf);
        let mut out = Vec::new();
        decode_row_selective(&buf, 3, &[0, 2], 77, Some(0), &mut out).unwrap();
        assert_eq!(out, vec![Value::Integer(77), Value::Integer(9)]);
    }

    #[test]
    fn selective_decode_skips_alias() {
        // Wanted columns exclude the alias: skipping must still advance
        // past the 1-byte marker.
        let row = vec![
            Value::Integer(5),
            Value::Text("name5".into()),
            Value::Integer(10),
        ];
        let mut buf = Vec::new();
        encode_row_aliased_into(&row, Some(0), &mut buf);
        let mut out = Vec::new();
        decode_row_selective(&buf, 3, &[1, 2], 5, Some(0), &mut out).unwrap();
        assert_eq!(out, vec![Value::Text("name5".into()), Value::Integer(10)]);
    }

    #[test]
    fn large_varint_text() {
        let s = "x".repeat(300);
        let row = vec![Value::Text(s.clone().into())];
        let bytes = encode_row(&row);
        // tag(1) + varint(2 bytes for 300) + 300 = 303
        assert_eq!(bytes.len(), 303);
        let decoded = decode_row(&bytes, 1, 0, None).unwrap();
        assert_eq!(decoded[0], Value::Text(s.clone().into()));
    }
}
