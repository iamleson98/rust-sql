//! SQLite record (row) format codec — fileformat2 §2.1 "Record Format".
//!
//! A record is `[varint header_size][varint serial_type × N][bodies…]`.
//! Serial types:
//!   0 = NULL              1..6 = big-endian ints (1,2,3,4,6,8 bytes)
//!   7 = IEEE f64 (BE)     8 = integer 0     9 = integer 1
//!   10, 11 = reserved
//!   N ≥ 12, even = BLOB of (N-12)/2 bytes
//!   N ≥ 13, odd  = TEXT of (N-13)/2 bytes

use super::varint::{read_varint, write_varint};
use crate::types::value::Value;

/// Text encoding of a SQLite file — fileformat2 header field 56.
/// 1 = UTF-8, 2 = UTF-16le, 3 = UTF-16be. Every TEXT value in the file
/// (records AND the sqlite_schema texts) uses this encoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextEnc {
    #[default]
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl TextEnc {
    /// Header field value (1/2/3).
    pub fn as_u32(self) -> u32 {
        match self {
            TextEnc::Utf8 => 1,
            TextEnc::Utf16Le => 2,
            TextEnc::Utf16Be => 3,
        }
    }

    /// True for UTF-8 (the engine's native text encoding).
    pub fn is_utf8(self) -> bool {
        self == TextEnc::Utf8
    }

    /// From header field value; 0 (pre-header-format files) means UTF-8.
    pub fn from_u32(v: u32) -> Result<Self, String> {
        match v {
            0 | 1 => Ok(TextEnc::Utf8),
            2 => Ok(TextEnc::Utf16Le),
            3 => Ok(TextEnc::Utf16Be),
            other => Err(format!("invalid text encoding {other}")),
        }
    }

    /// The string `PRAGMA encoding` reports (SQLite's exact spellings).
    pub fn pragma_name(self) -> &'static str {
        match self {
            TextEnc::Utf8 => "UTF-8",
            TextEnc::Utf16Le => "UTF-16le",
            TextEnc::Utf16Be => "UTF-16be",
        }
    }
}

/// BINARY-collation TEXT ordering under the FILE's encoding — what
/// SQLite's `sqlite3MemCompare` does for same-class TEXT values: memcmp
/// over the raw encoded bytes. UTF-8 gives code-point order; UTF-16be
/// gives code-unit order; UTF-16le gives raw little-endian BYTE order
/// (neither code-unit nor code-point — e.g. U+FFFD sorts between
/// surrogate-pair high and low halves). A memcmp-equal prefix ties by
/// encoded length. This is the exact comparator the file writer uses
/// for b-tree keys (`writer::compare_values`) and the one the executor
/// mirrors for ORDER BY / min / max on non-UTF-8 files.
pub fn text_order_cmp(a: &str, b: &str, enc: TextEnc) -> std::cmp::Ordering {
    use std::cmp::Ordering as O;
    if enc == TextEnc::Utf8 {
        return a.cmp(b);
    }
    let be = enc == TextEnc::Utf16Be;
    let xa = a.encode_utf16();
    let xb = b.encode_utf16();
    for (u, v) in xa.zip(xb) {
        let (ub, vb) = if be {
            (u.to_be_bytes(), v.to_be_bytes())
        } else {
            (u.to_le_bytes(), v.to_le_bytes())
        };
        let c = ub.cmp(&vb);
        if c != O::Equal {
            return c;
        }
    }
    // Prefix tie: shorter text first (memcmp + length, the SQLite rule).
    let (an, bn) = (a.encode_utf16().count(), b.encode_utf16().count());
    an.cmp(&bn)
}

/// A TEXT value's bytes in the file's encoding — what
/// `CAST(text AS BLOB)` yields on a non-UTF-8 file (SQLite returns the
/// database encoding's bytes, not UTF-8).
pub fn text_encoded_bytes(s: &str, enc: TextEnc) -> Vec<u8> {
    match enc {
        TextEnc::Utf8 => s.as_bytes().to_vec(),
        TextEnc::Utf16Le => s
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>(),
        TextEnc::Utf16Be => s
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<u8>>(),
    }
}

/// Decode a full record payload into `n_cols` values. Shorter records are
/// NULL-padded (ALTER TABLE ADD COLUMN semantics); the rowid-alias column
/// is stored as NULL on disk and resolved by the caller.
pub fn decode_record(payload: &[u8], n_cols: usize) -> Result<Vec<Value>, String> {
    decode_record_enc(payload, n_cols, TextEnc::Utf8)
}

/// Encoding-aware decode (see [`decode_record`]).
pub fn decode_record_enc(
    payload: &[u8],
    n_cols: usize,
    enc: TextEnc,
) -> Result<Vec<Value>, String> {
    let (header_size, mut off) = read_varint(payload, 0).ok_or("truncated record header")?;
    let header_size = header_size as usize;
    if header_size > payload.len() || header_size == 0 {
        return Err(format!("bad record header size {header_size}"));
    }
    // Parse serial types.
    let mut serials: Vec<u64> = Vec::with_capacity(n_cols.max(4));
    while off < header_size {
        let (st, used) = read_varint(payload, off).ok_or("truncated serial type")?;
        serials.push(st as u64);
        off += used;
    }
    if off != header_size {
        return Err("record header overrun".into());
    }
    let mut values = Vec::with_capacity(n_cols);
    let mut body = header_size;
    for st in serials {
        if st == 10 || st == 11 {
            return Err(format!("reserved serial type {st}"));
        }
        values.push(decode_value(payload, &mut body, st, enc)?);
        if values.len() == n_cols {
            break; // extra columns beyond the table's are ignored
        }
    }
    while values.len() < n_cols {
        values.push(Value::Null);
    }
    Ok(values)
}

fn decode_value(payload: &[u8], body: &mut usize, st: u64, enc: TextEnc) -> Result<Value, String> {
    let be_int = |n: usize, body: &mut usize| -> Result<Value, String> {
        let end = *body + n;
        if end > payload.len() {
            return Err("truncated integer body".into());
        }
        let mut b = [0u8; 8];
        b[8 - n..].copy_from_slice(&payload[*body..end]);
        *body = end;
        // Two's-complement sign extension for the 1/2/3/4/6-byte forms.
        if n < 8 && (b[8 - n] & 0x80) != 0 {
            b[..8 - n].fill(0xff);
        }
        Ok(Value::Integer(i64::from_be_bytes(b)))
    };
    match st {
        0 => Ok(Value::Null),
        1 => be_int(1, body),
        2 => be_int(2, body),
        3 => be_int(3, body),
        4 => be_int(4, body),
        5 => be_int(6, body),
        6 => be_int(8, body),
        7 => {
            let end = *body + 8;
            if end > payload.len() {
                return Err("truncated float body".into());
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&payload[*body..end]);
            *body = end;
            Ok(Value::Real(f64::from_bits(u64::from_be_bytes(b))))
        }
        8 => Ok(Value::Integer(0)),
        9 => Ok(Value::Integer(1)),
        st if st >= 12 && st % 2 == 0 => {
            let n = ((st - 12) / 2) as usize;
            let end = *body + n;
            if end > payload.len() {
                return Err("truncated blob body".into());
            }
            let v = Value::Blob(payload[*body..end].to_vec());
            *body = end;
            Ok(v)
        }
        st if st >= 13 => {
            let n = ((st - 13) / 2) as usize;
            let end = *body + n;
            if end > payload.len() {
                return Err("truncated text body".into());
            }
            // SQLite text is arbitrary bytes in the FILE's encoding; the
            // engine's TEXT is UTF-8. Invalid sequences (bad UTF-8,
            // unpaired surrogates, odd UTF-16 length) are repaired with a
            // lossy conversion (matching the engine's in-memory semantics
            // for imported text).
            let s = decode_text_bytes(&payload[*body..end], enc);
            *body = end;
            Ok(Value::Text(s.into()))
        }
        _ => Err(format!("invalid serial type {st}")),
    }
}

/// Compute the serial type + body bytes a value contributes to a record.
/// `alias` values (INTEGER PRIMARY KEY columns) are encoded as NULL —
/// the value lives in the cell's rowid key, not the record.
pub fn encode_record(values: &[Value]) -> Vec<u8> {
    encode_record_enc(values, TextEnc::Utf8)
}

/// Encoding-aware encode (see [`encode_record`]).
pub fn encode_record_enc(values: &[Value], enc: TextEnc) -> Vec<u8> {
    // Pass 1: sizes.
    let mut out = Vec::with_capacity(values.len() * 4 + 8);
    write_header_and_body(values, &mut out, enc);
    out
}

/// Encode with a value that is omitted entirely (the rowid-alias trick):
/// the record stores NULL at the alias position.
pub fn encode_record_aliased(values: &[Value], alias: Option<usize>) -> Vec<u8> {
    encode_record_aliased_enc(values, alias, TextEnc::Utf8)
}

/// Encoding-aware alias encode (see [`encode_record_aliased`]).
pub fn encode_record_aliased_enc(values: &[Value], alias: Option<usize>, enc: TextEnc) -> Vec<u8> {
    let mut tmp: Vec<Value> = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        if Some(i) == alias {
            tmp.push(Value::Null);
        } else {
            tmp.push(v.clone());
        }
    }
    encode_record_enc(&tmp, enc)
}

/// SQLite's int-as-real storage optimization: an integral double within
/// the exact i64 range is stored as an INTEGER serial type. (Matches the
/// engine's own zigzag-integral-REAL convention's guard, but with SQLite's
/// plain-integer storage.) -0.0, NaN and infinities keep the wide form.
fn real_is_storable_as_int(f: f64) -> bool {
    f.is_finite() && f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 // 2^53
        && !(f == 0.0 && f.is_sign_negative())
}

fn serial_type_and_size(v: &Value, enc: TextEnc) -> (u64, usize) {
    match v {
        Value::Null => (0, 0),
        Value::Integer(i) => {
            let i = *i;
            // 0 and 1 have dedicated constant serial types; the rest of
            // the ladder is the narrowest SIGNED width that fits.
            if i == 0 {
                (8, 0)
            } else if i == 1 {
                (9, 0)
            } else if (-128..128).contains(&i) {
                (1, 1)
            } else if (-32768..32768).contains(&i) {
                (2, 2)
            } else if (-8_388_608..8_388_608).contains(&i) {
                (3, 3)
            } else if (-2_147_483_648..2_147_483_648).contains(&i) {
                (4, 4)
            } else if (-140_737_488_355_328..140_737_488_355_328).contains(&i) {
                (5, 6)
            } else {
                (6, 8)
            }
        }
        // SQLite stores integral REALs as integers when possible
        // (the "int-as-real" storage optimization, schema format 4).
        Value::Real(f) if f.is_nan() => (7, 8),
        Value::Real(f) => {
            if real_is_storable_as_int(*f) {
                return serial_type_and_size(&Value::Integer(*f as i64), enc);
            }
            (7, 8)
        }
        Value::Text(t) => {
            let n = match enc {
                TextEnc::Utf8 => t.as_str().len(),
                TextEnc::Utf16Le | TextEnc::Utf16Be => t.as_str().encode_utf16().count() * 2,
            };
            (13 + 2 * n as u64, n)
        }
        Value::Blob(b) => (12 + 2 * b.len() as u64, b.len()),
    }
}

fn write_header_and_body(values: &[Value], out: &mut Vec<u8>, enc: TextEnc) {
    // First pass: header bytes.
    let mut header = Vec::with_capacity(values.len() + 4);
    for v in values {
        let (st, _) = serial_type_and_size(v, enc);
        write_varint(&mut header, st as i64);
    }
    // Total header size = 1 (for the size varint itself, may grow) + sum.
    // Compute iteratively: start assuming 1 byte for the size varint.
    let mut size_prefix = 1usize;
    loop {
        let total = size_prefix + header.len();
        let needed = varint_len(total as u64);
        if needed <= size_prefix {
            break;
        }
        size_prefix = needed;
    }
    write_varint(out, (size_prefix + header.len()) as i64);
    debug_assert_eq!(varint_len((size_prefix + header.len()) as u64), size_prefix);
    out.extend_from_slice(&header);
    for v in values {
        match v {
            Value::Null => {}
            Value::Integer(i) => {
                let (_, n) = serial_type_and_size(v, enc);
                if n > 0 {
                    write_be_int(out, *i, n);
                }
            }
            Value::Real(f) => {
                if real_is_storable_as_int(*f) {
                    let (_, n) = serial_type_and_size(&Value::Integer(*f as i64), enc);
                    if n > 0 {
                        write_be_int(out, *f as i64, n);
                    }
                } else {
                    out.extend_from_slice(&f.to_bits().to_be_bytes());
                }
            }
            Value::Text(t) => match enc {
                TextEnc::Utf8 => out.extend_from_slice(t.as_str().as_bytes()),
                TextEnc::Utf16Le | TextEnc::Utf16Be => {
                    out.extend_from_slice(&encode_text_bytes(t.as_str(), enc))
                }
            },
            Value::Blob(b) => out.extend_from_slice(b),
        }
    }
}

fn write_be_int(out: &mut Vec<u8>, v: i64, n: usize) {
    let be = v.to_be_bytes(); // always 8 bytes
    out.extend_from_slice(&be[8 - n..]);
}

/// Decode raw file-encoded TEXT bytes into engine UTF-8 text (lossy on
/// invalid input — unpaired surrogates become U+FFFD, odd-length UTF-16
/// drops the trailing byte, matching SQLite's read-anything behavior).
fn decode_text_bytes(bytes: &[u8], enc: TextEnc) -> String {
    match enc {
        TextEnc::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        TextEnc::Utf16Le | TextEnc::Utf16Be => {
            let be = enc == TextEnc::Utf16Be;
            let n = bytes.len() / 2; // an odd trailing byte is corruption
            let mut units = Vec::with_capacity(n);
            for i in 0..n {
                let (b0, b1) = if be {
                    (bytes[i * 2], bytes[i * 2 + 1])
                } else {
                    (bytes[i * 2 + 1], bytes[i * 2])
                };
                units.push(u16::from_be_bytes([b0, b1]));
            }
            String::from_utf16_lossy(&units)
        }
    }
}

/// Encode engine text into the file's TEXT encoding.
fn encode_text_bytes(s: &str, enc: TextEnc) -> Vec<u8> {
    match enc {
        TextEnc::Utf8 => s.as_bytes().to_vec(),
        TextEnc::Utf16Le => s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect(),
        TextEnc::Utf16Be => s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect(),
    }
}

#[inline]
fn varint_len(v: u64) -> usize {
    let mut n = 1;
    let mut x = v >> 7;
    while x > 0 {
        n += 1;
        x >>= 7;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(vals: Vec<Value>) {
        let rec = encode_record(&vals);
        let dec = decode_record(&rec, vals.len()).unwrap();
        // Ints encoded as 0/1 constants decode back to the same integer.
        assert_eq!(dec.len(), vals.len());
        for (a, b) in vals.iter().zip(dec.iter()) {
            match (a, b) {
                (Value::Real(x), Value::Real(y)) => {
                    assert!(x.to_bits() == y.to_bits() || (x == y && x % 1.0 != 0.0))
                }
                _ => {
                    let na = dbg_norm(a.clone());
                    let nb = dbg_norm(b.clone());
                    assert_eq!(na, nb);
                }
            }
        }
    }

    fn dbg_norm(v: Value) -> Value {
        v
    }

    #[test]
    fn roundtrip_all_types() {
        rt(vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(127),
            Value::Integer(128),
            Value::Integer(-1),
            Value::Integer(300),
            Value::Integer(i32::MAX as i64),
            Value::Integer(i64::MIN),
            Value::Real(3.25),
            Value::Text("héllo wörld".into()),
            Value::Blob(vec![0u8, 1, 2, 255]),
        ]);
    }

    #[test]
    fn integral_real_stored_as_int() {
        let rec = encode_record(&[Value::Real(2.0)]);
        // serial type 1 (1-byte int 2)
        assert_eq!(rec[1], 1);
        let dec = decode_record(&rec, 1).unwrap();
        match &dec[0] {
            Value::Integer(2) => {}
            other => panic!("expected Integer(2), got {other:?}"),
        }
    }

    #[test]
    fn short_record_null_padded() {
        // 1 value record, decode as 3 columns.
        let rec = encode_record(&[Value::Integer(7)]);
        let dec = decode_record(&rec, 3).unwrap();
        assert_eq!(dec.len(), 3);
        assert_eq!(dec[1], Value::Null);
        assert_eq!(dec[2], Value::Null);
    }
}
