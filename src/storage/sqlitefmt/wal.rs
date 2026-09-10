//! SQLite write-ahead-log (WAL) sidecar WRITER — fileformat2 §4.
//!
//! The engine's SQLite-format mode historically committed every
//! statement with a full atomic rewrite (temp + rename). This module
//! adds the REAL WAL protocol: after the first full write establishes
//! the main file, later commits append frames to a `<db>-wal` sidecar
//! holding only the pages that changed — a byte-exact WAL that BOTH
//! the engine's own reader (`reader::apply_wal`) and real SQLite (wal
//! recovery: checksum chain, salt match, commit frame) accept.
//!
//! Format (verified against the vendored sqlite3.c):
//!
//! * WAL header (32 bytes, all fields big-endian except the checksum
//!   interpretation): magic `0x377f0682` (LSB 0 = checksums treat data
//!   as 32-bit little-endian words), format version 3007000, page size,
//!   checkpoint sequence, salt-1, salt-2, and the running checksum of
//!   the header's first 24 bytes.
//! * Frame (24 + page-size bytes): page number, database size in pages
//!   after commit (non-zero ONLY on the final/commit frame), salt-1,
//!   salt-2 (raw copies of the header's), then the chained checksum of
//!   the frame header's first 8 bytes and the page content.
//! * The checksum chain starts at the WAL header checksum and threads
//!   through every frame; a torn/invalid frame stops replay exactly
//!   like a crash.
//! * A frame is valid only if its salts match the header's; recovery
//!   replays frames up to the LAST commit frame.
//!
//! Checksum algorithm (`walChecksumBytes`, little-endian-words mode):
//! fold 8-byte chunks as two u32 words `(w0, w1)` with
//! `s1 += w0 + s2; s2 += w1 + s1` (all modulo 2^32).

use std::path::Path;

/// WAL magic with LSB clear: checksums interpret data as 32-bit
/// little-endian words (native on every realistic target).
const WAL_MAGIC: u32 = 0x377f0682;
/// File format version (sqlite3.c WAL_MAX_VERSION).
const WAL_VERSION: u32 = 3_007_000;

/// Fold `bytes` (length must be a multiple of 8) into the running
/// little-endian-words checksum `(s1, s2)`.
fn wal_cksum(bytes: &[u8], s: (u32, u32)) -> (u32, u32) {
    debug_assert_eq!(bytes.len() % 8, 0);
    let (mut s1, mut s2) = s;
    for chunk in bytes.chunks_exact(8) {
        let w0 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let w1 = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        s1 = s1.wrapping_add(w0).wrapping_add(s2);
        s2 = s2.wrapping_add(w1).wrapping_add(s1);
    }
    (s1, s2)
}

/// Per-sidecar WAL writer state (one per open SQLite-format database;
/// reset — new salts — after every checkpoint/full write).
#[derive(Clone, Debug)]
pub struct WalWriter {
    page_size: u32,
    /// Checkpoint sequence number (WAL header field 12).
    ckpt_seq: u32,
    salt1: u32,
    salt2: u32,
    /// Running frame checksum chain (header checksum after a reset, the
    /// last frame's checksum after appends).
    cksum: (u32, u32),
    /// The WAL header's own checksum (bytes 0..24) — fixed at reset.
    hdr_cksum: (u32, u32),
    /// Bytes of WAL written under the current salts (32 when only the
    /// header exists).
    wal_len: u32,
    /// Frames committed under the current salts (checkpoint pressure).
    n_frames: u32,
}

impl WalWriter {
    /// Fresh writer: random-ish salts (uniqueness matters, not
    /// unpredictability), header checksum chained, no frames yet.
    pub fn new(page_size: u32, ckpt_seq: u32) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let salt1 = nanos
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(std::process::id());
        let salt2 = salt1.wrapping_mul(0x85EB_CA6B).wrapping_add(0xC2B2_AE35);
        let mut w = WalWriter {
            page_size,
            ckpt_seq,
            salt1,
            salt2,
            cksum: (0, 0),
            hdr_cksum: (0, 0),
            wal_len: 0,
            n_frames: 0,
        };
        w.hdr_cksum = wal_cksum(&w.header()[0..24], (0, 0));
        w.cksum = w.hdr_cksum;
        w.wal_len = 32;
        w
    }

    /// The 32-byte WAL header for the current salts.
    fn header(&self) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0..4].copy_from_slice(&WAL_MAGIC.to_be_bytes());
        h[4..8].copy_from_slice(&WAL_VERSION.to_be_bytes());
        h[8..12].copy_from_slice(&self.page_size.to_be_bytes());
        h[12..16].copy_from_slice(&self.ckpt_seq.to_be_bytes());
        h[16..20].copy_from_slice(&self.salt1.to_be_bytes());
        h[20..24].copy_from_slice(&self.salt2.to_be_bytes());
        // Checksum of the first 24 bytes (fixed at reset; see hdr_cksum).
        let (c1, c2) = self.hdr_cksum;
        h[24..28].copy_from_slice(&c1.to_be_bytes());
        h[28..32].copy_from_slice(&c2.to_be_bytes());
        h
    }

    /// Encode one commit as WAL bytes: every changed page as a frame
    /// (page order), the LAST frame carrying the new database size —
    /// the commit marker. `changed` must be non-empty (callers skip
    /// no-op commits). Returns the bytes to append to the sidecar.
    pub fn encode_commit(&mut self, changed: &[(u32, Vec<u8>)], db_size: u32) -> Vec<u8> {
        assert!(!changed.is_empty(), "a WAL commit needs at least one frame");
        let mut out = Vec::with_capacity(changed.len() * (24 + self.page_size as usize));
        if self.wal_len == 32 && self.n_frames == 0 {
            // First commit under fresh salts: the header leads.
            out.extend_from_slice(&self.header());
        }
        for (i, (pgno, page)) in changed.iter().enumerate() {
            debug_assert_eq!(page.len(), self.page_size as usize);
            let commit = i + 1 == changed.len();
            let mut frame = [0u8; 24];
            frame[0..4].copy_from_slice(&pgno.to_be_bytes());
            frame[4..8].copy_from_slice(&if commit { db_size } else { 0 }.to_be_bytes());
            frame[8..12].copy_from_slice(&self.salt1.to_be_bytes());
            frame[12..16].copy_from_slice(&self.salt2.to_be_bytes());
            // Chain: first 8 frame-header bytes, then the page content.
            self.cksum = wal_cksum(&frame[0..8], self.cksum);
            self.cksum = wal_cksum(page, self.cksum);
            frame[16..20].copy_from_slice(&self.cksum.0.to_be_bytes());
            frame[20..24].copy_from_slice(&self.cksum.1.to_be_bytes());
            out.extend_from_slice(&frame);
            out.extend_from_slice(page);
            self.n_frames += 1;
            self.wal_len += 24 + self.page_size;
        }
        out
    }

    /// Checkpoint pressure: SQLite's default autocheckpoint fires at
    /// 1000 pages; mirror that (frames or bytes, whichever trips first).
    pub fn should_checkpoint(&self) -> bool {
        self.n_frames >= 1000
    }

    /// Bytes written under the current salts.
    pub fn wal_len(&self) -> u32 {
        self.wal_len
    }

    /// The sidecar's page size (frame geometry + diff base contract).
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Checkpoint sequence number (bumped on WAL reset).
    pub fn ckpt_seq(&self) -> u32 {
        self.ckpt_seq
    }

    /// True once frames exist under the current salts (a clean-close
    /// checkpoint only fires when there is something to fold).
    pub fn has_frames(&self) -> bool {
        self.n_frames > 0
    }

    /// Raw salt bytes (header fields 16..24) for the sidecar identity
    /// check in [`append_wal`].
    fn salt_bytes(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&self.salt1.to_be_bytes());
        b[4..8].copy_from_slice(&self.salt2.to_be_bytes());
        b
    }
}

/// The pages of `image` that differ from `base` (or lie beyond it),
/// in page order — the frame set for one commit. Page size equality
/// is the caller's contract (both images come from the same writer).
pub fn diff_pages(base: &[u8], image: &[u8], page_size: u32) -> Vec<(u32, Vec<u8>)> {
    let ps = page_size as usize;
    debug_assert!(ps > 0 && base.len() % ps == 0 && image.len() % ps == 0);
    let n_old = base.len() / ps;
    let n_new = image.len() / ps;
    let mut out = Vec::new();
    for p in 0..n_new {
        let new_page = &image[p * ps..(p + 1) * ps];
        let changed = if p < n_old {
            new_page != &base[p * ps..(p + 1) * ps]
        } else {
            true
        };
        if changed {
            out.push((p as u32 + 1, new_page.to_vec()));
        }
    }
    out
}

/// Append a commit to the sidecar, then fsync. The file must match the
/// writer's state (length + salts): a replaced/deleted sidecar is only
/// recoverable while the writer has no frames yet (the commit bytes
/// then include the header); with frames in flight the history is gone
/// and the caller must fall back to a full rewrite.
pub fn append_wal(
    path: &Path,
    writer: &WalWriter,
    pre_commit_len: u32,
    bytes: &[u8],
) -> Result<(), String> {
    use std::io::{Read, Seek, Write};
    // `pre_commit_len`: the sidecar length BEFORE this commit's frames
    // were encoded (the caller captures writer.wal_len() first — the
    // writer's own length already counts the pending frames).
    let expected = pre_commit_len as u64;
    // A fresh session: only the (planned) header is on disk — the
    // commit bytes carry the header, so starting from zero is correct.
    let fresh = pre_commit_len == 32;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let actual = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let mut seek_to = expected;
    if actual != expected {
        if !fresh {
            return Err(format!(
                "wal sidecar {} replaced externally ({} bytes, expected {expected})",
                path.display(),
                actual
            ));
        }
        // Fresh session: the header rides with this commit's bytes.
        f.set_len(0)
            .map_err(|e| format!("truncate {}: {e}", path.display()))?;
        seek_to = 0;
    } else {
        // Length matches: the on-disk header must carry OUR salts (a
        // foreign 32-byte husk with matching length would silently
        // invalidate our frames for every reader's salt check).
        let mut head = [0u8; 24];
        f.read_exact(&mut head)
            .map_err(|e| format!("read head {}: {e}", path.display()))?;
        if head[16..24] != writer.salt_bytes() {
            if !fresh {
                return Err(format!(
                    "wal sidecar {} carries foreign salts",
                    path.display()
                ));
            }
            f.set_len(0)
                .map_err(|e| format!("truncate {}: {e}", path.display()))?;
            seek_to = 0;
        }
    }
    f.seek(std::io::SeekFrom::Start(seek_to))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cksum_matches_reference() {
        // Hand-chained reference: two 8-byte chunks.
        let s = wal_cksum(&[0u8; 8], (0, 0));
        assert_eq!(s, (0, 0));
        let s = wal_cksum(&[1, 0, 0, 0, 2, 0, 0, 0], (0, 0));
        // s1 = 1 + 0 = 1; s2 = 2 + 1 = 3.
        assert_eq!(s, (1, 3));
        // Chaining: s1 = 1 + 1 + 3 = 5; s2 = 3 + 2 + 5 = 10.
        let s2 = wal_cksum(&[1, 0, 0, 0, 2, 0, 0, 0], s);
        assert_eq!(s2, (5, 10));
    }

    #[test]
    fn commit_bytes_roundtrip_shape() {
        let ps = 512u32;
        let mut w = WalWriter::new(ps, 0);
        let pages = vec![(1u32, vec![0x0du8; 512]), (2u32, vec![0x05u8; 512])];
        let bytes = w.encode_commit(&pages, 2);
        // Header + 2 frames.
        assert_eq!(bytes.len(), 32 + 2 * (24 + 512));
        // Magic + version + page size.
        assert_eq!(&bytes[0..4], &0x377f0682u32.to_be_bytes());
        assert_eq!(&bytes[4..8], &3_007_000u32.to_be_bytes());
        assert_eq!(&bytes[8..12], &ps.to_be_bytes());
        // First frame: pgno 1, db_size 0 (non-commit).
        assert_eq!(&bytes[32..36], &1u32.to_be_bytes());
        assert_eq!(&bytes[36..40], &0u32.to_be_bytes());
        // Commit frame: pgno 2, db_size 2.
        let off = 32 + 24 + 512;
        assert_eq!(&bytes[off..off + 4], &2u32.to_be_bytes());
        assert_eq!(&bytes[off + 4..off + 8], &2u32.to_be_bytes());
        // Second commit appends without a new header.
        let more = w.encode_commit(&[(3u32, vec![9u8; 512])], 3);
        assert_eq!(more.len(), 24 + 512);
        assert_eq!(&more[4..8], &3u32.to_be_bytes());
    }

    #[test]
    fn diff_pages_detects_changes() {
        let ps = 16u32;
        let base = vec![0u8; 32]; // 2 pages
        let mut image = base.clone();
        image[16] = 1; // page 2 differs
        image.extend_from_slice(&[7u8; 16]); // page 3 new
        let d = diff_pages(&base, &image, ps);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].0, 2);
        assert_eq!(d[1].0, 3);
        // Identical images diff to nothing.
        assert!(diff_pages(&base, &base, ps).is_empty());
    }
}
