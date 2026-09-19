//! SQL value types and row representation.
//!
//! The engine uses a tagged-union `Value` enum, similar to SQLite's
//! dynamic typing (with a few concessions for performance: integers
//! use `i64`, floats use `f64`, blobs are `Vec<u8>`).

use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::types::text::Text;

// ---------------------------------------------------------------------------
// Trusted-text decode mode
//
// In-memory stores' payloads are produced exclusively by this process's
// own encoder (every Value::Text came from a `String`), so their TEXT
// bodies are valid UTF-8 by construction. Re-validating on every decode
// costs a full extra pass over the bytes — for 2 KB TEXT scans that is
// 50-150 ns/row of pure redundancy. The scan family arms this thread-
// local for the duration of a walk over an in-memory pager; file-backed
// databases (which may be corrupt or hand-crafted) keep full validation.
// ---------------------------------------------------------------------------

std::thread_local! {
    static TEXT_DECODE_TRUSTED: Cell<bool> = const { Cell::new(false) };
}

/// Arm/disarm trusted TEXT decoding for THIS thread (the scan family
/// saves + restores around each walk). Only arm for in-memory pagers.
#[inline]
pub fn set_text_decode_trusted(trusted: bool) {
    TEXT_DECODE_TRUSTED.with(|t| t.set(trusted));
}

#[inline]
pub fn text_decode_trusted() -> bool {
    TEXT_DECODE_TRUSTED.with(|t| t.get())
}

/// A single SQL value.
///
/// Ordering follows SQLite's type affinity rules:
/// NULL < INTEGER/REAL < TEXT < BLOB
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    /// TEXT with small-string optimization: strings of up to 23 bytes
    /// are stored inline in the `Value` itself — zero heap allocation on
    /// decode (mirrors SQLite's zero-copy column reads for the common
    /// short-string case). See [`crate::types::text`].
    Text(Text),
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

/// EXACT comparison of an i64 against an f64 — SQLite's
/// `sqlite3IntFloatCompare()` (the no-long-double path), verbatim
/// semantics. Integers beyond 2^53 stay exact: `9223372036854775807 =
/// 9.2233720368547758e18` is FALSE in SQLite (the double is exactly 2^63,
/// strictly greater than i64::MAX) while a naive `(i as f64) == r` cast
/// rounds both to 2^63 and calls them equal — found by the stateful
/// differential fuzz deep-sweep (DELETE with a 2^63 REAL literal wrongly
/// matching an i64::MAX row).
///
/// Algorithm (mirrors sqlite3IntFloatCompare): r outside [-2^63, 2^63)
/// decides immediately; otherwise the truncated integer decides, and only
/// when the truncation equals i does the (exact — i is representable)
/// double form break the tie. NaN never reaches here (Value::cmp's
/// num_class orders NaN after all numbers; values_sql_equal guards it).
pub(crate) fn int_float_compare(i: i64, r: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if r < -9223372036854775808.0 {
        // Any i64 is greater than every double below -2^63.
        return Ordering::Greater;
    }
    if r >= 9223372036854775808.0 {
        // Any i64 is less than every double >= 2^63 (i64::MAX < 2^63).
        return Ordering::Less;
    }
    // Guarded range: the truncating cast is exact and in-bounds (Rust `as`
    // saturates out-of-range, but the guards above make that unreachable).
    let y = r as i64;
    match i.cmp(&y) {
        Ordering::Equal => {
            let s = i as f64;
            if s < r {
                Ordering::Less
            } else if s > r {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        ord => ord,
    }
}

/// SQL equality on two values (numeric cross-type equality, NULL == NULL).
pub fn values_sql_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        // Numeric equality across INTEGER/REAL (SQLite semantics) — the
        // EXACT int/float comparison, not a double cast (see
        // `int_float_compare`). NaN compares false to everything.
        (Value::Integer(x), Value::Real(y)) | (Value::Real(y), Value::Integer(x)) => {
            !y.is_nan() && int_float_compare(*x, *y) == std::cmp::Ordering::Equal
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
            // collide with Integer(n); normalize -0.0 to 0. The bound is
            // 2^63 (not 2^53): EXACT integral doubles up to 2^63-1 are
            // i64-equal to their Integer form (values_sql_equal's exact
            // compare), and equal values MUST hash equal — Real(2^62)
            // and Integer(2^62) are one key under SQL semantics.
            if f.is_finite() && *f == f.trunc() && f.abs() < 9223372036854775808.0 {
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
            Value::Blob(b) => std::str::from_utf8(b)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0.0),
        }
    }

    /// Coerce to text.
    pub fn as_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Integer(i) => i.to_string(),
            Value::Real(f) => format_real(*f),
            Value::Text(s) => s.as_str().to_owned(),
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

    /// Truthiness in a boolean context (WHERE / ON / HAVING / CHECK /
    /// trigger WHEN). SQLite's rules, probed empirically against 3.45+:
    /// NULL is never true; INTEGER/REAL compare against 0; TEXT is
    /// numerically coerced with the `sqlite3AtoF` PREFIX semantics
    /// (`'abc'` -> 0 = false, `'1x'` -> 1 = true, `'inf'` -> 0 = false);
    /// a non-empty BLOB is TRUE and an empty blob is FALSE (a blob is
    /// "something", not a number — `SELECT 1 WHERE x'31'` passes while
    /// `SELECT 1 WHERE x''` does not).
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Integer(i) => *i != 0,
            Value::Real(f) => *f != 0.0,
            Value::Text(s) => parse_real_prefix(s) != 0.0,
            Value::Blob(b) => !b.is_empty(),
        }
    }

    /// Concatenate two values as text (SQLite `||` operator).
    pub fn concat(&self, other: &Value) -> Value {
        if self.is_null() || other.is_null() {
            return Value::Null;
        }
        // SQLite's || result type: BLOB only when BOTH operands are
        // blobs (byte concatenation); any other pairing is TEXT with the
        // operands text-coerced. `SELECT typeof(x'41' || x'42')` is
        // "blob" — the old text-always form returned "text".
        if let (Value::Blob(a), Value::Blob(b)) = (self, other) {
            let mut bytes = Vec::with_capacity(a.len() + b.len());
            bytes.extend_from_slice(a);
            bytes.extend_from_slice(b);
            return Value::Blob(bytes);
        }
        Value::Text(format!("{}{}", self.as_text(), other.as_text()).into())
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
    /// Exact encoded size of this value under `encode_into` (tag + length
    /// header + body). A row's total estimate lets the row encoder
    /// reserve once instead of doubling through a realloc cascade — a
    /// 64 KB blob over an 8-byte initial capacity paid ~13 reallocs and
    /// ~128 KB of extra memcpy per insert.
    #[inline]
    pub fn encoded_size(&self) -> usize {
        match self {
            Value::Null => 1,
            Value::Integer(i) => {
                if *i == 0 {
                    1
                } else if *i >= i8::MIN as i64 && *i <= i8::MAX as i64 {
                    2
                } else if *i >= i16::MIN as i64 && *i <= i16::MAX as i64 {
                    3
                } else if *i >= i32::MIN as i64 && *i <= i32::MAX as i64 {
                    5
                } else {
                    9
                }
            }
            Value::Real(f) => {
                if f.is_finite()
                    && f.fract() == 0.0
                    && f.abs() <= 9_007_199_254_740_992.0
                    && !(*f == 0.0 && f.is_sign_negative())
                {
                    // 0x0A + zigzag varint (zigzag doubles the magnitude,
                    // so a |value| < 2^62-ish stays <= 9 varint bytes).
                    let i = *f as i64;
                    let zigzag = ((i << 1) ^ (i >> 63)) as u64;
                    1 + crate::types::value::uvarint_len(zigzag)
                } else {
                    9
                }
            }
            Value::Text(s) => 1 + uvarint_len(s.len() as u64) + s.len(),
            Value::Blob(b) => 1 + uvarint_len(b.len() as u64) + b.len(),
        }
    }

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
                // UNIFORM 11-byte body: [01][double-order 8][delta 2].
                // The delta is zero for |i| <= 2^53 (the double is exact)
                // and the i128 remainder beyond it. Fixed width keeps the
                // numeric family PREFIX-FREE — a 9-byte small-int key used
                // to be a strict prefix of big-int/real keys, so an
                // equality probe for 2^53 could prefix-match a cell for
                // 2^53 + k (the same false-match class the Text/Blob
                // terminator had). (lo, delta) pairs still order exactly
                // like the integers they encode.
                out.push(0x01);
                let mut lo = *i as f64; // round-to-nearest
                if (lo as i128) > (*i as i128) {
                    // Rounded up — step down one ULP to the bucket floor.
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
            Value::Real(f) => {
                // Same uniform body (delta = 0): a REAL and the INTEGER
                // it equals encode IDENTICALLY (SQLite indexes 2 and 2.0
                // as one key), and no real key prefixes another.
                out.push(0x01);
                out.extend_from_slice(&double_order_key(*f).to_be_bytes());
                out.extend_from_slice(&0u16.to_be_bytes());
            }
            Value::Text(s) => {
                // TRUE lexicographic order (SQL text comparison: memcmp,
                // then shorter-prefix-first) in a PREFIX-FREE encoding:
                // [02] escape(bytes) [00 00], where escape rewrites every
                // 0x00 payload byte to the pair (00 01). The old raw-
                // bytes-plus-single-terminator form ordered correctly but
                // was prefix-AMBIGUOUS: encode('') = [02 00] is a strict
                // prefix of encode('\0a') = [02 00 61 00], so an equality
                // probe for '' prefix-matched cells of NUL-leading
                // strings (false UNIQUE conflicts, phantom rows). The
                // escape pairs keep the terminator [00 00] un-reachable
                // from inside the payload, so no key is a prefix of
                // another — while byte-wise comparison still orders
                // exactly like memcmp + shorter-prefix-first (a 00 inside
                // the payload always continues as 01, which is less than
                // any terminator-adjacent continuation... verified by the
                // order-key property tests).
                out.push(0x02);
                escape_order_bytes_into(s.as_bytes(), out);
                out.push(0x00);
                out.push(0x00);
            }
            Value::Blob(b) => {
                // Same escape scheme (SQL blob order is memcmp,
                // shorter-prefix-first). x'' = [03 00 00] is now
                // distinguishable from x'00..' = [03 00 01 .. 00 00].
                out.push(0x03);
                escape_order_bytes_into(b, out);
                out.push(0x00);
                out.push(0x00);
            }
        }
    }

    /// Order-preserving encoding (see `encode_order_key_into`).
    pub fn encode_order_key(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        self.encode_order_key_into(&mut out);
        out
    }

    /// Slice-based order-key encoder for stack buffers: writes into `out`,
    /// returning the number of bytes written, or `None` when `out` is too
    /// small (caller falls back to a heap Vec). Avoids the per-lookup heap
    /// allocation on the index point-lookup fast path.
    pub fn encode_order_key_into_slice(&self, out: &mut [u8]) -> Option<usize> {
        let need = self.order_key_len();
        if out.len() < need {
            return None;
        }
        let o = &mut out[..need];
        match self {
            Value::Null => {
                o[0] = 0x00;
            }
            Value::Integer(i) => {
                o[0] = 0x01;
                let mut lo = *i as f64;
                if (lo as i128) > (*i as i128) {
                    let b = lo.to_bits();
                    lo = if lo > 0.0 {
                        f64::from_bits(b - 1)
                    } else {
                        f64::from_bits(b + 1)
                    };
                }
                let delta = (*i as i128 - lo as i128) as u16;
                o[1..9].copy_from_slice(&double_order_key(lo).to_be_bytes());
                o[9..11].copy_from_slice(&delta.to_be_bytes());
            }
            Value::Real(f) => {
                o[0] = 0x01;
                o[1..9].copy_from_slice(&double_order_key(*f).to_be_bytes());
                o[9..11].copy_from_slice(&0u16.to_be_bytes());
            }
            Value::Text(s) => {
                o[0] = 0x02;
                let n = escape_order_bytes_into_slice(s.as_bytes(), &mut o[1..need - 2]);
                debug_assert_eq!(n, need - 3);
                o[need - 2] = 0x00;
                o[need - 1] = 0x00;
            }
            Value::Blob(b) => {
                o[0] = 0x03;
                let n = escape_order_bytes_into_slice(b, &mut o[1..need - 2]);
                debug_assert_eq!(n, need - 3);
                o[need - 2] = 0x00;
                o[need - 1] = 0x00;
            }
        }
        Some(need)
    }

    /// Exact encoded length of the order key for this value.
    #[inline]
    pub fn order_key_len(&self) -> usize {
        match self {
            Value::Null => 1,
            // Uniform [tag][double 8][delta 2].
            Value::Integer(_) => 11,
            Value::Real(_) => 11,
            // 1 type tag + escaped bytes + 2-byte terminator; each 0x00
            // payload byte escapes to two bytes.
            Value::Text(s) => 3 + s.len() + s.as_bytes().iter().filter(|&&b| b == 0).count(),
            Value::Blob(b) => 3 + b.len() + b.iter().filter(|&&b| b == 0).count(),
        }
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
                // Small-string optimization: short payloads (the dominant
                // OLTP case) decode INLINE — no heap allocation. Longer
                // payloads spill to a heap String exactly as before.
                // Storage-layer payloads were validated as UTF-8 on write;
                // validation here is a pure corrupt-FILE defense. In-memory
                // pagers arm the trusted mode (payloads are this process's
                // own encoder output), skipping the redundant pass.
                let body = &rest[n..n + len];
                if text_decode_trusted() {
                    // SAFETY: in-memory payloads were written by `encode`
                    // from `String` values — valid UTF-8 by construction.
                    let t = unsafe { Text::from_utf8_unchecked(body) };
                    Ok((Value::Text(t), 1 + n + len))
                } else {
                    if std::str::from_utf8(body).is_err() {
                        return Err("invalid utf8 in text");
                    }
                    // SAFETY: just validated above.
                    let t = unsafe { Text::from_utf8_unchecked(body) };
                    Ok((Value::Text(t), 1 + n + len))
                }
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

/// Number of bytes `encode_uvarint` writes for `n` (1-9, LEB128-ish:
/// 7 bits per byte with a continuation bit).
#[inline]
pub fn uvarint_len(n: u64) -> usize {
    // Leading-zero-based table equivalent: (64 - lz) / 7 rounded up.
    let bits = 64 - n.leading_zeros();
    (bits as usize).div_ceil(7)
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
    /// SQLite's NUMERIC affinity — the "everything else" bucket
    /// (NUMERIC, DECIMAL, DEC, FIXED, MONEY, BOOLEAN, DATE, DATETIME, ...).
    /// Numeric-looking TEXT coerces to INTEGER or REAL (integer preferred);
    /// numbers pass through; other values keep their storage class.
    Numeric,
    /// No affinity (NULL or expression result).
    None,
}

/// SQLite REAL-affinity storage semantics for negative zero: `-0.0` is
/// normalized to `+0.0` when stored in (or read back from) a REAL column —
/// SQLite's record layer treats integral REALs as integers on disk, which
/// drops the zero's sign bit. Empirically pinned against real SQLite:
/// `INSERT INTO t(f REAL) VALUES(-0.0)` reads back `0.0`, while
/// BLOB/none-affinity columns preserve `-0.0` bit-for-bit and expression
/// results (`SELECT -0.0`, `CAST(-0.0 AS REAL)`) keep the sign.
pub(crate) fn real_stored(f: f64) -> f64 {
    if f == 0.0 {
        0.0
    } else {
        f
    }
}

/// INTEGER/NUMERIC affinity's REAL→INTEGER squeeze (SQLite
/// `sqlite3RealSameAsInt` + `applyNumericAffinity`, bTryForInt=1):
/// convert iff the value round-trips through i64 EXACTLY, is strictly
/// inside ±2^63, and is not i64::MIN. Empirical pins vs real SQLite:
/// 2^62 → integer, 2^63 (any spelling) → real, i64::MIN → real,
/// -0.0 → integer 0, 1e30/1e19 → real, 8.0/'8e0'/'8.0' → integer 8.
fn numeric_real_affinity(f: f64) -> Value {
    let i = f as i64; // saturating cast
    if f.is_finite() && i != i64::MIN && f == (i as f64) && f.abs() < 9_223_372_036_854_775_808.0 {
        Value::Integer(i)
    } else {
        Value::Real(f)
    }
}

impl Affinity {
    /// SQLite's affinity rules (datatype3.html §3.1): a column declared
    /// INT* gets INTEGER affinity, CHAR/CLOB/TEXT → TEXT, BLOB or no type →
    /// BLOB, REAL/FLOA/DOUB → REAL, and EVERYTHING ELSE → NUMERIC. The
    /// final else previously fell through to BLOB — the affinity defect
    /// that made `DECIMAL`, `NUMERIC`, `DATE`, `BOOLEAN` and `MONEY`
    /// columns blob-affine; now they are NUMERIC like real SQLite.
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
            Affinity::Numeric
        }
    }

    /// Apply affinity to a value, coercing if necessary.
    pub fn coerce(&self, v: Value) -> Value {
        match (*self, v) {
            // SQLite (datatype3 §3.1 + vdbe.c applyNumericAffinity): a
            // column with INTEGER affinity stores values EXACTLY like
            // NUMERIC affinity — real text parses to INTEGER (preferred)
            // or REAL, and an integral REAL squeezes to INTEGER iff it
            // round-trips (`f == (f as i64) as f64`) strictly inside
            // ±2^63 and not i64::MIN (sqlite3RealSameAsInt). Verified
            // against real SQLite: 5.5 stays REAL (never truncated!),
            // 8.0/'8e0'/'8.0' → 8, 2^62 → integer, ±2^63 stay REAL,
            // -0.0 → 0, 1e30 stays REAL.
            (Affinity::Integer, Value::Integer(i)) => Value::Integer(i),
            (Affinity::Integer, Value::Real(f)) => numeric_real_affinity(f),
            (Affinity::Integer, Value::Text(s)) => {
                // SQLite affinity rules: try to parse the (trimmed) text as
                // an integer; if that fails, try as a real; if BOTH fail,
                // store the text as-is (SQLite quirk: non-numeric text
                // retains its TEXT type even in an INTEGER column).
                let trimmed = s.trim();
                if let Ok(i) = trimmed.parse::<i64>() {
                    Value::Integer(i)
                } else if let Ok(f) = trimmed.parse::<f64>() {
                    numeric_real_affinity(f)
                } else {
                    Value::Text(s)
                }
            }
            // SQLite (datatype3.html §3.1): a BLOB value is NEVER
            // converted by column affinity — it stays a BLOB.
            (Affinity::Integer, Value::Blob(b)) => Value::Blob(b),
            (Affinity::Integer, Value::Null) => Value::Null,

            (Affinity::Real, Value::Integer(i)) => Value::Real(i as f64),
            (Affinity::Real, Value::Real(f)) => Value::Real(real_stored(f)),
            (Affinity::Real, Value::Text(s)) => {
                // SQLite affinity: REAL column with TEXT input — convert if
                // the text looks like a number; otherwise keep as Text.
                let trimmed = s.trim();
                if let Ok(f) = trimmed.parse::<f64>() {
                    Value::Real(real_stored(f))
                } else {
                    Value::Text(s)
                }
            }
            (Affinity::Real, Value::Blob(b)) => {
                // SQLite: BLOBs are never converted by affinity.
                Value::Blob(b)
            }
            (Affinity::Real, Value::Null) => Value::Null,

            (Affinity::Text, Value::Integer(i)) => Value::Text(i.to_string().into()),
            (Affinity::Text, Value::Real(f)) => Value::Text(format_real(f).into()),
            (Affinity::Text, Value::Text(s)) => Value::Text(s.duplicate()),
            // SQLite: a BLOB is never converted to TEXT by affinity —
            // lossy UTF-8 decoding here would corrupt every binary value
            // (sqlx stores UUIDs as 16-byte BLOBs in uuid-text columns).
            (Affinity::Text, Value::Blob(b)) => Value::Blob(b),
            (Affinity::Text, Value::Null) => Value::Null,

            // NUMERIC affinity (pinned against real SQLite 3.53 via
            // examples/probe_numeric_affinity.rs): numeric-looking text
            // coerces to INTEGER (preferred) or REAL; lossless-integral
            // REALs squeeze to INTEGER (inserting 5.0 yields typeof
            // 'integer' 5) — the squeeze bound is the round-trip rule
            // (sqlite3RealSameAsInt: 2^62 squeezes, ±2^63 never do, -0.0
            // becomes 0), NOT a 2^53 cap; non-numeric text keeps its TEXT
            // storage class; BLOBs are never converted by any affinity.
            (Affinity::Numeric, Value::Integer(i)) => Value::Integer(i),
            (Affinity::Numeric, Value::Real(f)) => numeric_real_affinity(f),
            (Affinity::Numeric, Value::Text(s)) => {
                let trimmed = s.trim();
                if let Ok(i) = trimmed.parse::<i64>() {
                    Value::Integer(i)
                } else if let Ok(f) = trimmed.parse::<f64>() {
                    // '8.0'/'8e0': SQLite's applyNumericAffinity parses the
                    // text as REAL then squeezes the integral result to
                    // INTEGER (bTryForInt = 1).
                    numeric_real_affinity(f)
                } else {
                    Value::Text(s)
                }
            }
            (Affinity::Numeric, v) => v,

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

/// Escape payload bytes for the Text/Blob ORDER-KEY encoding: every 0x00
/// becomes the pair (00 01). Together with the [00 00] terminator this
/// makes the encoded key PREFIX-FREE (no key is a strict prefix of
/// another) while preserving memcmp order + shorter-prefix-first — see
/// `Value::encode_order_key_into`.
fn escape_order_bytes_into(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        if b == 0x00 {
            out.push(0x00);
            out.push(0x01);
        } else {
            out.push(b);
        }
    }
}

/// Slice form of `escape_order_bytes_into`: writes into `out`, returning
/// the number of bytes written (`out` must be large enough — callers size
/// it via `Value::order_key_len`).
fn escape_order_bytes_into_slice(bytes: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    for &b in bytes {
        if b == 0x00 {
            out[n] = 0x00;
            out[n + 1] = 0x01;
            n += 2;
        } else {
            out[n] = b;
            n += 1;
        }
    }
    n
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
        // SQLite's ordering: NULL < numbers < NaN < text < blob — NaN
        // sorts after every finite number but before text. (The old code
        // mapped NaN comparisons to Equal, so ORDER BY interleaved NaN
        // unpredictably and DISTINCT/GROUP BY semantics were luck-based.)
        let num_class = |v: &Value| match v {
            Integer(_) => 1u8,
            Real(f) if f.is_nan() => 2,
            Real(_) => 1,
            _ => 0,
        };
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Less,
            (_, Null) => Ordering::Greater,
            // Integer×Integer compares EXACTLY on i64 (SQLite semantics
            // — the f64 route below would collapse integers beyond 2^53
            // onto equal doubles) and skips the num_class/f64 dance on
            // what is the hottest comparison shape in sorts and
            // groupings.
            (Integer(a), Integer(b)) => a.cmp(b),
            (Integer(_) | Real(_), Integer(_) | Real(_)) => {
                let (ca, cb) = (num_class(self), num_class(other));
                match ca.cmp(&cb) {
                    Ordering::Equal => match (self, other) {
                        // Mixed int/real pairs use SQLite's EXACT
                        // comparison (sqlite3IntFloatCompare): an i64 never
                        // equals a double it merely rounds to —
                        // 9223372036854775807 orders BELOW 2^63.0. The
                        // old `as_real()` cast collapsed both to 2^63 and
                        // mis-ordered every integer beyond 2^53.
                        (Integer(a), Real(b)) => int_float_compare(*a, *b),
                        (Real(a), Integer(b)) => int_float_compare(*b, *a).reverse(),
                        _ => {
                            let (a, b) = (self.as_real(), other.as_real());
                            a.partial_cmp(&b).unwrap_or(Ordering::Equal)
                        }
                    },
                    ord => ord,
                }
            }
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
        return if f > 0.0 {
            "Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    // SQLite's REAL→TEXT is C `%!.17g`: 17 significant digits, `%g`
    // fixed/scientific selection, always a decimal point
    // (`1e15` → "1000000000000000.0", `1e18` → "1.0e+18",
    // `9.223372036854776e18` → "9.2233720368547758e+18").
    format_real_sig(f, 17)
}

/// C `"%!.Ng"` — N significant digits with SQLite's fixed/scientific
/// selection (scientific when exponent < -4 or >= N) and a guaranteed
/// decimal point. N = 17 for SQL REAL→TEXT, 15 for JSON rendering
/// (SQLite's json_quote).
pub fn format_real_sig(v: f64, sig: usize) -> String {
    if v == 0.0 {
        // SQLite prints "0.0" for both +0.0 and -0.0.
        return "0.0".to_string();
    }
    let prec = sig.saturating_sub(1);
    let sci = format!("{:.*e}", prec, v.abs());
    // "1.23456789012346e17" → mantissa digits + exponent.
    let (mant, exp_s) = sci.split_once('e').expect("scientific form");
    let exp: i32 = exp_s.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let neg = v < 0.0;
    if (-4..sig as i32).contains(&exp) {
        // Fixed notation: strip trailing zeros, at least one fractional
        // digit.
        let nd = digits.len() as i32;
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if exp >= 0 {
            let int_len = exp + 1;
            if nd <= int_len {
                out.push_str(digits);
                for _ in 0..(int_len - nd) {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len as usize]);
                out.push('.');
                out.push_str(&digits[int_len as usize..]);
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(digits);
        }
        out
    } else {
        // Scientific: mantissa with a decimal point, exponent with sign
        // and at least two digits ("1.0e+18", "1.2345e-05").
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if digits.len() == 1 {
            out.push_str(digits);
            out.push_str(".0");
        } else {
            out.push_str(&digits[..1]);
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if exp < 0 {
            out.push('-');
        } else {
            out.push('+');
        }
        let e = exp.abs();
        if e < 10 {
            out.push('0');
        }
        out.push_str(&e.to_string());
        out
    }
}

/// A row is an ordered sequence of values.
pub type Row = Vec<Value>;

/// SQLite `sqlite3AtoF` prefix semantics for CAST(... AS REAL):
/// optional whitespace and sign, digits with optional fraction, and an
/// optional exponent that only counts when at least one digit follows.
/// `inf`/`nan` are NOT accepted by CAST (they yield 0.0).
pub(crate) fn parse_real_prefix(s: &str) -> f64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut mantissa_digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        mantissa_digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            mantissa_digits += 1;
        }
    }
    if mantissa_digits == 0 {
        return 0.0;
    }
    let mut end = i;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let mut exp_digits = 0usize;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
            exp_digits += 1;
        }
        if exp_digits > 0 {
            end = j;
        }
    }
    s[start..end].parse::<f64>().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_ordering() {
        assert!(Value::Null < Value::Integer(0));
        assert!(Value::Integer(5) < Value::Integer(10));
        assert!(Value::Integer(5) < Value::Real(5.5));
        assert!(Value::Real(5.5) < Value::Text("a".to_string().into()));
        assert!(Value::Text("a".to_string().into()) < Value::Text("b".to_string().into()));
        assert!(Value::Text("z".to_string().into()) < Value::Blob(b"z".to_vec()));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let values = vec![
            Value::Null,
            Value::Integer(42),
            Value::Integer(-1_000_000),
            Value::Real(1.5),
            Value::Text("hello".to_string().into()),
            Value::Text("unicode: 你好".to_string().into()),
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
        assert_eq!(Affinity::Real.coerce(Value::Integer(7)), Value::Real(7.0));
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
        // SQLite numeric-coerces TEXT in boolean contexts with
        // sqlite3AtoF PREFIX semantics: 'abc' -> 0 (false), '1x' -> 1
        // (true). Pinned by tests/nested_join.rs::boolean_truthiness.
        assert!(!Value::Text("abc".into()).is_truthy());
        assert!(Value::Text("1x".into()).is_truthy());
        assert!(!Value::Text("inf".into()).is_truthy());
        assert!(Value::Text(".5".into()).is_truthy());
        // A non-empty blob is TRUE in a boolean context (probed against
        // SQLite: `SELECT 1 WHERE x'31'` passes).
        assert!(Value::Blob(vec![0x31]).is_truthy());
        assert!(!Value::Blob(vec![]).is_truthy());
    }
}
