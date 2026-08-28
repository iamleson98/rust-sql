//! Row codec: encode/decode rows (Vec<Value>) into B+tree payloads.
//!
//! Payload format (compact, length-prefixed):
//! ```text
//! +----------+-------------+----------+-------------+-----+
//! | value 0  | value 1     | ...      | value N-1   |     |
//! +----------+-------------+----------+-------------+-----+
//! ```
//! Each value is encoded via `Value::encode` (1 byte tag + payload).

use crate::error::Result;
use crate::types::{Affinity, Row, Value};

/// Encode a row into a byte vector.
pub fn encode_row(row: &Row) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len() * 8);
    for v in row {
        let bytes = v.encode();
        out.extend_from_slice(&bytes);
    }
    out
}

/// Encode a row into a caller-provided buffer. The buffer is cleared first
/// (capacity retained) and then refilled with the encoded bytes. This is the
/// zero-allocation fast path for hot loops that produce many rows (e.g.
/// `try_streaming_update`), mirroring `decode_row_into` for symmetry.
pub fn encode_row_into(row: &Row, out: &mut Vec<u8>) {
    out.clear();
    for v in row {
        v.encode_into(out);
    }
}

/// Decode a row from a byte slice.
pub fn decode_row(buf: &[u8], n_cols: usize) -> Result<Row> {
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
    Ok(row)
}

/// Decode a row into a caller-provided buffer. The buffer is cleared first;
/// after the call, it contains exactly `n_cols` values (padded with NULLs
/// if the encoded row was truncated).
///
/// This is a zero-allocation fast path for hot loops that consume many rows
/// (e.g. `exec_aggregate_streaming_scan`). The buffer is reused across rows,
/// avoiding the per-row `Vec::new()` + capacity allocation that `decode_row`
/// would do.
///
/// Values within the buffer are overwritten in-place: `Vec::clear()` drops
/// all existing elements (freeing any owned `String`/`Vec<u8>` data), then
/// `push` appends new values. For Integer/Real/Null columns this is free
/// (no heap allocation); for Text/Blob columns the per-value String/Vec
/// allocation still happens (same as `decode_row`), but the outer Vec is
/// reused.
pub fn decode_row_into(buf: &[u8], n_cols: usize, out: &mut Vec<Value>) -> Result<()> {
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
/// This is the hot-path decoder for aggregate/scan queries that only
/// touch a few columns of a wide table — e.g. `SELECT SUM(score) FROM t`
/// on a 10-column table skips decoding 9 columns per row, which is the
/// dominant cost for `exec_aggregate_no_group_by` and `Range scan`.
///
/// Cost: O(K + N_skip) where K = number of wanted columns and N_skip is
/// the total encoded bytes of the skipped columns. We still have to walk
/// the bytes of skipped columns (no length-prefix index in our format),
/// but we skip the `String::from_utf8` / `Vec::from_slice` allocations
/// that decode would otherwise do. For Text/Blob columns, the skip is
/// pure pointer arithmetic — no heap traffic.
pub fn decode_row_selective(
    buf: &[u8],
    n_cols_total: usize,
    col_indices: &[usize],
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
            // Decode this value.
            let (v, n) = Value::decode(&buf[pos..])
                .map_err(|e| crate::error::Error::corruption(format!("row decode: {}", e)))?;
            out[wanted_idx] = v;
            pos += n;
            wanted_idx += 1;
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
    let rest_len = buf.len() - 1;
    Ok(match tag {
        0 => 1,                                       // Null
        1 | 2 => 9,                                   // Integer / Real: 1 + 8
        3 | 4 => {
            // Text / Blob: 1 (tag) + 4 (len) + body
            if rest_len < 4 {
                return Err(crate::error::Error::corruption("truncated len prefix"));
            }
            let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
            5 + len
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
        let decoded = decode_row(&bytes, row.len()).unwrap();
        assert_eq!(row, decoded);
    }

    #[test]
    fn row_decode_pads_missing_columns() {
        let short = vec![Value::Integer(1)];
        let bytes = encode_row(&short);
        let decoded = decode_row(&bytes, 3).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], Value::Integer(1));
        assert_eq!(decoded[1], Value::Null);
        assert_eq!(decoded[2], Value::Null);
    }
}
