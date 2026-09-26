//! SQLite rollback journal (fileformat2.html §3) — writer and hot-journal
//! recovery, byte-exact with real SQLite so a journal left by a crashed
//! engine commit rolls back under the `sqlite3` CLI too, and a journal
//! left by a crashed real SQLite rolls back under this engine.
//!
//! Format (all integers big-endian):
//!
//! * Header (28 bytes, zero-padded to one sector):
//!   `0..8`   magic `0xd9 0xd5 0x05 0xf9 0x20 0xa1 0x63 0xd7`
//!   `8..12`  page-record count of the next segment, or `-1` (all
//!   remaining bytes are records)
//!   `12..16` random nonce seeding the page checksums
//!   `16..20` database size in pages BEFORE the transaction (the
//!   post-rollback truncation point)
//!   `20..24` sector size assumed by the writer
//!   `24..28` page size
//! * Page record: `pgno (u32) | original page content | checksum (u32)`.
//!   The checksum is SQLite's documented sparse sample: seed with the
//!   nonce, then add the byte at offsets N-200, N-400, ... (down to 0).
//! * COMMIT = the journal is deleted (or its header invalidated /
//!   truncated to zero — SQLite's three equivalent forms).
//! * ROLLBACK (hot journal at open): write every record's page back
//!   into the main file at `(pgno-1)*page_size`, truncate the file to
//!   the header's pre-transaction page count, delete the journal.
//!
//! Protocol (atomiccommit.html): a commit writes the journal (pre-images
//! of every page it is about to change) and fsyncs it, then writes the
//! changed pages into the main database file and fsyncs that, then
//! deletes the journal — the deletion is the atomic commit point. A
//! crash anywhere earlier leaves a hot journal, and the next open
//! restores the pre-transaction state. Single-writer rule as everywhere
//! in this engine: the journal offers crash atomicity, not cross-process
//! write serialization.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The journal header magic (fileformat2.html §3).
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// Sector size this writer assumes (and zero-pads the header to). 512 is
/// SQLite's own classic assumption (`SQLITE_DEFAULT_SECTOR_SIZE`); every
/// journal reader honors the sector size declared in the header, so the
/// choice is self-consistent for both us and real SQLite.
const SECTOR: u32 = 512;

/// `<path>-journal` sidecar path — same directory, same name, "-journal"
/// appended (SQLite's rule).
pub fn journal_path_of(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push("-journal");
    PathBuf::from(p)
}

/// SQLite's documented page-record checksum: seed with the journal
/// header nonce, then add the byte at offsets N-200, N-400, ... while
/// the offset is >= 0. (A page smaller than 200 bytes samples nothing —
/// the checksum stays at the seed.)
pub fn page_checksum(page: &[u8], nonce: u32) -> u32 {
    let mut cksum = nonce;
    let n = page.len() as i64;
    let mut x = n - 200;
    while x >= 0 {
        cksum = cksum.wrapping_add(page[x as usize] as u32);
        x -= 200;
    }
    cksum
}

/// One parsed journal-header segment.
#[derive(Clone, Copy, Debug)]
struct JournalHeader {
    /// Records in this segment, or -1 = to end of file.
    n_rec: i32,
    /// Nonce for this segment's page checksums.
    nonce: u32,
    /// Database size in pages before the transaction (rollback target).
    initial_db_pages: u32,
    /// Sector size the writer assumed (header stride between segments).
    sector: u32,
    /// Page size.
    page_size: u32,
}

impl JournalHeader {
    fn from_bytes(b: &[u8; 28]) -> Option<Self> {
        if b[0..8] != JOURNAL_MAGIC {
            return None;
        }
        let n_rec = i32::from_be_bytes([b[8], b[9], b[10], b[11]]);
        let nonce = u32::from_be_bytes([b[12], b[13], b[14], b[15]]);
        let initial_db_pages = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let sector = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        let page_size = u32::from_be_bytes([b[24], b[25], b[26], b[27]]);
        // Sanity gates: a header failing these is garbage or a persist-mode
        // husk, never a hot journal.
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return None;
        }
        if !(512..=65536).contains(&sector) || !sector.is_power_of_two() {
            return None;
        }
        if initial_db_pages > u32::MAX / 4 {
            return None;
        }
        Some(Self {
            n_rec,
            nonce,
            initial_db_pages,
            sector,
            page_size,
        })
    }
}

/// Write a rollback journal for the pages `old_pages` (page number,
/// pre-transaction content) — the "before any information-bearing page
/// is modified, write the original content to the journal" half of
/// SQLite's commit protocol. `initial_db_pages` is the database's page
/// count BEFORE the transaction (rollback truncation point). The journal
/// is fsynced before returning: the data-page writes that follow must
/// find it durable (atomiccommit.html's ordering).
pub fn write_journal(
    db_path: &Path,
    old_pages: &[(u32, Vec<u8>)],
    page_size: u32,
    initial_db_pages: u32,
) -> Result<(), String> {
    let jp = journal_path_of(db_path);
    let nonce = fresh_nonce();
    let mut buf: Vec<u8> =
        Vec::with_capacity(SECTOR as usize + old_pages.len() * (4 + page_size as usize + 4));
    let mut hdr = [0u8; 28];
    hdr[0..8].copy_from_slice(&JOURNAL_MAGIC);
    hdr[8..12].copy_from_slice(&(old_pages.len() as i32).to_be_bytes());
    hdr[12..16].copy_from_slice(&nonce.to_be_bytes());
    hdr[16..20].copy_from_slice(&initial_db_pages.to_be_bytes());
    hdr[20..24].copy_from_slice(&SECTOR.to_be_bytes());
    hdr[24..28].copy_from_slice(&page_size.to_be_bytes());
    buf.extend_from_slice(&hdr);
    buf.resize(SECTOR as usize, 0); // header lives in its own sector
    for (pgno, page) in old_pages {
        debug_assert_eq!(page.len(), page_size as usize, "journal page size");
        if *pgno == 0 {
            return Err("journal page number 0 is invalid".into());
        }
        buf.extend_from_slice(&pgno.to_be_bytes());
        buf.extend_from_slice(page);
        buf.extend_from_slice(&page_checksum(page, nonce).to_be_bytes());
    }
    let mut f = std::fs::File::create(&jp).map_err(|e| format!("create {}: {e}", jp.display()))?;
    f.write_all(&buf)
        .map_err(|e| format!("write {}: {e}", jp.display()))?;
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", jp.display()))?;
    Ok(())
}

/// Random-ish nonce (uniqueness matters, not unpredictability — same
/// tradeoff as the WAL writer's salts).
fn fresh_nonce() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(std::process::id())
}

/// Write the NEW page images into the main database file IN PLACE (the
/// "changed pages" half of SQLite's commit protocol) and fsync it. Pages
/// beyond the current file length extend the file (append-shaped
/// commits); page order in `pages` is page-number order from
/// `diff_pages`, so writes are sequential and later pages grow the file
/// monotonically. Cross-platform positioned writes via seek: callers
/// hold the session lock, so the shared file offset is uncontended.
pub fn write_pages(db_path: &Path, page_size: u32, pages: &[(u32, Vec<u8>)]) -> Result<(), String> {
    use std::io::{Seek, Write};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(db_path)
        .map_err(|e| format!("open {}: {e}", db_path.display()))?;
    for (pgno, page) in pages {
        debug_assert_eq!(page.len(), page_size as usize, "page size contract");
        let off = (*pgno as u64 - 1) * page_size as u64;
        f.seek(std::io::SeekFrom::Start(off))
            .map_err(|e| format!("seek {}: {e}", db_path.display()))?;
        f.write_all(page)
            .map_err(|e| format!("write page {pgno}: {e}"))?;
    }
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", db_path.display()))?;
    Ok(())
}

/// Cheap "is there a hot journal here?" probe for the open-path
/// format sniff: a crashed DELETE-mode commit can leave the MAIN
/// file's header torn (the format magic smashed mid-write) while the
/// journal holds the pre-images that restore it — so the presence of a
/// VALID journal routes the open into the SQLite-format path even
/// when the main file's magic check fails. Full validation is
/// replay's job; this only proves the journal is shaped hot (magic,
/// parseable header, at least one checksum-valid record).
pub fn has_hot_journal(db_path: &Path) -> bool {
    let bytes = match std::fs::read(journal_path_of(db_path)) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if bytes.len() < 28 + 512 {
        // Empty / truncated / invalidated header: not hot.
        return false;
    }
    let mut hdr = [0u8; 28];
    hdr.copy_from_slice(&bytes[0..28]);
    let Some(h) = JournalHeader::from_bytes(&hdr) else {
        return false;
    };
    if h.n_rec == 0 {
        return false; // invalidated (persist-mode commit marker)
    }
    let ps = h.page_size as usize;
    let rec_len = 4 + ps + 4;
    if bytes.len() < 512 + rec_len {
        return false;
    }
    let page = &bytes[512 + 4..512 + 4 + ps];
    let mut cksum_buf = [0u8; 4];
    cksum_buf.copy_from_slice(&bytes[512 + 4 + ps..512 + 8 + ps]);
    let cksum = u32::from_be_bytes(cksum_buf);
    cksum == page_checksum(page, h.nonce)
}

/// Replay a hot rollback journal onto `db_path` if one exists: restore
/// every valid page record, truncate the database back to the header's
/// pre-transaction page count, fsync, delete the journal. Returns
/// `true` when a rollback happened. A journal that is absent, empty, or
/// header-invalid is a COMMIT marker (SQLite's truncate/persist forms)
/// — it is removed and `false` returned.
///
/// Records are validated with the documented checksums; the replay
/// stops at the first invalid record (a torn write), exactly like
/// SQLite's `pager_playback`.
pub fn replay_if_hot(db_path: &Path) -> Result<bool, String> {
    let jp = journal_path_of(db_path);
    let bytes = match std::fs::read(&jp) {
        Ok(b) => b,
        Err(_) => return Ok(false), // no journal: clean
    };
    if bytes.is_empty() || bytes.len() < 28 {
        // Truncated-to-zero (journal_mode=truncate commit marker) or a
        // partial header: not hot.
        let _ = std::fs::remove_file(&jp);
        return Ok(false);
    }
    let db_exists = db_path.exists();
    // Parse segments: header, n_rec page records, then possibly another
    // header (multi-segment journals, per the format's "if the page count
    // M is greater than zero then after M page records ... another
    // journal header may be inserted").
    let mut restored: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut truncate_to: u64 = 0;
    let mut off = 0usize;
    let mut any_record = false;
    while off + 28 <= bytes.len() {
        let mut hdr_buf = [0u8; 28];
        hdr_buf.copy_from_slice(&bytes[off..off + 28]);
        let Some(hdr) = JournalHeader::from_bytes(&hdr_buf) else {
            break; // invalid/zeroed header: end of valid content
        };
        off += hdr.sector as usize; // records start after the padded header
        let ps = hdr.page_size as usize;
        let rec_len = 4 + ps + 4;
        let mut taken = 0i64;
        let limit = if hdr.n_rec < 0 {
            ((bytes.len() - off) / rec_len) as i64
        } else {
            hdr.n_rec as i64
        };
        for _ in 0..limit {
            if off + rec_len > bytes.len() {
                taken = i64::MAX; // truncated mid-record: stop everything
                break;
            }
            let pgno =
                u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
            let page = &bytes[off + 4..off + 4 + ps];
            let cksum = u32::from_be_bytes([
                bytes[off + 4 + ps],
                bytes[off + 5 + ps],
                bytes[off + 6 + ps],
                bytes[off + 7 + ps],
            ]);
            if cksum != page_checksum(page, hdr.nonce) {
                taken = i64::MAX; // torn record: stop (SQLite's rule)
                break;
            }
            if pgno == 0 || pgno > u32::MAX / 4 {
                taken = i64::MAX;
                break;
            }
            restored.push((pgno, page.to_vec()));
            any_record = true;
            off += rec_len;
            taken += 1;
        }
        if taken == i64::MAX {
            break;
        }
        truncate_to = hdr.initial_db_pages as u64 * ps as u64;
        // Pad to the next sector boundary before looking for another header.
        let stride = hdr.sector as usize;
        off = off.div_ceil(stride) * stride;
    }
    if !any_record {
        // Valid header, zero valid records: nothing to restore. This is
        // also the shape SQLite leaves after invalidating a header
        // in place (journal_mode=persist commit marker) — remove it.
        let _ = std::fs::remove_file(&jp);
        return Ok(false);
    }
    if !db_exists {
        // Orphaned journal without a database: nothing to restore onto.
        let _ = std::fs::remove_file(&jp);
        return Ok(false);
    }
    // Restore pages. Later records for the same page win (SQLite replays
    // sequentially with the same effect). Positioned writes via seek:
    // this runs at open, before any concurrent access exists.
    use std::io::Seek;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(db_path)
        .map_err(|e| format!("open {}: {e}", db_path.display()))?;
    for (pgno, page) in &restored {
        let off = (*pgno as u64 - 1) * page.len() as u64;
        f.seek(std::io::SeekFrom::Start(off))
            .map_err(|e| format!("seek page {pgno}: {e}"))?;
        f.write_all(page)
            .map_err(|e| format!("restore page {pgno}: {e}"))?;
    }
    if truncate_to > 0 {
        let cur = f.metadata().map(|m| m.len()).unwrap_or(0);
        if cur > truncate_to {
            f.set_len(truncate_to)
                .map_err(|e| format!("truncate {}: {e}", db_path.display()))?;
        }
    }
    f.sync_all()
        .map_err(|e| format!("fsync {}: {e}", db_path.display()))?;
    std::fs::remove_file(&jp).map_err(|e| format!("remove {}: {e}", jp.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rsql_rj_{}_{}", name, std::process::id()));
        for suffix in ["", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
        }
        p
    }

    fn page(fill: u8, ps: usize) -> Vec<u8> {
        vec![fill; ps]
    }

    #[test]
    fn checksum_matches_documented_algorithm() {
        // 512-byte page: offsets 312, 112 get added (512-200=312; 312-200=112;
        // 112-200 < 0 stops). Non-zero bytes there, zeros elsewhere.
        let mut pg = vec![0u8; 512];
        pg[312] = 7;
        pg[112] = 3;
        assert_eq!(page_checksum(&pg, 1000), 1000 + 7 + 3);
        // Small page: nothing sampled — checksum stays at the nonce.
        assert_eq!(page_checksum(&[0u8; 199], 42), 42);
        // 4096-byte page: offsets 3896, 3696, ..., 96 — twenty sampled
        // bytes (offset 0 is never reached: 96-200 < 0). All value 1.
        let mut big = vec![1u8; 4096];
        big[0] = 9; // offset 0 is NOT sampled
        let expect = 5u32 + 20;
        assert_eq!(page_checksum(&big, 5), expect);
    }

    #[test]
    fn journal_write_then_hot_replay_restores_pages() {
        let db = tmp("roundtrip");
        let ps = 512u32;
        // "Database": 3 pages, original contents.
        let orig: Vec<Vec<u8>> = (1..=3).map(|i| page(i as u8, ps as usize)).collect();
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&db).unwrap();
            for p in &orig {
                f.write_all(p).unwrap();
            }
        }
        // A crash mid-transaction: pages 1 and 3 already carry new content,
        // the journal holds the pre-images of 1 and 3 (page 2 untouched).
        let old_pages = vec![(1u32, orig[0].clone()), (3u32, orig[2].clone())];
        write_journal(&db, &old_pages, ps, 3).unwrap();
        assert!(
            journal_path_of(&db).exists(),
            "journal is on disk (crash state)"
        );
        {
            use std::io::{Seek, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(&page(0xAA, ps as usize)).unwrap();
            f.seek(std::io::SeekFrom::Start(2 * ps as u64)).unwrap();
            f.write_all(&page(0xBB, ps as usize)).unwrap();
        }
        assert!(replay_if_hot(&db).unwrap(), "hot journal must roll back");
        let got = std::fs::read(&db).unwrap();
        assert_eq!(got.len(), 3 * ps as usize);
        assert_eq!(&got[0..ps as usize], &orig[0][..], "page 1 restored");
        assert_eq!(
            &got[ps as usize..2 * ps as usize],
            &orig[1][..],
            "page 2 untouched"
        );
        assert_eq!(&got[2 * ps as usize..], &orig[2][..], "page 3 restored");
        assert!(
            !journal_path_of(&db).exists(),
            "journal removed after rollback"
        );
        // Second open: nothing to do.
        assert!(!replay_if_hot(&db).unwrap());
    }

    #[test]
    fn zeroed_header_and_empty_journal_are_commit_markers() {
        let db = tmp("markers");
        std::fs::write(&db, page(1, 512)).unwrap();
        // journal_mode=PERSIST husk: valid length, zeroed header.
        std::fs::write(journal_path_of(&db), vec![0u8; 512]).unwrap();
        assert!(
            !replay_if_hot(&db).unwrap(),
            "zeroed header is a commit marker"
        );
        assert!(!journal_path_of(&db).exists(), "husk removed");
        // journal_mode=TRUNCATE: zero-length journal.
        std::fs::write(journal_path_of(&db), b"").unwrap();
        assert!(!replay_if_hot(&db).unwrap());
        assert!(!journal_path_of(&db).exists());
        // No journal at all.
        assert!(!replay_if_hot(&db).unwrap());
        // Database content untouched through all of the above.
        assert_eq!(std::fs::read(&db).unwrap(), page(1, 512));
    }

    #[test]
    fn torn_record_stops_replay_and_restores_prefix() {
        let db = tmp("torn");
        let ps = 512u32;
        let orig1 = page(1, ps as usize);
        let orig2 = page(2, ps as usize);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&db).unwrap();
            f.write_all(&orig1).unwrap();
            f.write_all(&orig2).unwrap();
        }
        // Hand-build a journal: header + valid record for page 1 + a torn
        // (bad-checksum) record for page 2.
        let nonce = 0x1234_5678u32;
        let mut j = vec![0u8; SECTOR as usize];
        j[0..8].copy_from_slice(&JOURNAL_MAGIC);
        j[8..12].copy_from_slice(&(-1i32).to_be_bytes());
        j[12..16].copy_from_slice(&nonce.to_be_bytes());
        j[16..20].copy_from_slice(&2u32.to_be_bytes());
        j[20..24].copy_from_slice(&SECTOR.to_be_bytes());
        j[24..28].copy_from_slice(&ps.to_be_bytes());
        j.extend_from_slice(&1u32.to_be_bytes());
        j.extend_from_slice(&orig1);
        j.extend_from_slice(&page_checksum(&orig1, nonce).to_be_bytes());
        j.extend_from_slice(&2u32.to_be_bytes());
        j.extend_from_slice(&orig2);
        j.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // wrong checksum
        std::fs::write(journal_path_of(&db), &j).unwrap();
        // Corrupt page 1 in the db (the "partially applied" transaction).
        {
            use std::io::{Seek, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(&page(0xEE, ps as usize)).unwrap();
        }
        assert!(replay_if_hot(&db).unwrap());
        let got = std::fs::read(&db).unwrap();
        assert_eq!(&got[0..ps as usize], &orig1[..], "valid record replayed");
        assert_eq!(&got[ps as usize..], &orig2[..]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_write_at_compiles() {
        // write_all_at usage above is unix-gated in this module's replay;
        // the Windows path uses seek+write via the same std API set in
        // caller code. This test only pins the module's presence.
    }
}
