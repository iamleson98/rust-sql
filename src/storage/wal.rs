//! Write-Ahead Log (WAL).
//!
//! The WAL is a separate file (`<db>-wal`) containing committed page writes
//! that haven't been checkpointed into the main database file yet. The
//! format is intentionally simple:
//!
//! ```text
//! +-----------------------------+
//! | WAL header (32 bytes)       |
//! +-----------------------------+
//! | frame 0:                    |
//! |   frame header (24 bytes)   |
//! |   page data (page_size B)   |
//! +-----------------------------+
//! | frame 1: ...                |
//! +-----------------------------+
//! ```
//!
//! Each frame header contains:
//! - page number (u32 BE)
//! - commit marker (u32 BE; nonzero if this frame ends a transaction)
//! - salt1, salt2 (copied from the WAL header)
//! - checksum1, checksum2 (running CRC32, validated on recovery)
//!
//! On recovery, we replay all committed frames in order. On checkpoint,
//! we copy the latest version of each page from the WAL into the main file
//! and reset the WAL.

use crate::error::{Error, Result};
use crate::storage::page::PageId;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// WAL header is 32 bytes.
pub const WAL_HEADER_SIZE: u32 = 32;
/// Frame header is 24 bytes.
pub const FRAME_HEADER_SIZE: u32 = 24;

/// Magic value at the start of the WAL header (big-endian u32).
pub const WAL_MAGIC: u32 = 0x5253514C; // "RSQL"

/// The WAL header.
#[derive(Debug, Clone)]
pub struct WalHeader {
    pub magic: u32,
    pub format_version: u32,
    pub page_size: u32,
    pub checkpoint_seq: u32,
    pub salt1: u32,
    pub salt2: u32,
    pub checksum1: u32,
    pub checksum2: u32,
}

impl WalHeader {
    pub fn new(page_size: u32) -> Self {
        let salt1 = rand_u32();
        let salt2 = rand_u32();
        let (c1, c2) = crc32(0, 0, &[]);
        Self {
            magic: WAL_MAGIC,
            format_version: 1,
            page_size,
            checkpoint_seq: 0,
            salt1,
            salt2,
            checksum1: c1,
            checksum2: c2,
        }
    }

    pub fn encode(&self) -> [u8; WAL_HEADER_SIZE as usize] {
        let mut buf = [0u8; WAL_HEADER_SIZE as usize];
        buf[0..4].copy_from_slice(&self.magic.to_be_bytes());
        buf[4..8].copy_from_slice(&self.format_version.to_be_bytes());
        buf[8..12].copy_from_slice(&self.page_size.to_be_bytes());
        buf[12..16].copy_from_slice(&self.checkpoint_seq.to_be_bytes());
        buf[16..20].copy_from_slice(&self.salt1.to_be_bytes());
        buf[20..24].copy_from_slice(&self.salt2.to_be_bytes());
        buf[24..28].copy_from_slice(&self.checksum1.to_be_bytes());
        buf[28..32].copy_from_slice(&self.checksum2.to_be_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < WAL_HEADER_SIZE as usize {
            return Err(Error::corruption("WAL header too small"));
        }
        Ok(Self {
            magic: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            format_version: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            page_size: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            checkpoint_seq: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
            salt1: u32::from_be_bytes(buf[16..20].try_into().unwrap()),
            salt2: u32::from_be_bytes(buf[20..24].try_into().unwrap()),
            checksum1: u32::from_be_bytes(buf[24..28].try_into().unwrap()),
            checksum2: u32::from_be_bytes(buf[28..32].try_into().unwrap()),
        })
    }
}

/// A single frame header.
#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub page_id: PageId,
    pub commit: u32,
    pub salt1: u32,
    pub salt2: u32,
    pub checksum1: u32,
    pub checksum2: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_SIZE as usize] {
        let mut buf = [0u8; FRAME_HEADER_SIZE as usize];
        buf[0..4].copy_from_slice(&self.page_id.to_be_bytes());
        buf[4..8].copy_from_slice(&self.commit.to_be_bytes());
        buf[8..12].copy_from_slice(&self.salt1.to_be_bytes());
        buf[12..16].copy_from_slice(&self.salt2.to_be_bytes());
        buf[16..20].copy_from_slice(&self.checksum1.to_be_bytes());
        buf[20..24].copy_from_slice(&self.checksum2.to_be_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < FRAME_HEADER_SIZE as usize {
            return Err(Error::corruption("frame header too small"));
        }
        Ok(Self {
            page_id: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            commit: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            salt1: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            salt2: u32::from_be_bytes(buf[12..16].try_into().unwrap()),
            checksum1: u32::from_be_bytes(buf[16..20].try_into().unwrap()),
            checksum2: u32::from_be_bytes(buf[20..24].try_into().unwrap()),
        })
    }
}

/// CRC32 checksum used by the WAL. Uses the IEEE polynomial (same as `crc32fast`).
/// The WAL uses a running checksum: each frame's checksum continues from the
/// previous frame's checksum, so a torn write at frame N invalidates frames N+1, N+2, ...
pub fn crc32(prev1: u32, prev2: u32, data: &[u8]) -> (u32, u32) {
    // SQLite uses a custom checksum; we use a simpler scheme: two CRC32s
    // with different seeds for collision resistance.
    let mut h1 = crc32fast::Hasher::new_with_initial(prev1);
    h1.update(data);
    let c1 = h1.finalize();

    let mut h2 = crc32fast::Hasher::new_with_initial(prev2 ^ 0xA5A5A5A5);
    h2.update(data);
    let c2 = h2.finalize();
    (c1, c2)
}

fn rand_u32() -> u32 {
    // Use system time as a cheap entropy source.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos.wrapping_mul(2654435761).wrapping_add(0x12345678)
}

/// The WAL file.
pub struct Wal {
    file: File,
    path: PathBuf,
    pub header: WalHeader,
    page_size: u32,
    /// Running checksum state.
    checksum: (u32, u32),
    /// Number of frames written since the last checkpoint.
    n_frames: u32,
    /// True if we need to recreate the WAL on next write (after checkpoint).
    reset_pending: bool,
}

impl Wal {
    /// Open or create the WAL file alongside the main database file.
    pub fn open<P: AsRef<Path>>(db_path: P, page_size: u32) -> Result<Self> {
        let path = wal_path_for(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let mut wal = Self {
            file,
            path,
            header: WalHeader::new(page_size),
            page_size,
            checksum: (0, 0),
            n_frames: 0,
            reset_pending: false,
        };

        wal.recover()?;
        Ok(wal)
    }

    /// Recover the WAL: read the header and validate frames.
    fn recover(&mut self) -> Result<()> {
        let file_size = self.file.metadata()?.len();
        if file_size == 0 {
            // Fresh WAL: write the header.
            self.header = WalHeader::new(self.page_size);
            self.checksum = (self.header.checksum1, self.header.checksum2);
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&self.header.encode())?;
            self.file.sync_all()?;
            self.n_frames = 0;
            return Ok(());
        }

        // Read existing header.
        let mut header_buf = [0u8; WAL_HEADER_SIZE as usize];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut header_buf)?;
        let header = WalHeader::decode(&header_buf)?;
        if header.magic != WAL_MAGIC {
            // Stale WAL from another engine — reset.
            return self.reset();
        }
        self.header = header.clone();
        self.checksum = (header.checksum1, header.checksum2);
        self.page_size = header.page_size;

        // Count valid frames by walking the WAL and verifying checksums.
        let frame_size = (FRAME_HEADER_SIZE + self.page_size) as u64;
        let n_frames_in_file = (file_size - WAL_HEADER_SIZE as u64) / frame_size;
        let mut last_valid_frame: u32 = 0;
        let mut running_checksum = self.checksum;

        for i in 0..n_frames_in_file {
            let offset = WAL_HEADER_SIZE as u64 + i * frame_size;
            self.file.seek(SeekFrom::Start(offset))?;
            let mut fh_buf = [0u8; FRAME_HEADER_SIZE as usize];
            if self.file.read(&mut fh_buf)? < FRAME_HEADER_SIZE as usize {
                break;
            }
            let fh = FrameHeader::decode(&fh_buf)?;
            if fh.salt1 != self.header.salt1 || fh.salt2 != self.header.salt2 {
                break;
            }
            let mut page_buf = vec![0u8; self.page_size as usize];
            if self.file.read(&mut page_buf)? < self.page_size as usize {
                break;
            }
            // Verify checksum: include frame header bytes [0..8] (page_id + commit)
            // and page data, with running checksum.
            let mut check_data = Vec::with_capacity(8 + page_buf.len());
            check_data.extend_from_slice(&fh_buf[0..8]);
            check_data.extend_from_slice(&page_buf);
            let (c1, c2) = crc32(running_checksum.0, running_checksum.1, &check_data);
            if c1 != fh.checksum1 || c2 != fh.checksum2 {
                // Torn write — stop here.
                break;
            }
            running_checksum = (c1, c2);
            last_valid_frame = (i + 1) as u32;
        }
        self.n_frames = last_valid_frame;
        self.checksum = running_checksum;
        Ok(())
    }

    /// Reset the WAL to empty. Called after a checkpoint.
    pub fn reset(&mut self) -> Result<()> {
        self.header = WalHeader::new(self.page_size);
        self.checksum = (self.header.checksum1, self.header.checksum2);
        self.n_frames = 0;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.header.encode())?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Append a frame to the WAL. The frame is not committed unless `commit`
    /// is true (in which case the WAL is fsynced).
    pub fn append(&mut self, page_id: PageId, data: &[u8], commit: bool) -> Result<()> {
        if data.len() != self.page_size as usize {
            return Err(Error::InvalidArgument(format!(
                "WAL append: data length {} != page_size {}",
                data.len(),
                self.page_size
            )));
        }
        let offset = WAL_HEADER_SIZE as u64 + self.n_frames as u64 * (FRAME_HEADER_SIZE + self.page_size) as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        // Compute checksum: include page_id + commit (8 bytes) + page data.
        let mut check_data = Vec::with_capacity(8 + data.len());
        check_data.extend_from_slice(&page_id.to_be_bytes());
        check_data.extend_from_slice(&(if commit { 1u32 } else { 0u32 }).to_be_bytes());
        check_data.extend_from_slice(data);
        let (c1, c2) = crc32(self.checksum.0, self.checksum.1, &check_data);
        self.checksum = (c1, c2);

        let fh = FrameHeader {
            page_id,
            commit: if commit { 1 } else { 0 },
            salt1: self.header.salt1,
            salt2: self.header.salt2,
            checksum1: c1,
            checksum2: c2,
        };
        self.file.write_all(&fh.encode())?;
        self.file.write_all(data)?;
        if commit {
            self.file.sync_all()?;
        }
        self.n_frames += 1;
        Ok(())
    }

    /// Iterate over all valid frames in the WAL. The closure receives
    /// (page_id, data, is_commit).
    pub fn for_each_frame<F: FnMut(PageId, &[u8], bool)>(&mut self, mut f: F) -> Result<()> {
        let frame_size = (FRAME_HEADER_SIZE + self.page_size) as u64;
        let mut running_checksum = (self.header.checksum1, self.header.checksum2);
        for i in 0..self.n_frames {
            let offset = WAL_HEADER_SIZE as u64 + i as u64 * frame_size;
            self.file.seek(SeekFrom::Start(offset))?;
            let mut fh_buf = [0u8; FRAME_HEADER_SIZE as usize];
            self.file.read_exact(&mut fh_buf)?;
            let fh = FrameHeader::decode(&fh_buf)?;
            let mut page_buf = vec![0u8; self.page_size as usize];
            self.file.read_exact(&mut page_buf)?;
            let mut check_data = Vec::with_capacity(8 + page_buf.len());
            check_data.extend_from_slice(&fh_buf[0..8]);
            check_data.extend_from_slice(&page_buf);
            let (c1, c2) = crc32(running_checksum.0, running_checksum.1, &check_data);
            if c1 != fh.checksum1 || c2 != fh.checksum2 {
                break;
            }
            running_checksum = (c1, c2);
            f(fh.page_id, &page_buf, fh.commit != 0);
        }
        Ok(())
    }

    /// Number of valid frames currently in the WAL.
    pub fn n_frames(&self) -> u32 {
        self.n_frames
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }
}

/// Compute the WAL path for a given database path.
pub fn wal_path_for<P: AsRef<Path>>(db_path: P) -> PathBuf {
    let p = db_path.as_ref();
    let mut s = p.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn wal_create_and_recover() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path(), 4096).unwrap();
        let data1 = vec![1u8; 4096];
        let data2 = vec![2u8; 4096];
        wal.append(1, &data1, false).unwrap();
        wal.append(2, &data2, true).unwrap();
        assert_eq!(wal.n_frames(), 2);

        // Reopen and verify recovery.
        drop(wal);
        let mut wal = Wal::open(tmp.path(), 4096).unwrap();
        assert_eq!(wal.n_frames(), 2);
        let mut pages = Vec::new();
        wal.for_each_frame(|id, data, commit| {
            pages.push((id, data.to_vec(), commit));
        }).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].0, 1);
        assert_eq!(pages[1].0, 2);
        assert!(pages[1].2);
    }

    #[test]
    fn wal_checkpoint_resets() {
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path(), 4096).unwrap();
        wal.append(1, &vec![42u8; 4096], true).unwrap();
        wal.reset().unwrap();
        assert_eq!(wal.n_frames(), 0);
    }
}
