//! The session-extension value/record codec — a faithful transliteration
//! of SQLite's `sessionSerialLen` / `sessionSerializeValue` /
//! `sessionReadRecord` family (sqlite3session.c).
//!
//! RECORD FORMAT (one self-contained field per column, no header/data
//! separation — deliberately NOT the database record format):
//!
//!   0x00: Undefined value ("no value" placeholder)
//!   0x01: Integer — 8-byte big-endian i64
//!   0x02: Real    — 8-byte big-endian IEEE 754 f64
//!   0x03: Text    — varint byte-count + UTF-8 bytes
//!   0x04: Blob    — varint byte-count + bytes
//!   0x05: SQL NULL
//!   0xFF: "replaced" marker (rebase blobs only)
//!
//! Undefined and NULL are the single type byte; everything else carries
//! its payload inline. Varints are SQLite's big-endian record-format
//! varints.

use crate::types::Value;

/// Type bytes (`SQLITE_INTEGER` &c. — same values as sqlite3.h).
pub(crate) const TYPE_UNDEF: u8 = 0x00;
pub(crate) const TYPE_INT: u8 = 0x01;
pub(crate) const TYPE_REAL: u8 = 0x02;
pub(crate) const TYPE_TEXT: u8 = 0x03;
pub(crate) const TYPE_BLOB: u8 = 0x04;
pub(crate) const TYPE_NULL: u8 = 0x05;
pub(crate) const TYPE_REPLACED: u8 = 0xFF;

/// Session op codes (SQLITE_INSERT=18, DELETE=9, UPDATE=23).
pub const OP_INSERT: u8 = 18;
pub const OP_DELETE: u8 = 9;
pub const OP_UPDATE: u8 = 23;

/// The synthetic rowid column name rowid tables gain in changesets.
pub(crate) const SESSIONS_ROWID: &str = "_rowid_";

/// Big-endian i64 store.
pub(crate) fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Big-endian i64 load at `pos`.
pub(crate) fn get_i64(a: &[u8], pos: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&a[pos..pos + 8]);
    i64::from_be_bytes(b)
}

/// SQLite's `putVarint` — varints in changesets are the record-format
/// kind (`sessionVarintPut` defers to `putVarint32`). The 9-byte form
/// carries 7-bit groups in bytes 0..8 and the LOW 8 bits RAW in byte 8.
pub(crate) fn put_varint(buf: &mut Vec<u8>, v: u64) {
    if v <= 0x7f {
        buf.push(v as u8);
        return;
    }
    if v <= 0x00ff_ffff_ffff_ffff {
        let mut tmp = [0u8; 8];
        let mut n = 0;
        let mut x = v;
        while x != 0 {
            tmp[n] = (x & 0x7f) as u8;
            x >>= 7;
            n += 1;
        }
        for i in (0..n).rev() {
            buf.push(if i == 0 { tmp[i] } else { tmp[i] | 0x80 });
        }
        return;
    }
    // 9-byte form (putVarint64): byte 8 carries v's LOW 8 bits raw;
    // bytes 0..8 carry the remaining v>>8 as eight 7-bit groups, MSB
    // group first (shifts 57, 50, ..., 8).
    for s in [57, 50, 43, 36, 29, 22, 15, 8] {
        buf.push((0x80 | ((v >> s) & 0x7f)) as u8);
    }
    buf.push((v & 0xff) as u8);
}

/// Decode a varint at `pos`; returns `(value, bytes_consumed)`.
/// Total (never panics): a truncated varint reads as far as the buffer
/// allows — the caller's length checks reject the truncated record.
pub(crate) fn get_varint(a: &[u8], pos: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    let end = a.len().min(pos + 9);
    let mut i = pos;
    while i < end {
        let b = a[i];
        if i - pos == 8 {
            // 9-byte form: the last byte is raw.
            return ((v << 8) | b as u64, 9);
        }
        v = (v << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return (v, i - pos + 1);
        }
        i += 1;
    }
    (v, end - pos)
}

/// `sessionSerialLen`: bytes occupied by one field (type byte included).
/// `0x00`/`0xFF` and NULL are one byte; INT/REAL nine; TEXT/BLOB one
/// varint header + payload.
pub(crate) fn serial_len(a: &[u8], pos: usize) -> usize {
    match a[pos] {
        TYPE_UNDEF | TYPE_REPLACED | TYPE_NULL => 1,
        TYPE_INT | TYPE_REAL => 9,
        _ => {
            let (n, hdr) = get_varint(a, pos + 1);
            hdr + 1 + n as usize
        }
    }
}

/// Serialize one engine value into `out` (`sessionSerializeValue`).
/// `None` (no value available) serializes as `0x00` undefined — what the
/// capture path writes for an INSERT record's non-PK fields.
pub(crate) fn append_value(out: &mut Vec<u8>, v: Option<&Value>) {
    match v {
        None => out.push(TYPE_UNDEF),
        Some(Value::Null) => out.push(TYPE_NULL),
        Some(Value::Integer(i)) => {
            out.push(TYPE_INT);
            put_i64(out, *i);
        }
        Some(Value::Real(r)) => {
            out.push(TYPE_REAL);
            put_i64(out, r.to_bits() as i64);
        }
        Some(Value::Text(t)) => {
            let s = t.as_str();
            out.push(TYPE_TEXT);
            put_varint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Some(Value::Blob(b)) => {
            out.push(TYPE_BLOB);
            put_varint(out, b.len() as u64);
            out.extend_from_slice(b);
        }
    }
}

/// One parsed changeset field (the public iterator/value surface).
#[derive(Clone, Debug, PartialEq)]
pub enum SessVal {
    /// 0x00 — "no value" (the changeset's undefined placeholder).
    Undef,
    /// 0xFF — "replaced" (rebase blobs).
    Replaced,
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl SessVal {
    /// The engine value for this field. `None` for Undef/Replaced — the
    /// apply path treats binding those as a corrupt changeset (SQLite's
    /// `sessionBindRow` raises SQLITE_CORRUPT for a NULL
    /// `sqlite3_value`).
    pub fn to_value(&self) -> Option<Value> {
        match self {
            SessVal::Undef | SessVal::Replaced => None,
            SessVal::Null => Some(Value::Null),
            SessVal::Int(i) => Some(Value::Integer(*i)),
            SessVal::Real(r) => Some(Value::Real(*r)),
            // A TEXT field in a well-formed changeset is always valid
            // UTF-8; anything else is corruption.
            SessVal::Text(t) => std::str::from_utf8(t).ok().map(|s| Value::Text(s.into())),
            SessVal::Blob(b) => Some(Value::Blob(b.clone())),
        }
    }
}

/// Parse one field at `pos`; returns the value and the consumed length.
pub(crate) fn parse_value(a: &[u8], pos: usize) -> Result<(SessVal, usize), &'static str> {
    if pos >= a.len() {
        return Err("corrupt changeset record");
    }
    let e = a[pos];
    let len = serial_len(a, pos);
    if pos + len > a.len() {
        return Err("corrupt changeset record");
    }
    let v = match e {
        TYPE_UNDEF => SessVal::Undef,
        TYPE_REPLACED => SessVal::Replaced,
        TYPE_NULL => SessVal::Null,
        TYPE_INT => SessVal::Int(get_i64(a, pos + 1)),
        TYPE_REAL => SessVal::Real(f64::from_bits(get_i64(a, pos + 1) as u64)),
        TYPE_TEXT => {
            let (n, hdr) = get_varint(a, pos + 1);
            let start = pos + 1 + hdr;
            if start + n as usize > a.len() {
                return Err("corrupt changeset record");
            }
            SessVal::Text(a[start..start + n as usize].to_vec())
        }
        TYPE_BLOB => {
            let (n, hdr) = get_varint(a, pos + 1);
            let start = pos + 1 + hdr;
            if start + n as usize > a.len() {
                return Err("corrupt changeset record");
            }
            SessVal::Blob(a[start..start + n as usize].to_vec())
        }
        _ => return Err("corrupt changeset record"),
    };
    Ok((v, len))
}

/// Skip one record of `n` fields starting at `pos` (`sessionSkipRecord`).
pub(crate) fn skip_record(a: &[u8], mut pos: usize, n: usize) -> usize {
    for _ in 0..n {
        pos += serial_len(a, pos);
    }
    pos
}

/// Parse a whole record of `n` fields.
pub(crate) fn parse_record(
    a: &[u8],
    pos: usize,
    n: usize,
) -> Result<(Vec<SessVal>, usize), &'static str> {
    let mut out = Vec::with_capacity(n);
    let mut p = pos;
    for _ in 0..n {
        let (v, l) = parse_value(a, p)?;
        out.push(v);
        p += l;
    }
    Ok((out, p - pos))
}

/// The session hash step (`HASH_APPEND`).
#[inline]
fn hash_append(h: u32, add: u32) -> u32 {
    (h << 3) ^ h ^ add
}

/// `sessionHashAppendI64` — two 32-bit words, low then high.
pub(crate) fn hash_i64(mut h: u32, i: i64) -> u32 {
    h = hash_append(h, (i & 0xffff_ffff) as u32);
    hash_append(h, ((i >> 32) & 0xffff_ffff) as u32)
}

/// `sessionHashAppendBlob` — bytes only (length NOT hashed).
pub(crate) fn hash_blob(mut h: u32, z: &[u8]) -> u32 {
    for &b in z {
        h = hash_append(h, b as u32);
    }
    h
}

/// `sessionHashAppendType`.
pub(crate) fn hash_type(h: u32, e_type: u8) -> u32 {
    hash_append(h, e_type as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        let vals = [
            0u64,
            1,
            0x7f,
            0x80,
            0x3fff,
            0x4000,
            0xffff_ffff,
            0x1_0000_0000,
            0x00ff_ffff_ffff_ffff,
            0x0100_0000_0000_0000,
            u64::MAX,
            0x00ff_ffff_ffff_ffff + 1,
        ];
        for &v in &vals {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            let (got, used) = get_varint(&b, 0);
            assert_eq!(used, b.len(), "len for {v}");
            assert_eq!(got, v, "value for {v}");
        }
    }

    #[test]
    fn value_serialization_round_trip() {
        let vals = vec![
            Value::Null,
            Value::Integer(i64::MIN),
            Value::Integer(-1),
            Value::Integer(0),
            Value::Integer(i64::MAX),
            Value::Real(3.5),
            Value::Real(-0.0),
            Value::Text("héllo".into()),
            Value::Text(String::new().into()),
            Value::Blob(vec![1, 2, 3]),
            Value::Blob(Vec::new()),
        ];
        for v in &vals {
            let mut b = Vec::new();
            append_value(&mut b, Some(v));
            let (got, used) = parse_value(&b, 0).unwrap();
            assert_eq!(used, b.len());
            assert_eq!(got.to_value().unwrap(), *v);
        }
        // Undefined placeholder
        let mut b = Vec::new();
        append_value(&mut b, None);
        assert_eq!(b, vec![TYPE_UNDEF]);
        assert_eq!(serial_len(&b, 0), 1);
    }

    #[test]
    fn serial_len_matches_written() {
        let mut b = Vec::new();
        append_value(&mut b, Some(&Value::Text("abc".into())));
        assert_eq!(serial_len(&b, 0), b.len());
        b.clear();
        append_value(&mut b, Some(&Value::Integer(7)));
        assert_eq!(serial_len(&b, 0), 9);
    }

    #[test]
    fn hash_matches_sqlite_shape() {
        // HASH_APPEND(0, x) = (0<<3)^0^x = x — the first append is the
        // raw value; verify the composition shape for a rowid of 1:
        // h = HASH_APPEND(HASH_APPEND(0, 1), 0) = (1<<3)^1^0 = 9.
        assert_eq!(hash_i64(0, 1), 9);
    }
}
