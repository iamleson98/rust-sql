//! The `zipfile()` virtual table module — SQLite's zipfile.c ported to
//! the engine's plugin API, plus the zip container codec it rides.
//!
//! Registered by the CLI (the sqlite3 shell links shell.c's extensions
//! into every session; rustqlite's CLI does the same — see
//! `register()`). Two access forms, exactly like SQLite:
//!
//! * eponymous: `SELECT * FROM zipfile('x.zip')` — a read-only walk of
//!   the archive's members (the FROM-clause function form, dispatched
//!   through the table-valued-function path);
//! * named + writable: `CREATE VIRTUAL TABLE temp.zz USING
//!   zipfile('x.zip')` followed by `REPLACE INTO zz(name, mode, mtime,
//!   data)` / `DELETE FROM zz` — the `.archive` command's machinery.
//!
//! Column set (SQLite's exact 7 + the hidden `z`):
//! `name TEXT, mode INT, mtime INT, sz INT, rawdata BLOB, data BLOB,
//! method INT` (+ `z` hidden: the archive filename).
//!
//! # Byte discipline (writer)
//!
//! Members are written STORED (method 0) — the exact behavior of a
//! sqlite3 shell built without zlib (`SQLITE_HAVE_ZLIB` undefined):
//! local header `PK\x03\x04` with version-needed 20, flags `0x0800`
//! (UTF-8 names), method 0, DOS time/date derived from the member
//! mtime (UTC), crc32 of the payload, the 9-byte `UT` extended-
//! timestamp extra (`'U','T', size 5, flags 0x01, mtime u32 LE`);
//! central directory `PK\x01\x02` with version-made-by `0x031e`
//! (UNIX/3.0), external attrs `(mode << 16)`, internal attrs 0, no
//! comments; EOCD `PK\x05\x06` with no trailing bytes. Every field
//! value pinned against the real 3.53.4 shell's stored archives
//! (fixture-differential, `tests/cli_archive.rs`).
//!
//! DEFLATED members (method 8) are READ byte-exactly through the
//! engine's own RFC-1951 inflater (decompression is deterministic —
//! any conforming inflater reproduces the member bytes); writing stays
//! stored, so archives we create are fully interoperable with every
//! zip reader including the zlib-linked sqlite3, but not byte-identical
//! to that shell's output for compressible members (documented in the
//! README ledger — the no-zlib build's discipline).

use crate::error::{Error, Result};
use crate::plugin::vtab::{
    IndexInfo, ModuleCaps, UpdateOp, VirtualTable, VirtualTableCursor, VirtualTableModule,
    VtabConstraint,
};
use crate::types::Value;
use crate::Result as CrateResult;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ============================================================
// CRC-32 (IEEE 802.3, reflected, init/final 0xFFFFFFFF) — the zip
// checksum. Table-driven; the table is the standard reflection of
// the CRC-32 polynomial 0xEDB88320.

fn crc32_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        t
    })
}

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let t = crc32_table();
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ============================================================
// RFC 1951 DEFLATE inflater (raw streams; the zlib wrapper is
// handled by the call site). Fixed and dynamic Huffman blocks,
// stored blocks. Any conforming inflater reproduces the exact
// decompressed bytes — read parity needs nothing more.

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    acc: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit: 0,
            acc: 0,
        }
    }

    fn need(&mut self, n: u32) -> Result<()> {
        while self.bit < n {
            let b = *self
                .data
                .get(self.pos)
                .ok_or_else(|| Error::corruption(String::from("zip: deflate stream truncated")))?;
            self.pos += 1;
            self.acc |= (b as u32) << self.bit;
            self.bit += 8;
        }
        Ok(())
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.acc & ((1u32 << n) - 1);
        self.acc >>= n;
        self.bit -= n;
        Ok(v)
    }

    fn align(&mut self) {
        let drop = self.bit % 8;
        self.acc >>= drop;
        self.bit -= drop;
    }
}

/// Canonical-Huffman decode table (RFC 1951's codes are assigned in
/// bit-reversed order of increasing length/symbol).
struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Self { counts, symbols }
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Error::corruption(String::from("zip: bad huffman code")))
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Inflate a raw DEFLATE stream.
pub(crate) fn inflate_raw(data: &[u8], size_hint: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(size_hint.min(1 << 22));
    let mut br = BitReader::new(data);
    loop {
        let last = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                // Stored: skip to the byte boundary, LEN/NLEN, bytes.
                br.align();
                let l0 = br.bits(8)? as usize;
                let l1 = br.bits(8)? as usize;
                let n0 = br.bits(8)? as usize;
                let n1 = br.bits(8)? as usize;
                let len = l0 | (l1 << 8);
                let nlen = n0 | (n1 << 8);
                if len != (!nlen & 0xFFFF) {
                    return Err(Error::corruption(String::from(
                        "zip: stored-block length mismatch",
                    )));
                }
                for _ in 0..len {
                    out.push(br.bits(8)? as u8);
                }
            }
            1 => {
                // Fixed Huffman.
                let mut lit = [0u8; 288];
                for (i, e) in lit.iter_mut().enumerate() {
                    *e = if i < 144 {
                        8
                    } else if i < 256 {
                        9
                    } else if i < 280 {
                        7
                    } else {
                        8
                    };
                }
                let dist = [5u8; 30];
                inflate_block(&mut br, &Huff::new(&lit), &Huff::new(&dist), &mut out)?;
            }
            2 => {
                // Dynamic Huffman.
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut cl = [0u8; 19];
                for &o in ORDER.iter().take(hclen) {
                    cl[o] = br.bits(3)? as u8;
                }
                let clh = Huff::new(&cl);
                let mut lengths = vec![0u8; hlit + hdist];
                let mut i = 0usize;
                while i < lengths.len() {
                    let sym = clh.decode(&mut br)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err(Error::corruption(String::from(
                                    "zip: repeat with no previous",
                                )));
                            }
                            let prev = lengths[i - 1];
                            let n = 3 + br.bits(2)? as usize;
                            for _ in 0..n {
                                if i >= lengths.len() {
                                    return Err(Error::corruption(String::from(
                                        "zip: overlong repeat",
                                    )));
                                }
                                lengths[i] = prev;
                                i += 1;
                            }
                        }
                        17 => {
                            let n = 3 + br.bits(3)? as usize;
                            i += n;
                        }
                        18 => {
                            let n = 11 + br.bits(7)? as usize;
                            i += n;
                        }
                        _ => {
                            return Err(Error::corruption(String::from(
                                "zip: bad code-length symbol",
                            )))
                        }
                    }
                }
                if i > lengths.len() {
                    return Err(Error::corruption(String::from(
                        "zip: code lengths overflow",
                    )));
                }
                let lith = Huff::new(&lengths[..hlit]);
                let disth = Huff::new(&lengths[hlit..]);
                inflate_block(&mut br, &lith, &disth, &mut out)?;
            }
            _ => return Err(Error::corruption(String::from("zip: reserved block type"))),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

fn inflate_block(br: &mut BitReader<'_>, lit: &Huff, dist: &Huff, out: &mut Vec<u8>) -> Result<()> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let li = (sym - 257) as usize;
                let len = LEN_BASE[li] as usize + br.bits(LEN_EXTRA[li] as u32)? as usize;
                let ds = dist.decode(br)? as usize;
                if ds >= 30 {
                    return Err(Error::corruption(String::from("zip: bad distance symbol")));
                }
                let d = DIST_BASE[ds] as usize + br.bits(DIST_EXTRA[ds] as u32)? as usize;
                if d > out.len() {
                    return Err(Error::corruption(String::from(
                        "zip: distance before start",
                    )));
                }
                let start = out.len() - d;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err(Error::corruption(String::from("zip: bad literal symbol"))),
        }
    }
}

/// Inflate a zlib-wrapped stream (sqlar's `compress()` output): the
/// 2-byte header is validated, the 4-byte adler32 trailer ignored
/// (sqlite's own sqlar_uncompress never verifies it either).
pub(crate) fn inflate_zlib(data: &[u8], size_hint: usize) -> Result<Vec<u8>> {
    if data.len() < 6 {
        return Err(Error::corruption(String::from(
            "sqlar: zlib stream too short",
        )));
    }
    let cmf = data[0];
    let flg = data[1];
    if cmf & 0x0F != 8 || ((cmf as u16) << 8 | flg as u16) % 31 != 0 {
        return Err(Error::corruption(String::from("sqlar: bad zlib header")));
    }
    if flg & 0x20 != 0 {
        return Err(Error::corruption(String::from(
            "sqlar: unexpected zlib dictionary",
        )));
    }
    inflate_raw(&data[2..], size_hint)
}

// ============================================================
// Zip container codec.

/// One archive member.
#[derive(Clone, Debug)]
pub(crate) struct ZipEntry {
    pub name: String,
    pub mode: i64,
    /// Unix mtime (seconds); from the `UT` extra when present, else
    /// the DOS stamp read as UTC.
    pub mtime: i64,
    pub method: i64,
    /// The member's payload AS STORED IN THE FILE (compressed bytes
    /// for method 8, the raw bytes for method 0).
    pub raw: Vec<u8>,
    /// The member's uncompressed size.
    pub sz: usize,
}

impl ZipEntry {
    /// The member's data (inflated for method 8 — deterministic).
    pub fn data(&self) -> Result<Vec<u8>> {
        match self.method {
            0 => Ok(self.raw.clone()),
            8 => inflate_raw(&self.raw, self.sz),
            m => Err(Error::corruption(format!("zip: unsupported method {m}"))),
        }
    }

    /// A new stored member (the writer's only form).
    fn stored(name: String, mode: i64, mtime: i64, data: Vec<u8>) -> Self {
        Self {
            name,
            mode,
            mtime,
            method: 0,
            sz: data.len(),
            raw: data,
        }
    }
}

/// DOS time/date from a unix mtime (UTC — zipfile.c's conversion).
fn dos_time_date(mtime: i64) -> (u16, u16) {
    // Days-from-epoch → civil (Howard Hinnant's algorithm).
    let days = mtime.div_euclid(86400).max(0);
    let secs = mtime.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let year = y.max(1980) as u16;
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | ((s / 2) as u16);
    let date = ((year - 1980) << 9) | ((m as u16) << 5) | d as u16;
    (time, date)
}

/// Unix mtime from a DOS stamp (read as UTC).
fn dos_to_unix(time: u16, date: u16) -> i64 {
    let year = 1980 + (date >> 9) as i64;
    let mon = ((date >> 5) & 0x0F) as i64;
    let day = (date & 0x1F) as i64;
    let h = (time >> 11) as i64;
    let mi = ((time >> 5) & 0x3F) as i64;
    let s = ((time & 0x1F) as i64) * 2;
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) {
        return 0;
    }
    // Civil → days-from-epoch (inverse of dos_time_date's algorithm).
    let y = if mon <= 2 { year - 1 } else { year };
    let m = if mon <= 2 { mon + 9 } else { mon - 3 };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86400 + h * 3600 + mi * 60 + s
}

const CANNOT_FIND_EOCD: &str = "cannot find end of central directory record";

fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Parse a zip archive. Members keep the file's order (the central
/// directory's order — SQLite's).
pub(crate) fn parse_zip(data: &[u8]) -> Result<Vec<ZipEntry>> {
    const EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    if data.len() < 22 {
        return Err(Error::corruption(CANNOT_FIND_EOCD.to_string()));
    }
    let scan_start = data.len().saturating_sub(65_557);
    let mut eocd = None;
    let mut i = data.len() - 22;
    loop {
        if data[i..i + 4] == EOCD_SIG {
            // A candidate: comment length must reach exactly the end.
            let clen = rd_u16(data, i + 20) as usize;
            if i + 22 + clen == data.len() {
                eocd = Some(i);
                break;
            }
        }
        if i == scan_start {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.ok_or_else(|| {
        Error::corruption(String::from("cannot find end of central directory record"))
    })?;
    let n = rd_u16(data, eocd + 10) as usize;
    let cd_size = rd_u32(data, eocd + 12) as usize;
    let cd_off = rd_u32(data, eocd + 16) as usize;
    let mut entries = Vec::with_capacity(n);
    let mut off = cd_off;
    for _ in 0..n {
        if off + 46 > data.len() || data[off..off + 4] != [0x50, 0x4b, 0x01, 0x02] {
            return Err(Error::corruption(String::from(
                "zip: bad central directory entry",
            )));
        }
        let flags = rd_u16(data, off + 8);
        let method = rd_u16(data, off + 10) as i64;
        let dtime = rd_u16(data, off + 12);
        let ddate = rd_u16(data, off + 14);
        let _crc = rd_u32(data, off + 16);
        let csize = rd_u32(data, off + 20) as usize;
        let usize_ = rd_u32(data, off + 24) as usize;
        let nlen = rd_u16(data, off + 28) as usize;
        let elen = rd_u16(data, off + 30) as usize;
        let clen = rd_u16(data, off + 32) as usize;
        let eattr = rd_u32(data, off + 38);
        let lho = rd_u32(data, off + 42) as usize;
        if off + 46 + nlen + elen + clen > data.len() {
            return Err(Error::corruption(String::from(
                "zip: truncated central directory",
            )));
        }
        let name_bytes = &data[off + 46..off + 46 + nlen];
        let extra = &data[off + 46 + nlen..off + 46 + nlen + elen];
        let name = decode_name(name_bytes, flags);
        // UT extended timestamp (0x5455): flags byte, then mtime.
        let mut mtime = dos_to_unix(dtime, ddate);
        let mut eo = 0usize;
        while eo + 4 <= extra.len() {
            let id = rd_u16(extra, eo);
            let sz = rd_u16(extra, eo + 2) as usize;
            if eo + 4 + sz > extra.len() {
                break;
            }
            if id == 0x5455 && sz >= 5 && extra[eo + 4] & 0x01 != 0 && sz >= 5 {
                let t = rd_u32(extra, eo + 5) as i64;
                // sqlite's zipfile reads UT as SIGNED 32-bit.
                mtime = t as i32 as i64;
                break;
            }
            eo += 4 + sz;
        }
        // The member's payload: from the local header (its own name
        // length may differ from the CD's when bit 11 renamed).
        if lho + 30 > data.len() || data[lho..lho + 4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err(Error::corruption(String::from("zip: bad local header")));
        }
        let lnlen = rd_u16(data, lho + 26) as usize;
        let lelen = rd_u16(data, lho + 28) as usize;
        let dstart = lho + 30 + lnlen + lelen;
        if dstart + csize > data.len() {
            return Err(Error::corruption(String::from(
                "zip: truncated member data",
            )));
        }
        entries.push(ZipEntry {
            name,
            mode: (eattr >> 16) as i64,
            mtime,
            method,
            raw: data[dstart..dstart + csize].to_vec(),
            sz: usize_,
        });
        off += 46 + nlen + elen + clen;
    }
    let _ = cd_size;
    Ok(entries)
}

fn decode_name(b: &[u8], flags: u16) -> String {
    if flags & 0x0800 != 0 {
        String::from_utf8_lossy(b).into_owned()
    } else {
        // Legacy code-page names: sqlite's zipfile passes them through
        // as-is; we decode as UTF-8 with lossy fallback.
        String::from_utf8_lossy(b).into_owned()
    }
}

/// Serialize a zip archive (stored members) in the oracle-pinned
/// discipline — see the module docs.
pub(crate) fn write_zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    let mut offsets = Vec::with_capacity(entries.len());
    for e in entries {
        offsets.push(out.len() as u32);
        let (t, d) = dos_time_date(e.mtime);
        let name = e.name.as_bytes();
        // Local file header.
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 flag
        out.extend_from_slice(&(e.method as u16).to_le_bytes());
        out.extend_from_slice(&t.to_le_bytes());
        out.extend_from_slice(&d.to_le_bytes());
        out.extend_from_slice(&crc32(&e.raw).to_le_bytes());
        out.extend_from_slice(&(e.raw.len() as u32).to_le_bytes()); // csize
        out.extend_from_slice(&(e.sz as u32).to_le_bytes()); // usize
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&9u16.to_le_bytes()); // extra len
        out.extend_from_slice(name);
        // UT extended timestamp.
        out.extend_from_slice(b"UT");
        out.extend_from_slice(&5u16.to_le_bytes());
        out.push(0x01);
        out.extend_from_slice(&((e.mtime as u32).to_le_bytes()));
        out.extend_from_slice(&e.raw);
    }
    let cd_start = out.len() as u32;
    for (e, &lho) in entries.iter().zip(offsets.iter()) {
        let (t, d) = dos_time_date(e.mtime);
        let name = e.name.as_bytes();
        out.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
        out.extend_from_slice(&0x031eu16.to_le_bytes()); // made by: UNIX/3.0
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // flags
        out.extend_from_slice(&(e.method as u16).to_le_bytes());
        out.extend_from_slice(&t.to_le_bytes());
        out.extend_from_slice(&d.to_le_bytes());
        out.extend_from_slice(&crc32(&e.raw).to_le_bytes());
        out.extend_from_slice(&(e.raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(e.sz as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&9u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk start
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&((e.mode as u32) << 16).to_le_bytes());
        out.extend_from_slice(&lho.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(b"UT");
        out.extend_from_slice(&5u16.to_le_bytes());
        out.push(0x01);
        out.extend_from_slice(&((e.mtime as u32).to_le_bytes()));
    }
    let cd_size = out.len() as u32 - cd_start;
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment
    out
}

// ============================================================
// The zipfile() virtual table module.

/// Shared archive state: one per `CREATE VIRTUAL TABLE` instance. The
/// entry list is parsed lazily (the archive may not exist yet when the
/// vtab is created) and snapshotted into cursors at open time (a scan
/// sees a stable member list — the engine buffers INSERT..SELECT
/// source rows before `update` runs, the same contract SQLite's vtab
/// transactions give).
#[derive(Clone)]
pub struct ZipfileTable {
    path: String,
    state: Arc<Mutex<ZipfileState>>,
}

struct ZipfileState {
    entries: Vec<ZipEntry>,
    loaded: bool,
    /// rowid → entry index (inserts append and keep old rowids stable
    /// within one update batch; a DELETE rebuilds positionally —
    /// SQLite's zipfile re-reads the rewritten file after every
    /// transaction anyway).
    rowid_map: HashMap<i64, usize>,
    next_rowid: i64,
}

impl ZipfileTable {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            state: Arc::new(Mutex::new(ZipfileState {
                entries: Vec::new(),
                loaded: false,
                rowid_map: HashMap::new(),
                next_rowid: 1,
            })),
        }
    }

    /// Load (once) from the archive file.
    fn load(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.loaded {
            return Ok(());
        }
        st.loaded = true;
        st.entries.clear();
        st.rowid_map.clear();
        match std::fs::read(&self.path) {
            Ok(data) => {
                if !data.is_empty() {
                    st.entries = parse_zip(&data)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::corruption(format!(
                    "zipfile: cannot open {}: {}",
                    self.path, e
                )))
            }
        }
        let n = st.entries.len();
        for i in 0..n {
            st.rowid_map.insert(i as i64 + 1, i);
        }
        st.next_rowid = st.entries.len() as i64 + 1;
        Ok(())
    }
}

impl VirtualTable for ZipfileTable {
    fn columns(&self) -> Vec<(String, String)> {
        vec![
            ("name".into(), "TEXT".into()),
            ("mode".into(), "INT".into()),
            ("mtime".into(), "INT".into()),
            ("sz".into(), "INT".into()),
            ("rawdata".into(), "BLOB".into()),
            ("data".into(), "BLOB".into()),
            ("method".into(), "INT".into()),
        ]
    }

    fn aux_columns(&self) -> Vec<(String, String)> {
        vec![("z".into(), "TEXT".into())]
    }

    fn best_index(&self, constraints: &[VtabConstraint]) -> Result<IndexInfo> {
        // Full scans only (SQLite's own zipfile seeks on name
        // equality, but the CLI's shapes never need it).
        let mut info = IndexInfo::full_scan(constraints.len());
        info.estimated_rows = 1024;
        info.estimated_cost = 1e7;
        Ok(info)
    }

    fn open(&self) -> Result<Box<dyn VirtualTableCursor>> {
        self.load()?;
        let (entries, base) = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (st.entries.clone(), 1i64)
        };
        Ok(Box::new(ZipfileCursor {
            entries,
            pos: 0,
            base,
            data_cache: std::cell::RefCell::new((usize::MAX, Vec::new())),
        }))
    }

    fn update(&mut self, ops: Vec<UpdateOp>) -> Result<Vec<Option<i64>>> {
        self.load()?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out_rowids = Vec::with_capacity(ops.len());
        for op in ops {
            match (&op.old_rowid, op.columns.as_slice()) {
                (None, cols) => {
                    // INSERT (REPLACE resolves same-name first).
                    let get = |i: usize| -> Option<Value> { cols.get(i).cloned().flatten() };
                    let name = match get(0) {
                        Some(Value::Text(t)) => t.to_string(),
                        Some(v) => v.to_string(),
                        None => {
                            return Err(Error::semantic(String::from(
                                "zipfile: missing value for name",
                            )))
                        }
                    };
                    let mode = match get(1) {
                        Some(Value::Integer(i)) => i,
                        Some(Value::Null) | None => 0o100644,
                        Some(v) => v.as_integer(),
                    };
                    let mtime = match get(2) {
                        Some(Value::Integer(i)) => i,
                        Some(Value::Null) | None => 0,
                        Some(v) => v.as_integer(),
                    };
                    // data (col 5) is authoritative; rawdata (col 4)
                    // is stored as-is when data is absent.
                    let payload = match get(5).or_else(|| get(4)) {
                        Some(v) => value_bytes(&v),
                        None => Vec::new(),
                    };
                    // REPLACE semantics: drop a same-name member.
                    st.entries.retain(|e| e.name != name);
                    let entry = ZipEntry::stored(name, mode, mtime, payload);
                    let rowid = st.next_rowid;
                    st.next_rowid += 1;
                    let idx = st.entries.len();
                    st.entries.push(entry);
                    st.rowid_map.insert(rowid, idx);
                    out_rowids.push(Some(rowid));
                }
                (Some(rid), []) => {
                    // DELETE by rowid (the UPDATE arm below rejects
                    // non-empty columns).
                    let idx = st.rowid_map.get(rid).copied().ok_or_else(|| {
                        Error::corruption(format!("zipfile: no such rowid {rid}"))
                    })?;
                    if idx >= st.entries.len() {
                        return Err(Error::corruption(format!("zipfile: stale rowid {rid}")));
                    }
                    st.entries.remove(idx);
                    // Positional renumber (see the struct docs).
                    st.rowid_map.clear();
                    let n = st.entries.len();
                    for i in 0..n {
                        st.rowid_map.insert(i as i64 + 1, i);
                    }
                    st.next_rowid = st.entries.len() as i64 + 1;
                    out_rowids.push(None);
                }
                _ => {
                    return Err(Error::semantic(String::from(
                        "zipfile: UPDATE of members is not supported",
                    )))
                }
            }
        }
        // Persist: rewrite the archive (all members stored).
        let bytes = write_zip(&st.entries);
        drop(st);
        std::fs::write(&self.path, &bytes).map_err(|e| {
            Error::corruption(format!("zipfile: cannot write {}: {}", self.path, e))
        })?;
        Ok(out_rowids)
    }
}

pub struct ZipfileCursor {
    entries: Vec<ZipEntry>,
    pos: usize,
    base: i64,
    /// Inflated `data` of the entry at `.0` (method-8 members inflate
    /// once per position; RefCell because `column(&self)` needs
    /// interior mutability — safe under the cursor's single-owner use).
    data_cache: std::cell::RefCell<(usize, Vec<u8>)>,
}

impl VirtualTableCursor for ZipfileCursor {
    fn filter(&mut self, _idx_num: usize, _idx_str: Option<&str>, _args: &[Value]) -> Result<()> {
        self.pos = 0;
        self.data_cache.borrow_mut().0 = usize::MAX;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.pos >= self.entries.len()
    }

    fn column(&self, i: usize) -> Result<Value> {
        let e = self
            .entries
            .get(self.pos)
            .ok_or_else(|| Error::corruption(String::from("zipfile: cursor past end")))?;
        // Column 5 (`data`) inflates once per position.
        if i == 5 {
            {
                let mut cache = self.data_cache.borrow_mut();
                if cache.0 != self.pos {
                    cache.1 = e.data()?;
                    cache.0 = self.pos;
                }
            }
            let cache = self.data_cache.borrow();
            return Ok(Value::Blob(cache.1.clone().into_boxed_slice().into()));
        }
        Ok(match i {
            0 => Value::Text(e.name.clone().into()),
            1 => Value::Integer(e.mode),
            2 => Value::Integer(e.mtime),
            3 => Value::Integer(e.sz as i64),
            4 => Value::Blob(e.raw.clone().into_boxed_slice().into()),
            6 => Value::Integer(e.method),
            _ => Value::Null,
        })
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.base + self.pos as i64)
    }
}

/// The module singleton.
pub struct ZipfileModule;

impl VirtualTableModule for ZipfileModule {
    fn name(&self) -> &str {
        "zipfile"
    }
    fn caps(&self) -> u32 {
        ModuleCaps::WRITABLE
    }
    fn create(&self, _table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        let path = args
            .first()
            .ok_or_else(|| Error::semantic(String::from("zipfile: missing filename")))?;
        Ok(Box::new(ZipfileTable::new(path)))
    }
}

// ============================================================
// Registration + eponymous read.

/// `SELECT ... FROM zipfile('x.zip')` — the eponymous read. Columns
/// are the 7 vtab columns (z rides as the 8th, the filename).
pub(crate) fn zipfile_read(args: &[Value]) -> Result<(Vec<&'static str>, Vec<Vec<Value>>)> {
    let path = match args.first() {
        Some(Value::Text(t)) => t.as_str(),
        Some(Value::Null) | None => {
            return Err(Error::semantic(String::from("zipfile: missing filename")))
        }
        Some(_) => {
            return Err(Error::semantic(String::from(
                "zipfile: filename must be text",
            )))
        }
    };
    let entries = match std::fs::read(path) {
        Ok(d) if d.is_empty() => Vec::new(),
        Ok(d) => parse_zip(&d)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::corruption(format!("cannot open file: {path}")))
        }
        Err(e) => return Err(Error::corruption(format!("cannot open file: {path}: {e}"))),
    };
    let cols: Vec<&'static str> = vec!["name", "mode", "mtime", "sz", "rawdata", "data", "method"];
    let mut rows = Vec::with_capacity(entries.len());
    for e in &entries {
        let data = e.data()?;
        rows.push(vec![
            Value::Text(e.name.clone().into()),
            Value::Integer(e.mode),
            Value::Integer(e.mtime),
            Value::Integer(e.sz as i64),
            Value::Blob(e.raw.clone()),
            Value::Blob(data.into_boxed_slice().into()),
            Value::Integer(e.method),
        ]);
    }
    Ok((cols, rows))
}

fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.clone(),
        Value::Text(t) => t.as_bytes().to_vec(),
        Value::Null => Vec::new(),
        other => other.to_string().into_bytes(),
    }
}

/// Register the module on a database handle.
pub fn register(db: &mut crate::Database) -> CrateResult<()> {
    db.create_module(ZipfileModule)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_goldens() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b"hello world\n"), 0xAF08_3B2D);
        assert_eq!(crc32(b"second file\n"), 0xE472_FF82);
        assert_eq!(crc32(b"nested\n"), 0xEBAB_958D);
    }

    #[test]
    fn dos_round_trip() {
        for &t in &[
            951_825_600,   // 2000-02-29 (leap)
            315_619_200,   // 1980-01-07 (the DOS epoch)
            1_790_830_394, // 2026-10-01 04:53:14
            2_147_483_647,
            4_102_444_800, // 2100-01-01
        ] {
            let (time, date) = dos_time_date(t);
            let back = dos_to_unix(time, date);
            // DOS stamps lose the second's parity and pre-1980 dates.
            let (lo, hi) = (t & !1, t | 1);
            assert!(
                back == lo || back == hi || back == 0 && t < 315_532_800,
                "mtime {t} -> dos ({time:#x},{date:#x}) -> {back}"
            );
        }
        let (time, date) = dos_time_date(1_790_830_394);
        assert_eq!((time, date), (0x26A7, 0x5D41));
    }

    #[test]
    fn roundtrip_stored_zip() {
        let entries = vec![
            ZipEntry::stored(
                "a.txt".into(),
                0o100644,
                1_790_830_394,
                b"hello world\n".to_vec(),
            ),
            ZipEntry::stored(
                "sub/c.txt".into(),
                0o100664,
                1_790_830_394,
                b"nested\n".to_vec(),
            ),
        ];
        let bytes = write_zip(&entries);
        let back = parse_zip(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "a.txt");
        assert_eq!(back[0].mode, 0o100644);
        assert_eq!(back[0].mtime, 1_790_830_394);
        assert_eq!(back[0].method, 0);
        assert_eq!(back[0].data().unwrap(), b"hello world\n");
        assert_eq!(back[1].name, "sub/c.txt");
        assert_eq!(back[1].data().unwrap(), b"nested\n");
    }

    /// The oracle's exact stored zip (3 members) parses with every
    /// field intact — the golden fixture from tests/fixtures.
    #[test]
    fn parse_oracle_stored_zip() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/arch-three-members.zip"
        );
        let data = std::fs::read(path).expect("fixture present");
        let e = parse_zip(&data).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].name, "a.txt");
        assert_eq!(e[0].mode, 33204);
        assert_eq!(e[0].mtime, 1_790_830_394);
        assert_eq!(e[0].data().unwrap(), b"hello world\n");
        assert_eq!(e[1].name, "b.txt");
        assert_eq!(e[1].data().unwrap(), b"second file\n");
        assert_eq!(e[2].name, "sub/c.txt");
        assert_eq!(e[2].data().unwrap(), b"nested\n");
        // Byte discipline: rebuilding the same members reproduces the
        // oracle's file exactly (stored entries).
        let rebuilt = write_zip(&e);
        assert_eq!(rebuilt, data, "stored-entry rebuild must be byte-identical");
    }

    /// A Python-made deflated zip (method 8) inflates byte-exactly.
    #[test]
    fn inflate_deflated_member() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/arch-deflated.zip"
        );
        let data = std::fs::read(path).expect("fixture present");
        let e = parse_zip(&data).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].name, "big.txt");
        assert_eq!(e[0].method, 8);
        assert_eq!(e[0].sz, 1200);
        let want = b"hello world ".repeat(100);
        assert_eq!(e[0].data().unwrap(), want);
        assert_eq!(e[1].name, "tiny.txt");
        assert_eq!(e[1].data().unwrap(), b"x");
    }

    #[test]
    fn inflate_zlib_streams() {
        // sqlar's compress() output: 2-byte header + deflate + adler.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/arch-big.sqlar");
        // The fixture is a SQLite file; find the 789C blob inside it.
        let data = std::fs::read(path).expect("fixture present");
        let mut i = 0;
        while i + 2 < data.len() {
            if data[i] == 0x78 && data[i + 1] == 0x9C {
                // Try inflating from here; success on a plausible size.
                if let Ok(out) = inflate_zlib(&data[i..], 1200) {
                    if out.len() == 1200 && out.starts_with(b"hello world ") {
                        break;
                    }
                }
            }
            i += 1;
        }
        assert!(i + 2 < data.len(), "zlib stream not found in fixture");
    }
}
