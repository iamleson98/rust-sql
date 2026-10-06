//! Savepoint pre-image store with a bounded-RAM spill discipline.
//!
//! The savepoint undo machinery captures a 4 KB pre-image of every page a
//! transaction fetches (see `Pager::capture_savepoint_undo`): the bytes at
//! FETCH time are the only moment the pre-mutation state is in hand. For
//! the shapes the mega-scale marathon runs at 100M rows — a single
//! band-UPDATE that mutates a page in every leaf of a 2.3 GB table — that
//! is O(touched pages) of RAM, where SQLite journals the same images to
//! its on-disk rollback journal. This module keeps that memory bounded:
//! the most recent captures stay in a hot map; past a page-count
//! threshold the hot set drains into an append-only temp file (positioned
//! writes only — see `storage::tempstore`'s Windows position lesson), with
//! an id -> offset index left in RAM. Rollback streams images back from
//! the file. A failure to create or write the file is never fatal: the
//! log disarms its spill and keeps capturing in RAM (the pre-spill
//! behavior — correct, just unbounded).
//!
//! Wire format (little-endian): each record is `[u32 page_id][u32 len][len
//! bytes]` at the offset the index carries.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::page::PageId;

/// Default hot-map capacity before a drain into the spill file (pages).
/// 8192 pages x 4 KB = 32 MB of hot images per savepoint level — small
/// transactions (the overwhelming majority) never touch the file at all.
pub const DEFAULT_HOT_PAGES: usize = 8192;

/// Hard cap on one record's payload size (a corrupt index entry reads
/// nothing rather than a huge allocation).
const MAX_RECORD: usize = 16 << 20;

/// Read-back granularity for the rollback drain (sequential walk).
const READ_CHUNK: usize = 256 * 1024;

static FILE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path() -> PathBuf {
    let n = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rustqlite-preimage-{}-{}-{}.tmp",
        std::process::id(),
        n,
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0),
    ))
}

/// Hot page-count threshold override (tests force it low to exercise the
/// spill paths deterministically; deployments may tune it).
pub fn hot_page_threshold() -> usize {
    static CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("RSQL_PREIMAGE_HOT_PAGES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v >= 1)
            .unwrap_or(DEFAULT_HOT_PAGES)
    })
}

/// One savepoint level's pre-image log: hot map + optional spill file.
pub struct PreimageLog {
    hot: HashMap<PageId, Vec<u8>>,
    /// id -> record offset in the spill file (the RAM-resident index).
    index: HashMap<PageId, u64>,
    file: Option<File>,
    path: Option<PathBuf>,
    next_offset: u64,
    /// Latched false on any spill I/O error — capture keeps working in
    /// RAM (correct, unbounded; the pre-spill behavior).
    spill_armed: bool,
    threshold: usize,
}

impl Default for PreimageLog {
    fn default() -> Self {
        Self {
            hot: HashMap::new(),
            index: HashMap::new(),
            file: None,
            path: None,
            next_offset: 0,
            spill_armed: true,
            threshold: hot_page_threshold(),
        }
    }
}

impl PreimageLog {
    /// Record a pre-image (capture path). Idempotent: an id already in
    /// the hot set or the spill index is left exactly as first captured.
    pub fn insert(&mut self, id: PageId, bytes: Vec<u8>) {
        if self.hot.contains_key(&id) || self.index.contains_key(&id) {
            return;
        }
        self.hot.insert(id, bytes);
        if self.spill_armed && self.hot.len() >= self.threshold {
            self.drain_hot();
        }
    }

    /// Append the whole hot set into the spill file in ONE positioned
    /// write (a batched buffer — no per-page syscalls), then keep only
    /// the id -> offset index in RAM. Any I/O failure disarms the spill
    /// and leaves the hot set untouched.
    fn drain_hot(&mut self) {
        if self.hot.is_empty() {
            return;
        }
        if self.file.is_none() {
            let path = unique_temp_path();
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
            {
                Ok(f) => {
                    self.file = Some(f);
                    self.path = Some(path);
                }
                Err(_) => {
                    self.spill_armed = false;
                    return;
                }
            }
        }
        // Serialize: [u32 id][u32 len][bytes] per record.
        let payload: usize = self.hot.values().map(|v| 8 + v.len()).sum();
        let mut buf: Vec<u8> = Vec::with_capacity(payload);
        let mut offsets: Vec<(PageId, u64)> = Vec::with_capacity(self.hot.len());
        let mut cursor = self.next_offset;
        for (id, bytes) in self.hot.iter() {
            offsets.push((*id, cursor));
            buf.extend_from_slice(&(*id).to_le_bytes());
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
            cursor += 8 + bytes.len() as u64;
        }
        let file = self.file.as_mut().unwrap();
        match positioned_write_all(file, &buf, self.next_offset) {
            Ok(()) => {
                self.next_offset = cursor;
                self.hot.clear();
                for (id, off) in offsets {
                    self.index.insert(id, off);
                }
            }
            Err(_) => {
                // Keep the hot set; stop spilling (correct, RAM-boundless).
                self.spill_armed = false;
            }
        }
    }

    /// True when the log holds a pre-image for `id` (hot or spilled).
    pub fn contains(&self, id: &PageId) -> bool {
        self.hot.contains_key(id) || self.index.contains_key(id)
    }

    /// Fetch the pre-image bytes for `id` (hot first, then a positioned
    /// read from the spill file).
    pub fn get(&self, id: &PageId) -> Option<Vec<u8>> {
        if let Some(bytes) = self.hot.get(id) {
            return Some(bytes.clone());
        }
        let off = *self.index.get(id)?;
        let file = self.file.as_ref()?;
        let mut header = [0u8; 8];
        if !pread_full(file, &mut header, off) {
            return None;
        }
        let len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        if len == 0 || len > MAX_RECORD {
            return None;
        }
        let mut bytes = vec![0u8; len];
        if !pread_full(file, &mut bytes, off + 8) {
            return None;
        }
        Some(bytes)
    }

    /// Every page id this log holds (the committed-view invalidation
    /// walk's key set).
    pub fn ids(&self) -> impl Iterator<Item = &'_ PageId> {
        self.hot.keys().chain(self.index.keys())
    }

    pub fn is_empty(&self) -> bool {
        self.hot.is_empty() && self.index.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hot.len() + self.index.len()
    }

    /// Drain the WHOLE log as (id, bytes) pairs — rollback's restore
    /// walk. The hot map yields directly; the spill file streams back
    /// sequentially in chunks (no per-record seeks). Ownership of the
    /// spill file (and its cleanup) transfers to the iterator.
    pub fn drain(mut self) -> PreimageDrain {
        let file = std::mem::take(&mut self.file);
        let path = std::mem::take(&mut self.path);
        let hot = std::mem::take(&mut self.hot);
        let next_offset = self.next_offset;
        let file_iter = file.and_then(|f| SpillFileIter::new(f, path, next_offset));
        PreimageDrain {
            hot: hot.into_iter(),
            file: file_iter,
        }
    }

    #[cfg(test)]
    fn hot_is_empty_for_test(&self) -> bool {
        self.hot.is_empty()
    }

    #[cfg(test)]
    fn with_threshold(threshold: usize) -> Self {
        Self {
            hot: HashMap::new(),
            index: HashMap::new(),
            file: None,
            path: None,
            next_offset: 0,
            spill_armed: true,
            threshold,
        }
    }
}

impl IntoIterator for PreimageLog {
    type Item = (PageId, Vec<u8>);
    type IntoIter = PreimageDrain;
    fn into_iter(self) -> PreimageDrain {
        self.drain()
    }
}

/// Iterator over a whole pre-image log: hot entries first, then the
/// spill file's records in write order.
pub struct PreimageDrain {
    hot: std::collections::hash_map::IntoIter<PageId, Vec<u8>>,
    file: Option<SpillFileIter>,
}

impl Iterator for PreimageDrain {
    type Item = (PageId, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(kv) = self.hot.next() {
            return Some(kv);
        }
        self.file.as_mut()?.next()
    }
}

/// Sequential reader over the spill file's records; removes the file on
/// drop (the log itself may already be gone after `into_iter`).
struct SpillFileIter {
    file: File,
    path: Option<PathBuf>,
    buf: Vec<u8>,
    buf_valid: usize,
    buf_pos: usize,
    file_pos: u64,
    end: u64,
}

impl SpillFileIter {
    fn new(file: File, path: Option<PathBuf>, end: u64) -> Option<Self> {
        let mut it = Self {
            file,
            path,
            buf: vec![0u8; READ_CHUNK],
            buf_valid: 0,
            buf_pos: 0,
            file_pos: 0,
            end,
        };
        if end == 0 || it.fill() {
            Some(it)
        } else {
            None
        }
    }

    /// Refill the chunk buffer; false when the readable stream is over.
    fn fill(&mut self) -> bool {
        while self.buf_pos >= self.buf_valid {
            if self.file_pos >= self.end {
                self.buf_valid = 0;
                self.buf_pos = 0;
                return false;
            }
            let want = (self.end - self.file_pos).min(READ_CHUNK as u64) as usize;
            if want == 0 {
                return false;
            }
            if !pread_full(&self.file, &mut self.buf[..want], self.file_pos) {
                return false;
            }
            self.buf_valid = want;
            self.buf_pos = 0;
            self.file_pos += want as u64;
        }
        true
    }

    fn take(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(n.min(READ_CHUNK));
        while out.len() < n {
            if !self.fill() {
                return None;
            }
            let avail = self.buf_valid - self.buf_pos;
            let take = avail.min(n - out.len());
            out.extend_from_slice(&self.buf[self.buf_pos..self.buf_pos + take]);
            self.buf_pos += take;
        }
        Some(out)
    }
}

impl Iterator for SpillFileIter {
    type Item = (PageId, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        let header = self.take(8)?;
        let id = u32::from_le_bytes(header[0..4].try_into().ok()?) as PageId;
        let len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        if len == 0 || len > MAX_RECORD {
            return None; // corrupt record — stop (defensive)
        }
        let bytes = self.take(len)?;
        Some((id, bytes))
    }
}

impl Drop for SpillFileIter {
    fn drop(&mut self) {
        // Best-effort unlink (the tempstore pattern): Rust's std opens
        // files with FILE_SHARE_DELETE on Windows, so removing while the
        // handle is still open is fine there too; the handle itself
        // closes right after, as the fields drop.
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Drop for PreimageLog {
    fn drop(&mut self) {
        // Best-effort unlink — never panic during unwind; a stray temp
        // file is reclaimed by the OS temp cleaner.
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Platform positioned read: reads into `buf` at `offset` WITHOUT
/// touching the kernel file position (the tempstore position
/// discipline — see that module's Windows lesson).
#[cfg(unix)]
fn positioned_read_once(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn positioned_read_once(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn positioned_read_once(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

/// Platform positioned write (see `positioned_read_once`). NOTE: the
/// Windows `seek_write` needs `&mut File` — callers pass it.
#[cfg(unix)]
fn positioned_write_once(file: &mut File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
fn positioned_write_once(file: &mut File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(not(any(unix, windows)))]
fn positioned_write_once(file: &mut File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.write(buf)
}

fn positioned_write_all(file: &mut File, buf: &[u8], at: u64) -> io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = positioned_write_once(file, &buf[done..], at + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "pre-image spill: zero-byte positioned write",
            ));
        }
        done += n;
    }
    Ok(())
}

fn pread_full(file: &File, buf: &mut [u8], at: u64) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        match positioned_read_once(file, &mut buf[done..], at + done as u64) {
            Ok(0) => return false,
            Ok(n) => done += n,
            Err(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_only_log_round_trips() {
        let mut log = PreimageLog::default();
        assert!(log.is_empty());
        log.insert(3, vec![7u8; 100]);
        assert!(log.contains(&3));
        assert_eq!(log.get(&3).unwrap().len(), 100);
        let drained: Vec<(PageId, Vec<u8>)> = log.drain().collect();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, 3);
    }

    #[test]
    fn spill_and_read_back() {
        let mut log = PreimageLog::with_threshold(2);
        log.insert(1, vec![1u8; 4096]);
        assert!(!log.hot_is_empty_for_test(), "below threshold: stays hot");
        log.insert(2, vec![2u8; 4096]);
        // Threshold reached: hot drained to the file.
        assert!(
            log.hot_is_empty_for_test(),
            "hot set must drain at threshold"
        );
        assert_eq!(log.len(), 2);
        // Reads come back from the file, byte-exact.
        assert_eq!(log.get(&1).unwrap(), vec![1u8; 4096]);
        assert_eq!(log.get(&2).unwrap(), vec![2u8; 4096]);
        // A capture after the drain stays hot (below threshold again).
        log.insert(3, vec![3u8; 4096]);
        assert!(log.contains(&3) && !log.hot_is_empty_for_test());
        // Re-capture of a spilled id is a no-op (index hit).
        log.insert(1, vec![9u8; 4096]);
        assert_eq!(log.get(&1).unwrap(), vec![1u8; 4096]);
        // The drain iterator yields everything: file records + hot.
        let all: std::collections::HashMap<PageId, Vec<u8>> = log.drain().collect();
        assert_eq!(all.len(), 3);
        assert_eq!(all[&1], vec![1u8; 4096]);
        assert_eq!(all[&2], vec![2u8; 4096]);
        assert_eq!(all[&3], vec![3u8; 4096]);
    }

    #[test]
    fn many_batches_of_spill_round_trip() {
        // Multiple drain cycles: 10 pages through a threshold of 3.
        let mut log = PreimageLog::with_threshold(3);
        for id in 1..=10u32 {
            let mut v = vec![0u8; 4096];
            v[0..4].copy_from_slice(&id.to_le_bytes());
            log.insert(id, v.clone());
            assert_eq!(log.get(&id).unwrap(), v, "read-after-insert id {id}");
        }
        let all: std::collections::HashMap<PageId, Vec<u8>> = log.drain().collect();
        assert_eq!(all.len(), 10);
        for id in 1..=10u32 {
            let mut v = vec![0u8; 4096];
            v[0..4].copy_from_slice(&id.to_le_bytes());
            assert_eq!(all[&id], v);
        }
    }
}
