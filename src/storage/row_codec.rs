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

/// Apply column affinities to a row in place.
pub fn apply_affinities(row: &mut Row, affinities: &[Affinity]) {
    for (v, aff) in row.iter_mut().zip(affinities.iter()) {
        if let Some(coerced) = affinity_apply_opt(*aff, v.clone()) {
            *v = coerced;
        }
    }
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
