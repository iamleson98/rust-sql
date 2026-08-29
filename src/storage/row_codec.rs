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
    let mut out = Vec::with_capacity(row.len() * 4);
    for v in row {
        v.encode_into(&mut out);
    }
    out
}

/// Encode a row into a caller-provided buffer (zero-allocation fast path).
/// The buffer is cleared first (capacity retained).
pub fn encode_row_into(row: &Row, out: &mut Vec<u8>) {
    out.clear();
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
    let mut out = Vec::with_capacity(row.len() * 4);
    encode_row_aliased_into(row, alias, &mut out);
    out
}

/// Fill `out[alias]` with `Integer(rowid)` — the rowid-alias column's
/// value comes from the B+tree cell key, not the payload.
#[inline]
fn materialize_alias(out: &mut Vec<Value>, alias: Option<usize>, rowid: i64) {
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

/// Decode only a subset of columns from a row payload.
///
/// `col_indices` is a sorted list of column indices to extract (e.g.
/// `[2, 4]` extracts columns 2 and 4). The result is placed in `out`,
/// cleared and resized to `col_indices.len()`. Columns that don't exist
/// in the encoded payload (short row) are filled with NULL.
///
/// If the rowid-alias column is among `col_indices`, its slot is filled
/// with `Integer(rowid)` (the payload holds only a marker byte there).
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

    // Walk through the encoded columns once. For each column index that's
    // in `col_indices`, decode the value and place it in the right slot of
    // `out`. For other columns, skip the bytes.
    let mut pos = 0usize;
    let mut col = 0usize;
    let mut wanted_idx = 0usize;

    while pos < buf.len() && col < n_cols_total && wanted_idx < col_indices.len() {
        // Advance `wanted_idx` past any indices < col.
        while wanted_idx < col_indices.len() && col_indices[wanted_idx] < col {
            wanted_idx += 1;
        }
        if wanted_idx >= col_indices.len() {
            break;
        }
        let target = col_indices[wanted_idx];

        if col == target {
            if alias == Some(col) {
                // Rowid-alias column: payload holds the 0x09 marker (1
                // byte); the value is the cell key.
                if buf[pos] != ROWID_MARKER {
                    return Err(crate::error::Error::corruption(
                        "rowid-alias column must hold the rowid marker",
                    ));
                }
                out[wanted_idx] = Value::Integer(rowid);
                pos += 1;
            } else {
                let (v, n) = Value::decode(&buf[pos..])
                    .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
                out[wanted_idx] = v;
                pos += n;
            }
            wanted_idx += 1;
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
        0x00 | 0x01 | 0x09 => 1,                     // Null / zero / rowid marker
        0x02 => 2,                                   // i8
        0x03 => 3,                                   // i16
        0x04 => 5,                                   // i32
        0x05 | 0x06 => 9,                            // i64 / f64
        0x0A => {
            // Integral REAL as zigzag varint: tag + varint.
            let rest = &buf[1..];
            let (_, n) = crate::types::value::decode_uvarint(rest)
                .map_err(|e| crate::error::Error::corruption(e))?;
            1 + n
        }
        0x07 | 0x08 => {
            // Text / Blob: 1 (tag) + varint length + body.
            let rest = &buf[1..];
            let (len, n) = crate::types::value::decode_uvarint(rest)
                .map_err(|e| crate::error::Error::corruption(e))?;
            1 + n + len as usize
        }
        _ => return Err(crate::error::Error::corruption("unknown value tag")),
    })
}

fn affinity_apply_opt(aff: Affinity, v: Value) -> Option<Value> {
    match (aff, v) {
        (Affinity::None, v) => Some(v),
        (aff, v) => Some(aff.coerce(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_roundtrip() {
        let row = vec![
            Value::Integer(42),
            Value::Text("hello".into()),
            Value::Real(3.14),
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
        let row = vec![Value::Integer(1234), Value::Text("abc".into()), Value::Integer(-7)];
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
        let row = vec![Value::Integer(77), Value::Text("xy".into()), Value::Integer(9)];
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
        let row = vec![Value::Integer(5), Value::Text("name5".into()), Value::Integer(10)];
        let mut buf = Vec::new();
        encode_row_aliased_into(&row, Some(0), &mut buf);
        let mut out = Vec::new();
        decode_row_selective(&buf, 3, &[1, 2], 5, Some(0), &mut out).unwrap();
        assert_eq!(out, vec![Value::Text("name5".into()), Value::Integer(10)]);
    }

    #[test]
    fn large_varint_text() {
        let s = "x".repeat(300);
        let row = vec![Value::Text(s.clone())];
        let bytes = encode_row(&row);
        // tag(1) + varint(2 bytes for 300) + 300 = 303
        assert_eq!(bytes.len(), 303);
        let decoded = decode_row(&bytes, 1, 0, None).unwrap();
        assert_eq!(decoded[0], Value::Text(s));
    }
}
