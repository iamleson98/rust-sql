//! SQL value types and row representation.
//!
//! The engine uses a tagged-union `Value` enum, similar to SQLite's
//! dynamic typing (with a few concessions for performance: integers
//! use `i64`, floats use `f64`, blobs are `Vec<u8>`).

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A single SQL value.
///
/// Ordering follows SQLite's type affinity rules:
/// NULL < INTEGER/REAL < TEXT < BLOB
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A wrapper around `&[Value]` (or any key slice) that implements `Hash`
/// and `Eq` with **SQL grouping semantics**, matching how SQLite's GROUP BY
/// / DISTINCT / UNION compare rows:
///
/// - NULL groups with NULL (SQLite's GROUP BY treats NULLs as one group).
/// - INTEGER(n) groups with REAL(n as f64) — numeric equality, so `5` and
///   `5.0` land in the same group, and `-0.0` groups with `0`.
/// - TEXT and BLOB compare bitwise (BINARY collation); text is never
///   equal to a blob even with identical bytes.
/// - A non-integral REAL hashes its exact bit pattern (two NaN-free
///   doubles that differ by any ULP are distinct groups).
///
/// The previous implementation of GROUP BY built a `format!("{:?}")` String
/// per key per row — for a 100-group scan over 10k rows that was ~10k heap
/// allocations plus Debug-formatting work. With `GroupKey` the hash map
/// keys borrow the decoded values directly: zero allocations per row.
#[derive(Debug)]
pub struct GroupKey<'a>(pub &'a [Value]);

impl PartialEq for GroupKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.0, other.0);
        if a.len() != b.len() {
            return false;
        }
        for (x, y) in a.iter().zip(b.iter()) {
            if !values_sql_equal(x, y) {
                return false;
            }
        }
        true
    }
}

impl Eq for GroupKey<'_> {}

impl Hash for GroupKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for v in self.0 {
            hash_value_sql(v, state);
        }
    }
}

/// SQL equality on two values (numeric cross-type equality, NULL == NULL).
pub fn values_sql_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        // Numeric equality across INTEGER/REAL (SQLite semantics).
        (Value::Integer(x), Value::Real(y)) | (Value::Real(y), Value::Integer(x)) => {
            *x as f64 == *y
        }
        (Value::Real(x), Value::Real(y)) => x == y,
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Blob(x), Value::Blob(y)) => x == y,
        _ => false,
    }
}

/// Hash one value consistently with `values_sql_equal`: numerics hash by
/// their (normalized-to-integer-when-exact) value so Integer(5) and
/// Real(5.0) collide, and -0.0 hashes like 0.
fn hash_value_sql<H: Hasher>(v: &Value, state: &mut H) {
    match v {
        Value::Null => state.write_u8(0),
        Value::Integer(i) => {
            state.write_u8(1);
            state.write_i64(*i);
        }
        Value::Real(f) => {
            // Normalize integral doubles to their integer hash so they
            // collide with Integer(n); normalize -0.0 to 0.
            if f.is_finite() && *f == f.trunc() && f.abs() <= 9.007_199_254_740_992e15 {
                state.write_u8(1);
                state.write_i64(*f as i64);
            } else {
                state.write_u8(2);
                state.write_u64(f.to_bits());
            }
        }
        Value::Text(s) => {
            state.write_u8(3);
            s.hash(state);
        }
        Value::Blob(b) => {
            state.write_u8(4);
            b.hash(state);
        }
    }
}

impl Value {
    /// SQL type affinity for this value.
    pub fn type_affinity(&self) -> Affinity {
        match self {
            Value::Null => Affinity::None,
            Value::Integer(_) => Affinity::Integer,
            Value::Real(_) => Affinity::Real,
            Value::Text(_) => Affinity::Text,
            Value::Blob(_) => Affinity::Blob,
        }
    }

    /// True if the value is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Coerce to i64 using SQL semantics. NULL → 0, Real → truncated,
    /// Text → parsed (0 on failure), Blob → 0.
    pub fn as_integer(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Integer(i) => *i,
            Value::Real(f) => *f as i64,
            Value::Text(s) => s.trim().parse().unwrap_or(0),
            Value::Blob(_) => 0,
        }
    }

    /// Coerce to f64. NULL → 0.0, Integer → as f64, Text → parsed.
    pub fn as_real(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Integer(i) => *i as f64,
            Value::Real(f) => *f,
            Value::Text(s) => s.trim().parse().unwrap_or(0.0),
            Value::Blob(b) => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0),
        }
    }

    /// Coerce to text.
    pub fn as_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Integer(i) => i.to_string(),
            Value::Real(f) => format_real(*f),
            Value::Text(s) => s.clone(),
            Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        }
    }

    /// Returns the byte length of the value as SQLite's `length()` function would.
    pub fn length(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 8,
            Value::Text(s) => s.chars().count() as i64,
            Value::Blob(b) => b.len() as i64,
        }
    }

    /// True if value is truthy (non-zero, non-null, non-empty).
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Integer(i) => *i != 0,
            Value::Real(f) => *f != 0.0,
            Value::Text(s) => {
                // SQLite: numeric strings are parsed; "0" is false, anything else true.
                match s.trim().parse::<f64>() {
                    Ok(f) => f != 0.0,
                    Err(_) => !s.is_empty(),
                }
            }
            Value::Blob(b) => !b.is_empty(),
        }
    }

    /// Concatenate two values as text (SQLite `||` operator).
    pub fn concat(&self, other: &Value) -> Value {
        if self.is_null() || other.is_null() {
            return Value::Null;
        }
        Value::Text(format!("{}{}", self.as_text(), other.as_text()))
    }

    /// Encode the value for B+tree storage (compact binary form).
    /// Format: 1 byte tag + payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Zero-allocation encoder: appends the encoded bytes to `out` without
    /// creating an intermediate Vec. Used by `encode_row_into` for the bulk
    /// INSERT/UPDATE hot loops, where the per-row Vec allocation (~30-50 ns
    /// each, including the malloc + free) becomes the dominant cost for
    /// small rows (e.g., `INSERT INTO t VALUES (1, 'a', 2)` — three
    /// `encode()` calls per row = ~150 ns of pure allocator overhead per row,
    /// which on 10k-row inserts is ~1.5 ms of wasted time).
    ///
    /// For larger Text/Blob values, the inner String/Vec allocation is
    /// unavoidable (we have to copy the bytes somewhere), but the outer
    /// Vec<u8> allocation is saved.
    ///
    /// ## Storage codec v2 (compact)
    ///
    /// Mirrors SQLite's record-format idea of size-classed integers and
    /// varint lengths — the old format spent 9 bytes on EVERY integer and a
    /// fixed 4-byte length prefix on every text/blob:
    ///
    ///   Null      -> [0x00]                          (1 byte)
    ///   Integer   -> [0x01..=0x05] + body             (1-9 bytes)
    ///                0x01: 0 (constant zero)
    ///                0x02: i8   0x03: i16   0x04: i32   0x05: i64 (LE)
    ///   Real      -> [0x06] + f64 LE                  (9 bytes)
    ///                [0x0A] + zigzag uvarint(i64)    (2-4 bytes) when the
    ///                double is an exact integer in ±2^53 (SQLite's
    ///                "integral REAL stored as integer" optimization —
    ///                the dominant per-row saving on REAL-heavy schemas:
    ///                9 bytes -> 2-3 for scores/amounts/prices that happen
    ///                to be whole numbers)
    ///   Text      -> [0x07] + uvarint(len) + bytes
    ///   Blob      -> [0x08] + uvarint(len) + bytes
    ///   RowidRef  -> [0x09]                           (1 byte; row-level
    ///                marker for the rowid-alias column — decoded from the
    ///                B+tree cell key, never stored)
    ///
    /// Typical OLTP row (small ints, short text) shrinks from 27-41 bytes
    /// to 6-20 bytes, directly closing the ~3.5x DB-file-size gap vs SQLite.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.push(0x00),
            Value::Integer(i) => {
                if *i == 0 {
                    out.push(0x01);
                } else if *i >= i8::MIN as i64 && *i <= i8::MAX as i64 {
                    out.push(0x02);
                    out.push(*i as i8 as u8);
                } else if *i >= i16::MIN as i64 && *i <= i16::MAX as i64 {
                    out.push(0x03);
                    out.extend_from_slice(&(*i as i16).to_le_bytes());
                } else if *i >= i32::MIN as i64 && *i <= i32::MAX as i64 {
                    out.push(0x04);
                    out.extend_from_slice(&(*i as i32).to_le_bytes());
                } else {
                    out.push(0x05);
                    out.extend_from_slice(&i.to_le_bytes());
                }
            }
            Value::Real(f) => {
                // Integral doubles within ±2^53 round-trip exactly through
                // i64: store them as a 2-4 byte zigzag varint instead of a
                // 9-byte tagged f64. -0.0 keeps the wide form (its sign
                // would be lost); NaN/inf/huge values fall through too.
                if f.is_finite()
                    && f.fract() == 0.0
                    && f.abs() <= 9_007_199_254_740_992.0 // 2^53
                    && !(*f == 0.0 && f.is_sign_negative())
                {
                    let i = *f as i64;
                    let zigzag = ((i << 1) ^ (i >> 63)) as u64;
                    out.push(0x0A);
                    encode_uvarint(zigzag, out);
                } else {
                    out.push(0x06);
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
            Value::Text(s) => {
                out.push(0x07);
                encode_uvarint(s.len() as u64, out);
                out.extend_from_slice(s.as_bytes());
            }
            Value::Blob(b) => {
                out.push(0x08);
                encode_uvarint(b.len() as u64, out);
                out.extend_from_slice(b);
            }
        }
    }

    /// Order-preserving encoding used for SECONDARY INDEX KEYS.
    ///
    /// Unlike `encode_into` (a compact storage codec), this encoding's
    /// lexicographic byte order matches `Value`'s SQL ordering exactly:
    ///
    ///   NULL < all numerics (integers AND reals interleaved numerically)
    ///       < TEXT (memcmp, shorter-prefix first) < BLOB (same)
    ///
    /// This is what allows the index B+tree — which sorts entries by raw
    /// key bytes — to serve range scans (`col > ?`) and ordered scans
    /// (`ORDER BY col`) correctly, and to binary-search for equality.
    ///
    /// Layout per value:
    ///   Null      -> [0x00]
    ///   numeric   -> [0x01] + 8-byte total-order double key
    ///                (|i| <= 2^53 integers and all reals)
    ///   large int -> [0x01] + 8-byte floor-double key + 2-byte delta
    ///                (|i| > 2^53, exact within the double bucket)
    ///   Text      -> [0x02] + BE u32 length + bytes
    ///   Blob      -> [0x03] + BE u32 length + bytes
    pub fn encode_order_key_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.push(0x00),
            Value::Integer(i) => {
                out.push(0x01);
                if i.unsigned_abs() <= (1u64 << 53) {
                    out.extend_from_slice(&double_order_key(*i as f64).to_be_bytes());
                } else {
                    // Find the largest double <= i (the bucket floor).
                    let mut lo = *i as f64; // round-to-nearest
                    if (lo as i128) > (*i as i128) {
                        // Rounded up — step down one ULP.
                        let b = lo.to_bits();
                        lo = if lo > 0.0 {
                            f64::from_bits(b - 1)
                        } else {
                            f64::from_bits(b + 1)
                        };
                    }
                    let delta = (*i as i128 - lo as i128) as u16;
                    out.extend_from_slice(&double_order_key(lo).to_be_bytes());
                    out.extend_from_slice(&delta.to_be_bytes());
                }
            }
            Value::Real(f) => {
                out.push(0x01);
                out.extend_from_slice(&double_order_key(*f).to_be_bytes());
            }
            Value::Text(s) => {
                out.push(0x02);
                out.extend_from_slice(&(s.len() as u32).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Value::Blob(b) => {
                out.push(0x03);
                out.extend_from_slice(&(b.len() as u32).to_be_bytes());
                out.extend_from_slice(b);
            }
        }
    }

    /// Order-preserving encoding (see `encode_order_key_into`).
    pub fn encode_order_key(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        self.encode_order_key_into(&mut out);
        out
    }

    /// Decode a value from bytes (storage codec v2). Returns (value, bytes
    /// consumed). The rowid marker 0x09 decodes as NULL at this level — the
    /// row-level decoder (`decode_row*`) substitutes the B+tree cell key.
    pub fn decode(buf: &[u8]) -> Result<(Value, usize), &'static str> {
        if buf.is_empty() {
            return Err("empty buffer");
        }
        let tag = buf[0];
        let rest = &buf[1..];
        match tag {
            0x00 => Ok((Value::Null, 1)),
            0x01 => Ok((Value::Integer(0), 1)),
            0x02 => {
                if rest.is_empty() {
                    return Err("truncated i8");
                }
                Ok((Value::Integer(rest[0] as i8 as i64), 2))
            }
            0x03 => {
                if rest.len() < 2 {
                    return Err("truncated i16");
                }
                let mut b = [0u8; 2];
                b.copy_from_slice(&rest[..2]);
                Ok((Value::Integer(i16::from_le_bytes(b) as i64), 3))
            }
            0x04 => {
                if rest.len() < 4 {
                    return Err("truncated i32");
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&rest[..4]);
                Ok((Value::Integer(i32::from_le_bytes(b) as i64), 5))
            }
            0x05 => {
                if rest.len() < 8 {
                    return Err("truncated i64");
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&rest[..8]);
                Ok((Value::Integer(i64::from_le_bytes(b)), 9))
            }
            0x06 => {
                if rest.len() < 8 {
                    return Err("truncated real");
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&rest[..8]);
                Ok((Value::Real(f64::from_le_bytes(b)), 9))
            }
            0x07 => {
                let (len, n) = decode_uvarint(rest)?;
                let len = len as usize;
                if rest.len() < n + len {
                    return Err("truncated text body");
                }
                let s = std::str::from_utf8(&rest[n..n + len])
                    .map_err(|_| "invalid utf8 in text")?
                    .to_string();
                Ok((Value::Text(s), 1 + n + len))
            }
            0x08 => {
                let (len, n) = decode_uvarint(rest)?;
                let len = len as usize;
                if rest.len() < n + len {
                    return Err("truncated blob body");
                }
                Ok((Value::Blob(rest[n..n + len].to_vec()), 1 + n + len))
            }
            // Rowid-alias marker: NULL at the Value level; the row decoder
            // replaces it with the cell's rowid.
            0x09 => Ok((Value::Null, 1)),
            // Integral REAL stored as zigzag varint — decodes back to
            // Real(f) with the exact same value (lossless for |v| <= 2^53).
            0x0A => {
                let (z, n) = decode_uvarint(rest)?;
                let i = ((z >> 1) as i64) ^ -((z & 1) as i64);
                Ok((Value::Real(i as f64), 1 + n))
            }
            _ => Err("unknown value tag"),
        }
    }
}

/// Encode a u64 as a LEB128 variable-length integer (1 byte for < 128,
/// up to 9 bytes for the full u64 range). Used for Text/Blob lengths in
/// the storage codec — short strings cost 1 byte instead of the old
/// fixed 4-byte length prefix.
pub fn encode_uvarint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Decode a LEB128 u64 from the front of `buf`. Returns (value, bytes
/// consumed).
pub fn decode_uvarint(buf: &[u8]) -> Result<(u64, usize), &'static str> {
    let mut n: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return Err("varint too long");
        }
        n |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok((n, i + 1));
        }
    }
    Err("truncated varint")
}

/// SQL type affinity (used in CREATE TABLE).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Real,
    Text,
    Blob,
    /// No affinity (NULL or expression result).
    None,
}

impl Affinity {
    /// SQLite's affinity rules: a column declared INTEGER gets INTEGER affinity,
    /// REAL/FLOAT/DOUBLE → REAL, CHAR/CLOB/TEXT → TEXT, BLOB or no type → BLOB.
    pub fn from_declared_type(decl: &str) -> Affinity {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            Affinity::Integer
        } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
            Affinity::Text
        } else if d.contains("BLOB") || d.is_empty() {
            Affinity::Blob
        } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            Affinity::Real
        } else {
            Affinity::Blob
        }
    }

    /// Apply affinity to a value, coercing if necessary.
    pub fn coerce(&self, v: Value) -> Value {
        match (*self, v) {
            (Affinity::Integer, Value::Integer(i)) => Value::Integer(i),
            (Affinity::Integer, Value::Real(f)) => Value::Integer(f as i64),
            (Affinity::Integer, Value::Text(s)) => {
                // SQLite affinity rules: try to parse the (trimmed) text as
                // an integer; if that fails, try as a real; if BOTH fail,
                // store the text as-is (SQLite quirk: non-numeric text
                // retains its TEXT type even in an INTEGER column).
                let trimmed = s.trim();
                if let Ok(i) = trimmed.parse::<i64>() {
                    Value::Integer(i)
                } else if let Ok(f) = trimmed.parse::<f64>() {
                    // Real-looking value in an integer column: SQLite stores
                    // it as REAL (NOT as a truncated integer).
                    Value::Real(f)
                } else {
                    Value::Text(s)
                }
            },
            (Affinity::Integer, Value::Blob(b)) => {
                let s = std::str::from_utf8(&b).unwrap_or("");
                let trimmed = s.trim();
                if let Ok(i) = trimmed.parse::<i64>() {
                    Value::Integer(i)
                } else if let Ok(f) = trimmed.parse::<f64>() {
                    Value::Real(f)
                } else {
                    Value::Blob(b)
                }
            },
            (Affinity::Integer, Value::Null) => Value::Null,

            (Affinity::Real, Value::Integer(i)) => Value::Real(i as f64),
            (Affinity::Real, Value::Real(f)) => Value::Real(f),
            (Affinity::Real, Value::Text(s)) => {
                // SQLite affinity: REAL column with TEXT input — convert if
                // the text looks like a number; otherwise keep as Text.
                let trimmed = s.trim();
                if let Ok(f) = trimmed.parse::<f64>() {
                    Value::Real(f)
                } else {
                    Value::Text(s)
                }
            }
            (Affinity::Real, Value::Blob(b)) => {
                let s = std::str::from_utf8(&b).unwrap_or("");
                let trimmed = s.trim();
                if let Ok(f) = trimmed.parse::<f64>() {
                    Value::Real(f)
                } else {
                    Value::Blob(b)
                }
            },
            (Affinity::Real, Value::Null) => Value::Null,

            (Affinity::Text, Value::Integer(i)) => Value::Text(i.to_string()),
            (Affinity::Text, Value::Real(f)) => Value::Text(format_real(f)),
            (Affinity::Text, Value::Text(s)) => Value::Text(s),
            (Affinity::Text, Value::Blob(b)) => Value::Text(String::from_utf8_lossy(&b).into_owned()),
            (Affinity::Text, Value::Null) => Value::Null,

            // BLOB and None: leave as-is
            (_, v) => v,
        }
    }
}

/// Map a finite f64 to a u64 whose big-endian byte order matches the
/// numeric order (the classic sign-flip trick):
///   negative: !bits   (more negative → larger u64)
///   positive: bits | sign bit
pub(crate) fn double_order_key(f: f64) -> u64 {
    let bits = f.to_bits();
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | 0x8000_0000_0000_0000
    }
}

/// SQL ordering semantics: NULL sorts first.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        use Value::*;
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Less,
            (_, Null) => Ordering::Greater,
            (Integer(a), Integer(b)) => a.cmp(b),
            (Real(a), Real(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Integer(a), Real(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
            (Real(a), Integer(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Integer(_) | Real(_), Text(_) | Blob(_)) => Ordering::Less,
            (Text(_) | Blob(_), Integer(_) | Real(_)) => Ordering::Greater,
            (Text(a), Text(b)) => a.cmp(b),
            (Blob(a), Blob(b)) => a.cmp(b),
            (Text(_), Blob(_)) => Ordering::Less,
            (Blob(_), Text(_)) => Ordering::Greater,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Integer(i) => write!(f, "{}", i),
            Value::Real(x) => write!(f, "{}", format_real(*x)),
            Value::Text(s) => write!(f, "{}", s),
            Value::Blob(b) => write!(f, "{}", String::from_utf8_lossy(b)),
        }
    }
}

/// Format a real number with SQLite-style rounding.
///
/// SQLite uses the shortest round-trippable representation: the fewest digits
/// such that parsing the resulting string back into an f64 yields a value
/// equal to the original. We try digits=1, 2, 3, … and return the first one
/// that round-trips. If none do (which can happen for subnormals / NaN), we
/// fall back to Rust's default `{}` formatter.
pub fn format_real(f: f64) -> String {
    if f.is_nan() {
        return "".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Inf".to_string() } else { "-Inf".to_string() };
    }
    if f == 0.0 {
        // SQLite prints "0.0" for both +0.0 and -0.0.
        return "0.0".to_string();
    }

    // Shortest round-trippable: try digits=0, 1, 2, …, 17 and return the
    // first one whose textual form parses back to the same f64.
    for digits in 0..=17usize {
        let candidate = format!("{:.*}", digits, f);
        if let Ok(parsed) = candidate.parse::<f64>() {
            if parsed == f {
                return normalize_real_string(candidate);
            }
        }
    }
    normalize_real_string(format!("{}", f))
}

fn normalize_real_string(s: String) -> String {
    // Ensure there's always a decimal point (SQLite quirk).
    if !s.contains('.') && !s.contains('e') && !s.contains("inf") {
        format!("{}.0", s)
    } else {
        s
    }
}

/// A row is an ordered sequence of values.
pub type Row = Vec<Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_ordering() {
        assert!(Value::Null < Value::Integer(0));
        assert!(Value::Integer(5) < Value::Integer(10));
        assert!(Value::Integer(5) < Value::Real(5.5));
        assert!(Value::Real(5.5) < Value::Text("a".to_string()));
        assert!(Value::Text("a".to_string()) < Value::Text("b".to_string()));
        assert!(Value::Text("z".to_string()) < Value::Blob(b"z".to_vec()));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let values = vec![
            Value::Null,
            Value::Integer(42),
            Value::Integer(-1_000_000),
            Value::Real(3.14159),
            Value::Text("hello".to_string()),
            Value::Text("unicode: 你好".to_string()),
            Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ];
        for v in values {
            let encoded = v.encode();
            let (decoded, n) = Value::decode(&encoded).unwrap();
            assert_eq!(n, encoded.len());
            assert_eq!(v, decoded);
        }
    }

    #[test]
    fn integral_real_compact_roundtrip() {
        // Integral doubles encode as 0x0A + zigzag varint (2-4 bytes) and
        // decode back to the exact same Real value.
        for f in [
            0.0,
            1.0,
            -1.0,
            42.0,
            -42.0,
            127.0,
            -128.0,
            32_767.0,
            -32_768.0,
            2_147_483_647.0,
            1e12,
            -1e12,
            9_007_199_254_740_992.0, // 2^53 (still compact-path, though the varint is wide)
            -9_007_199_254_740_992.0,
        ] {
            let v = Value::Real(f);
            let encoded = v.encode();
            assert!(
                encoded.len() <= 9,
                "integral real {} should never exceed the wide form, got {} bytes",
                f,
                encoded.len()
            );
            let (decoded, n) = Value::decode(&encoded).unwrap();
            assert_eq!(n, encoded.len());
            assert_eq!(v, decoded);
        }
        // Small integral reals are 2 bytes (tag + 1-byte zigzag).
        assert_eq!(Value::Real(1.0).encode().len(), 2);
        assert_eq!(Value::Real(-1.0).encode().len(), 2);
        assert_eq!(Value::Real(100.0).encode().len(), 3);
        assert_eq!(Value::Real(10_000.0).encode().len(), 4); // zigzag(10k)=20k -> 3-byte varint
        assert_eq!(Value::Real(1_000_000.0).encode().len(), 4);
    }

    #[test]
    fn non_integral_real_keeps_wide_form() {
        // Fractional / huge / NaN / inf / -0.0 values must keep the 9-byte
        // form so the roundtrip is bit-exact.
        for f in [
            3.5,
            -3.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.0,
            9_007_199_254_740_994.0, // 2^53 + 2 (not representable as i64 roundtrip? it IS integral but > 2^53)
            1e300,
        ] {
            let v = Value::Real(f);
            let encoded = v.encode();
            assert_eq!(encoded.len(), 9, "value {:?} must use the wide form", f);
            let (decoded, n) = Value::decode(&encoded).unwrap();
            assert_eq!(n, 9);
            if f.is_nan() {
                assert!(decoded.as_real().is_nan());
            } else {
                assert_eq!(decoded, v, "bit-exact roundtrip for {:?}", f);
            }
        }
    }

    #[test]
    fn affinity_coercion() {
        assert_eq!(
            Affinity::Integer.coerce(Value::Text("42".into())),
            Value::Integer(42)
        );
        assert_eq!(
            Affinity::Real.coerce(Value::Integer(7)),
            Value::Real(7.0)
        );
        assert_eq!(
            Affinity::Text.coerce(Value::Integer(7)),
            Value::Text("7".into())
        );
    }

    #[test]
    fn truthiness() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Integer(0).is_truthy());
        assert!(Value::Integer(1).is_truthy());
        assert!(!Value::Text("0".into()).is_truthy());
        assert!(Value::Text("1".into()).is_truthy());
        assert!(Value::Text("abc".into()).is_truthy());
    }
}
