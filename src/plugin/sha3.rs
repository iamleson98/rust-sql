//! The SHA3 extension (SQLite's `ext/misc/shathree.c`), reimplemented:
//! `sha3(X[,N])`, `sha3_agg(X[,N])`, and the `.sha3sum` hashing machinery.
//!
//! The hash primitive is FIPS-202 SHA3 — the Keccak-f[1600] step below is
//! a line-for-line transcription of shathree.c's unrolled
//! `KeccakF1600Step` (including its exact round-constant table), so the
//! digests are bit-identical to SQLite's. The SQL surface mirrors
//! shathree.c exactly:
//!
//! * `sha3(X)` / `sha3(X, N)` hashes the VALUE BYTES — text via the
//!   engine's SQLite-3.53 REAL→TEXT conversion for numbers, blob bytes
//!   for blobs; N ∈ {224, 256, 384, 512} (default 256, anything else is
//!   an error: `SHA3 size should be one of: 224 256 384 512`).
//! * `sha3_agg(X[,N])` hashes each row value with the same STREAM
//!   ENCODING `sha3_query` uses per column — `N` for NULL, `I` + 8
//!   big-endian bytes for INTEGER, `F` + 8 big-endian IEEE bytes for
//!   REAL, `T<len>:` + UTF-8 for TEXT, `B<len>:` + bytes for BLOB. N
//!   outside {224, 384, 512} silently falls back to 256 (shathree.c's
//!   `sha3AggStep`); zero rows → NULL.
//!
//! Everything is pinned byte-for-byte against the real SQLite 3.53.4
//! shell (golden hashes in the tests below and `tests/cli_sha3sum.rs`).

use crate::types::Value;
use crate::{Error, Result};

// ---------------------------------------------------------------------------
// Keccak-f[1600) / FIPS-202 SHA3 — verbatim port of shathree.c's
// KeccakF1600Step (unrolled, with its exact RC table)
// ---------------------------------------------------------------------------

/// Round constants (iota) — copied from shathree.c.
const RC: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

#[inline(always)]
fn rol64(a: u64, x: u32) -> u64 {
    a.rotate_left(x)
}

/// One Keccak-f[1600] permutation over the 25-lane state
/// (`s[5*x + y]` in shathree.c's aXY naming).
fn keccak_f1600_step(s: &mut [u64; 25]) {
    let (mut a00, mut a01, mut a02, mut a03, mut a04) = (s[0], s[1], s[2], s[3], s[4]);
    let (mut a10, mut a11, mut a12, mut a13, mut a14) = (s[5], s[6], s[7], s[8], s[9]);
    let (mut a20, mut a21, mut a22, mut a23, mut a24) = (s[10], s[11], s[12], s[13], s[14]);
    let (mut a30, mut a31, mut a32, mut a33, mut a34) = (s[15], s[16], s[17], s[18], s[19]);
    let (mut a40, mut a41, mut a42, mut a43, mut a44) = (s[20], s[21], s[22], s[23], s[24]);
    let (mut b0, mut b1, mut b2, mut b3, mut b4): (u64, u64, u64, u64, u64);
    let (mut c0, mut c1, mut c2, mut c3, mut c4): (u64, u64, u64, u64, u64);
    let (mut d0, mut d1, mut d2, mut d3, mut d4): (u64, u64, u64, u64, u64);

    let mut i = 0usize;
    while i < 24 {
        // ------------------------------ round i
        c0 = a00 ^ a10 ^ a20 ^ a30 ^ a40;
        c1 = a01 ^ a11 ^ a21 ^ a31 ^ a41;
        c2 = a02 ^ a12 ^ a22 ^ a32 ^ a42;
        c3 = a03 ^ a13 ^ a23 ^ a33 ^ a43;
        c4 = a04 ^ a14 ^ a24 ^ a34 ^ a44;
        d0 = c4 ^ rol64(c1, 1);
        d1 = c0 ^ rol64(c2, 1);
        d2 = c1 ^ rol64(c3, 1);
        d3 = c2 ^ rol64(c4, 1);
        d4 = c3 ^ rol64(c0, 1);
        b0 = a00 ^ d0;
        b1 = rol64(a11 ^ d1, 44);
        b2 = rol64(a22 ^ d2, 43);
        b3 = rol64(a33 ^ d3, 21);
        b4 = rol64(a44 ^ d4, 14);
        a00 = b0 ^ (!b1 & b2);
        a00 ^= RC[i];
        a11 = b1 ^ (!b2 & b3);
        a22 = b2 ^ (!b3 & b4);
        a33 = b3 ^ (!b4 & b0);
        a44 = b4 ^ (!b0 & b1);
        b2 = rol64(a20 ^ d0, 3);
        b3 = rol64(a31 ^ d1, 45);
        b4 = rol64(a42 ^ d2, 61);
        b0 = rol64(a03 ^ d3, 28);
        b1 = rol64(a14 ^ d4, 20);
        a20 = b0 ^ (!b1 & b2);
        a31 = b1 ^ (!b2 & b3);
        a42 = b2 ^ (!b3 & b4);
        a03 = b3 ^ (!b4 & b0);
        a14 = b4 ^ (!b0 & b1);
        b4 = rol64(a40 ^ d0, 18);
        b0 = rol64(a01 ^ d1, 1);
        b1 = rol64(a12 ^ d2, 6);
        b2 = rol64(a23 ^ d3, 25);
        b3 = rol64(a34 ^ d4, 8);
        a40 = b0 ^ (!b1 & b2);
        a01 = b1 ^ (!b2 & b3);
        a12 = b2 ^ (!b3 & b4);
        a23 = b3 ^ (!b4 & b0);
        a34 = b4 ^ (!b0 & b1);
        b1 = rol64(a10 ^ d0, 36);
        b2 = rol64(a21 ^ d1, 10);
        b3 = rol64(a32 ^ d2, 15);
        b4 = rol64(a43 ^ d3, 56);
        b0 = rol64(a04 ^ d4, 27);
        a10 = b0 ^ (!b1 & b2);
        a21 = b1 ^ (!b2 & b3);
        a32 = b2 ^ (!b3 & b4);
        a43 = b3 ^ (!b4 & b0);
        a04 = b4 ^ (!b0 & b1);
        b3 = rol64(a30 ^ d0, 41);
        b4 = rol64(a41 ^ d1, 2);
        b0 = rol64(a02 ^ d2, 62);
        b1 = rol64(a13 ^ d3, 55);
        b2 = rol64(a24 ^ d4, 39);
        a30 = b0 ^ (!b1 & b2);
        a41 = b1 ^ (!b2 & b3);
        a02 = b2 ^ (!b3 & b4);
        a13 = b3 ^ (!b4 & b0);
        a24 = b4 ^ (!b0 & b1);

        // ------------------------------ round i+1
        c0 = a00 ^ a20 ^ a40 ^ a10 ^ a30;
        c1 = a11 ^ a31 ^ a01 ^ a21 ^ a41;
        c2 = a22 ^ a42 ^ a12 ^ a32 ^ a02;
        c3 = a33 ^ a03 ^ a23 ^ a43 ^ a13;
        c4 = a44 ^ a14 ^ a34 ^ a04 ^ a24;
        d0 = c4 ^ rol64(c1, 1);
        d1 = c0 ^ rol64(c2, 1);
        d2 = c1 ^ rol64(c3, 1);
        d3 = c2 ^ rol64(c4, 1);
        d4 = c3 ^ rol64(c0, 1);
        b0 = a00 ^ d0;
        b1 = rol64(a31 ^ d1, 44);
        b2 = rol64(a12 ^ d2, 43);
        b3 = rol64(a43 ^ d3, 21);
        b4 = rol64(a24 ^ d4, 14);
        a00 = b0 ^ (!b1 & b2);
        a00 ^= RC[i + 1];
        a31 = b1 ^ (!b2 & b3);
        a12 = b2 ^ (!b3 & b4);
        a43 = b3 ^ (!b4 & b0);
        a24 = b4 ^ (!b0 & b1);
        b2 = rol64(a40 ^ d0, 3);
        b3 = rol64(a21 ^ d1, 45);
        b4 = rol64(a02 ^ d2, 61);
        b0 = rol64(a33 ^ d3, 28);
        b1 = rol64(a14 ^ d4, 20);
        a40 = b0 ^ (!b1 & b2);
        a21 = b1 ^ (!b2 & b3);
        a02 = b2 ^ (!b3 & b4);
        a33 = b3 ^ (!b4 & b0);
        a14 = b4 ^ (!b0 & b1);
        b4 = rol64(a30 ^ d0, 18);
        b0 = rol64(a11 ^ d1, 1);
        b1 = rol64(a42 ^ d2, 6);
        b2 = rol64(a23 ^ d3, 25);
        b3 = rol64(a04 ^ d4, 8);
        a30 = b0 ^ (!b1 & b2);
        a11 = b1 ^ (!b2 & b3);
        a42 = b2 ^ (!b3 & b4);
        a23 = b3 ^ (!b4 & b0);
        a04 = b4 ^ (!b0 & b1);
        b1 = rol64(a20 ^ d0, 36);
        b2 = rol64(a01 ^ d1, 10);
        b3 = rol64(a32 ^ d2, 15);
        b4 = rol64(a13 ^ d3, 56);
        b0 = rol64(a44 ^ d4, 27);
        a20 = b0 ^ (!b1 & b2);
        a01 = b1 ^ (!b2 & b3);
        a32 = b2 ^ (!b3 & b4);
        a13 = b3 ^ (!b4 & b0);
        a44 = b4 ^ (!b0 & b1);
        b3 = rol64(a10 ^ d0, 41);
        b4 = rol64(a41 ^ d1, 2);
        b0 = rol64(a22 ^ d2, 62);
        b1 = rol64(a03 ^ d3, 55);
        b2 = rol64(a34 ^ d4, 39);
        a10 = b0 ^ (!b1 & b2);
        a41 = b1 ^ (!b2 & b3);
        a22 = b2 ^ (!b3 & b4);
        a03 = b3 ^ (!b4 & b0);
        a34 = b4 ^ (!b0 & b1);

        // ------------------------------ round i+2
        c0 = a00 ^ a40 ^ a30 ^ a20 ^ a10;
        c1 = a21 ^ a11 ^ a01 ^ a31 ^ a41;
        c2 = a02 ^ a42 ^ a32 ^ a22 ^ a12;
        c3 = a13 ^ a03 ^ a43 ^ a33 ^ a23;
        c4 = a34 ^ a24 ^ a14 ^ a44 ^ a04;
        d0 = c4 ^ rol64(c1, 1);
        d1 = c0 ^ rol64(c2, 1);
        d2 = c1 ^ rol64(c3, 1);
        d3 = c2 ^ rol64(c4, 1);
        d4 = c3 ^ rol64(c0, 1);
        b0 = a00 ^ d0;
        b1 = rol64(a21 ^ d1, 44);
        b2 = rol64(a42 ^ d2, 43);
        b3 = rol64(a13 ^ d3, 21);
        b4 = rol64(a34 ^ d4, 14);
        a00 = b0 ^ (!b1 & b2);
        a00 ^= RC[i + 2];
        a21 = b1 ^ (!b2 & b3);
        a42 = b2 ^ (!b3 & b4);
        a13 = b3 ^ (!b4 & b0);
        a34 = b4 ^ (!b0 & b1);
        b2 = rol64(a30 ^ d0, 3);
        b3 = rol64(a01 ^ d1, 45);
        b4 = rol64(a22 ^ d2, 61);
        b0 = rol64(a43 ^ d3, 28);
        b1 = rol64(a14 ^ d4, 20);
        a30 = b0 ^ (!b1 & b2);
        a01 = b1 ^ (!b2 & b3);
        a22 = b2 ^ (!b3 & b4);
        a43 = b3 ^ (!b4 & b0);
        a14 = b4 ^ (!b0 & b1);
        b4 = rol64(a10 ^ d0, 18);
        b0 = rol64(a31 ^ d1, 1);
        b1 = rol64(a02 ^ d2, 6);
        b2 = rol64(a23 ^ d3, 25);
        b3 = rol64(a44 ^ d4, 8);
        a10 = b0 ^ (!b1 & b2);
        a31 = b1 ^ (!b2 & b3);
        a02 = b2 ^ (!b3 & b4);
        a23 = b3 ^ (!b4 & b0);
        a44 = b4 ^ (!b0 & b1);
        b1 = rol64(a40 ^ d0, 36);
        b2 = rol64(a11 ^ d1, 10);
        b3 = rol64(a32 ^ d2, 15);
        b4 = rol64(a03 ^ d3, 56);
        b0 = rol64(a24 ^ d4, 27);
        a40 = b0 ^ (!b1 & b2);
        a11 = b1 ^ (!b2 & b3);
        a32 = b2 ^ (!b3 & b4);
        a03 = b3 ^ (!b4 & b0);
        a24 = b4 ^ (!b0 & b1);
        b3 = rol64(a20 ^ d0, 41);
        b4 = rol64(a41 ^ d1, 2);
        b0 = rol64(a12 ^ d2, 62);
        b1 = rol64(a33 ^ d3, 55);
        b2 = rol64(a04 ^ d4, 39);
        a20 = b0 ^ (!b1 & b2);
        a41 = b1 ^ (!b2 & b3);
        a12 = b2 ^ (!b3 & b4);
        a33 = b3 ^ (!b4 & b0);
        a04 = b4 ^ (!b0 & b1);

        // ------------------------------ round i+3
        c0 = a00 ^ a30 ^ a10 ^ a40 ^ a20;
        c1 = a31 ^ a21 ^ a11 ^ a01 ^ a41;
        c2 = a12 ^ a02 ^ a42 ^ a32 ^ a22;
        c3 = a43 ^ a33 ^ a23 ^ a13 ^ a03;
        c4 = a24 ^ a14 ^ a04 ^ a44 ^ a34;
        d0 = c4 ^ rol64(c1, 1);
        d1 = c0 ^ rol64(c2, 1);
        d2 = c1 ^ rol64(c3, 1);
        d3 = c2 ^ rol64(c4, 1);
        d4 = c3 ^ rol64(c0, 1);
        b0 = a00 ^ d0;
        b1 = rol64(a01 ^ d1, 44);
        b2 = rol64(a02 ^ d2, 43);
        b3 = rol64(a03 ^ d3, 21);
        b4 = rol64(a04 ^ d4, 14);
        a00 = b0 ^ (!b1 & b2);
        a00 ^= RC[i + 3];
        a01 = b1 ^ (!b2 & b3);
        a02 = b2 ^ (!b3 & b4);
        a03 = b3 ^ (!b4 & b0);
        a04 = b4 ^ (!b0 & b1);
        b2 = rol64(a10 ^ d0, 3);
        b3 = rol64(a11 ^ d1, 45);
        b4 = rol64(a12 ^ d2, 61);
        b0 = rol64(a13 ^ d3, 28);
        b1 = rol64(a14 ^ d4, 20);
        a10 = b0 ^ (!b1 & b2);
        a11 = b1 ^ (!b2 & b3);
        a12 = b2 ^ (!b3 & b4);
        a13 = b3 ^ (!b4 & b0);
        a14 = b4 ^ (!b0 & b1);
        b4 = rol64(a20 ^ d0, 18);
        b0 = rol64(a21 ^ d1, 1);
        b1 = rol64(a22 ^ d2, 6);
        b2 = rol64(a23 ^ d3, 25);
        b3 = rol64(a24 ^ d4, 8);
        a20 = b0 ^ (!b1 & b2);
        a21 = b1 ^ (!b2 & b3);
        a22 = b2 ^ (!b3 & b4);
        a23 = b3 ^ (!b4 & b0);
        a24 = b4 ^ (!b0 & b1);
        b1 = rol64(a30 ^ d0, 36);
        b2 = rol64(a31 ^ d1, 10);
        b3 = rol64(a32 ^ d2, 15);
        b4 = rol64(a33 ^ d3, 56);
        b0 = rol64(a34 ^ d4, 27);
        a30 = b0 ^ (!b1 & b2);
        a31 = b1 ^ (!b2 & b3);
        a32 = b2 ^ (!b3 & b4);
        a33 = b3 ^ (!b4 & b0);
        a34 = b4 ^ (!b0 & b1);
        b3 = rol64(a40 ^ d0, 41);
        b4 = rol64(a41 ^ d1, 2);
        b0 = rol64(a42 ^ d2, 62);
        b1 = rol64(a43 ^ d3, 55);
        b2 = rol64(a44 ^ d4, 39);
        a40 = b0 ^ (!b1 & b2);
        a41 = b1 ^ (!b2 & b3);
        a42 = b2 ^ (!b3 & b4);
        a43 = b3 ^ (!b4 & b0);
        a44 = b4 ^ (!b0 & b1);

        i += 4;
    }

    *s = [
        a00, a01, a02, a03, a04, a10, a11, a12, a13, a14, a20, a21, a22, a23, a24, a30, a31, a32,
        a33, a34, a40, a41, a42, a43, a44,
    ];
}

/// A streaming SHA3 context (FIPS-202: Keccak with the 0x06/0x80 pad),
/// mirroring shathree.c's SHA3Context (little-endian byte lane packing).
pub struct Sha3 {
    s: [u64; 25],
    /// Rate in bytes = (1600 - 2*size_bits)/8, rounded up to a multiple
    /// of 8 (shathree's `(1600 - ((iSize + 31) & !31)*2)/8`).
    rate: usize,
    /// Bytes loaded into the current block.
    loaded: usize,
    /// Output length in bytes.
    outlen: usize,
}

impl Sha3 {
    /// New SHA3-224/256/384/512 context. Any other bit size is rejected
    /// with SQLite's exact message.
    pub fn new(bits: u32) -> Result<Sha3> {
        if !matches!(bits, 224 | 256 | 384 | 512) {
            return Err(Error::semantic(
                "SHA3 size should be one of: 224 256 384 512",
            ));
        }
        let rate = (1600usize - (((bits as usize) + 31) & !31) * 2) / 8;
        Ok(Sha3 {
            s: [0u64; 25],
            rate,
            loaded: 0,
            outlen: bits as usize / 8,
        })
    }

    fn block_byte(s: &mut [u64; 25], i: usize, byte: u8) {
        // Little-endian byte packing into lane i/8, byte i%8 (u.x[]).
        s[i / 8] ^= (byte as u64) << (8 * (i % 8));
    }

    /// Absorb bytes.
    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            Self::block_byte(&mut self.s, self.loaded, byte);
            self.loaded += 1;
            if self.loaded == self.rate {
                keccak_f1600_step(&mut self.s);
                self.loaded = 0;
            }
        }
    }

    /// Finish (absorb the domain pad) and return the digest.
    pub fn finalize(mut self) -> Vec<u8> {
        if self.loaded == self.rate - 1 {
            self.update(&[0x86]);
        } else {
            self.update(&[0x06]);
            self.loaded = self.rate - 1;
            self.update(&[0x80]);
        }
        let mut out = Vec::with_capacity(self.outlen);
        let mut i = 0usize;
        while i < self.outlen {
            out.push(((self.s[i / 8] >> (8 * (i % 8))) & 0xff) as u8);
            i += 1;
        }
        out
    }

    /// Clone the streaming state.
    pub fn clone_state(&self) -> Sha3 {
        Sha3 {
            s: self.s,
            rate: self.rate,
            loaded: self.loaded,
            outlen: self.outlen,
        }
    }
}

/// One-shot SHA3 over `data`.
pub fn sha3(bits: u32, data: &[u8]) -> Result<Vec<u8>> {
    let mut h = Sha3::new(bits)?;
    h.update(data);
    Ok(h.finalize())
}

// ---------------------------------------------------------------------------
// The shathree.c stream encoding (sha3_query rows / sha3_agg values)
// ---------------------------------------------------------------------------

/// Append one value in shathree.c's `sha3UpdateFromValue` encoding.
pub fn sha3_update_value(h: &mut Sha3, v: &Value) {
    match v {
        Value::Null => h.update(b"N"),
        Value::Integer(i) => {
            h.update(b"I");
            h.update(&i.to_be_bytes());
        }
        Value::Real(f) => {
            h.update(b"F");
            h.update(&f.to_bits().to_be_bytes());
        }
        Value::Text(s) => {
            let b = s.as_bytes();
            h.update(format!("T{}:", b.len()).as_bytes());
            h.update(b);
        }
        Value::Blob(b) => {
            h.update(format!("B{}:", b.len()).as_bytes());
            h.update(b);
        }
    }
}

/// Append a `S<len>:` statement-text segment (sha3_query's per-statement
/// prefix).
pub fn sha3_update_stmt(h: &mut Sha3, sql: &str) {
    h.update(format!("S{}:", sql.len()).as_bytes());
    h.update(sql.as_bytes());
}

/// `sha3(X[,N])`: hash the value's TEXT/BLOB bytes (the scalar form —
/// distinct from the stream encoding above).
fn sha3_scalar(args: &[Value]) -> Result<Value> {
    let bits: u32 = match args.len() {
        1 => 256,
        _ => match args[1].as_integer() {
            n if matches!(n, 224 | 256 | 384 | 512) => n as u32,
            _ => {
                return Err(Error::semantic(
                    "SHA3 size should be one of: 224 256 384 512",
                ))
            }
        },
    };
    if args[0].is_null() {
        return Ok(Value::Null);
    }
    let bytes: Vec<u8> = match &args[0] {
        Value::Blob(b) => b.to_vec(),
        other => other.as_text().into_bytes(),
    };
    Ok(Value::Blob(sha3(bits, &bytes)?))
}

/// The registered `sha3(X[,N])` scalar function.
pub struct Sha3Func;

impl crate::plugin::ScalarFunction for Sha3Func {
    fn name(&self) -> &str {
        "sha3"
    }
    fn arity(&self) -> crate::plugin::Arity {
        crate::plugin::Arity::Variadic
    }
    fn deterministic(&self) -> bool {
        true
    }
    fn call(&self, _ctx: &crate::plugin::FnCtx, args: &[Value]) -> Result<Value> {
        if args.is_empty() {
            return Err(Error::semantic("wrong number of arguments"));
        }
        sha3_scalar(args)
    }
}

/// Per-group state of `sha3_agg(X[,N])`.
struct Sha3AggState {
    hash: Option<Sha3>,
}

/// The registered `sha3_agg(X[,N])` aggregate.
pub struct Sha3Agg;

impl crate::plugin::AggregateFunction for Sha3Agg {
    fn name(&self) -> &str {
        "sha3_agg"
    }
    fn arity(&self) -> crate::plugin::Arity {
        crate::plugin::Arity::Variadic
    }
    fn init(&self) -> Box<dyn crate::plugin::AggState> {
        Box::new(Sha3AggState { hash: None })
    }
}

impl crate::plugin::AggState for Sha3AggState {
    fn step(&mut self, _ctx: &crate::plugin::AggCtx, args: &[Value]) -> Result<()> {
        if args.is_empty() {
            return Err(Error::semantic("wrong number of arguments"));
        }
        if self.hash.is_none() {
            // shathree.c sha3AggStep: default 256; 224/384/512 honored,
            // ANY OTHER VALUE (including a bad type) falls back to 256.
            let bits = if args.len() >= 2 {
                match args[1].as_integer() {
                    224 => 224,
                    384 => 384,
                    512 => 512,
                    _ => 256,
                }
            } else {
                256
            };
            self.hash = Some(Sha3::new(bits)?);
        }
        if let Some(h) = self.hash.as_mut() {
            sha3_update_value(h, &args[0]);
        }
        Ok(())
    }
    fn value(&self) -> Result<Value> {
        match &self.hash {
            // No rows in the group: shathree.c's xFinal leaves the result
            // unset → NULL.
            None => Ok(Value::Null),
            Some(h) => Ok(Value::Blob(h.clone_state().finalize())),
        }
    }
}

/// Register `sha3` + `sha3_agg` on a database handle (the sqlite3 shell
/// links shathree.c into every session; the rustqlite CLI does the same).
pub fn register(db: &mut crate::Database) -> Result<()> {
    db.create_function(Sha3Func)?;
    db.create_aggregate(Sha3Agg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::AggregateFunction;

    fn blob_bytes(v: &Value) -> &[u8] {
        match v {
            Value::Blob(b) => b,
            _ => panic!("expected blob"),
        }
    }

    /// Golden values from the REAL SQLite 3.53.4 shell (shathree.c).
    #[test]
    fn golden_sha3_vectors() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{:02x}", x)).collect::<String>();
        // sha3('abc') at all four sizes.
        assert_eq!(
            hex(&sha3(224, b"abc").unwrap()),
            "e642824c3f8cf24ad09234ee7d3c766fc9a3a5168d0c94ad73b46fdf"
        );
        assert_eq!(
            hex(&sha3(256, b"abc").unwrap()),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
        assert_eq!(
            hex(&sha3(384, b"abc").unwrap()),
            "ec01498288516fc926459f58e2c6ad8df9b473cb0fc08c2596da7cf0e49be4b298d88cea927ac7f539f1edf228376d25"
        );
        assert_eq!(
            hex(&sha3(512, b"abc").unwrap()),
            "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0"
        );
        // sha3('') and sha3(x'')
        assert_eq!(
            hex(&sha3(256, b"").unwrap()),
            "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a"
        );
        assert_eq!(
            hex(&sha3(256, &[0x00, 0xff]).unwrap()),
            "17709a2e0d4734ada82a5f7042e459c726ed979924216b5eedc769422d6558cf"
        );
        // Bad size → SQLite's exact error text.
        assert!(sha3(128, b"x").is_err());
    }

    /// sha3(X) scalar semantics on numbers: the TEXT conversion feeds the
    /// hash (pinned against the real shell).
    #[test]
    fn golden_sha3_scalar_values() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{:02x}", x)).collect::<String>();
        let cases: &[(Value, &str)] = &[
            // sha3(0) / sha3(1) / sha3(-1)
            (
                Value::Integer(0),
                "f9e2eaaa42d9fe9e558a9b8ef1bf366f190aacaa83bad2641ee106e9041096e4",
            ),
            (
                Value::Integer(1),
                "67b176705b46206614219f47a05aee7ae6a3edbe850bbbe214c536b989aea4d2",
            ),
            (
                Value::Integer(-1),
                "28f061b4d9c2b1e35e8fbc5e339ccf7f0c99319b278ba50c4195a0a55d53d1e4",
            ),
            // sha3(1.5) — "1.5"
            (
                Value::Real(1.5),
                "331a267242613d2e77c52cbcfa51cf06a40b3a8f37ed2916d99253acedfff747",
            ),
            // sha3(1e300) — "1.0e+300" (the 3.53 renderer!)
            (
                Value::Real(1e300),
                "be99f38abd9c9bff5978b8419c955ad9f223b19d55068a1d66fc09bfb49f2c32",
            ),
        ];
        for (v, want) in cases {
            let got = sha3_scalar(std::slice::from_ref(v)).unwrap();
            assert_eq!(hex(blob_bytes(&got)), *want, "sha3 of {:?}", v);
        }
        // NULL in → NULL out.
        assert!(sha3_scalar(&[Value::Null]).unwrap().is_null());
    }

    /// sha3_agg's stream encoding (pinned against the real shell over
    /// the rows (1, 'a', NULL, 2.5, x'00ff')).
    #[test]
    fn golden_sha3_agg_stream() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{:02x}", x)).collect::<String>();
        let agg = Sha3Agg;
        let mut state = agg.init();
        let ctx = crate::plugin::AggCtx::new(1);
        for v in [
            Value::Integer(1),
            Value::Text("a".into()),
            Value::Null,
            Value::Real(2.5),
            Value::Blob(vec![0x00, 0xff]),
        ] {
            state.step(&ctx, &[v]).unwrap();
        }
        let out = state.value().unwrap();
        assert_eq!(
            hex(blob_bytes(&out)),
            "c17ffdb5ab195de1922e8930360b6c1fde4d01a6edb1d3c636a1a90e5046d6ec"
        );
    }
}
