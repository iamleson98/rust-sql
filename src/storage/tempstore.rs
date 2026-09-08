//! Ephemeral (temp-store) spill files — the engine's equivalent of
//! SQLite's temporary b-trees.
//!
//! SQLite materializes big ephemeral structures (GROUP BY / DISTINCT /
//! ORDER BY sorters) through disk-backed b-trees with a small page cache,
//! so their memory profile is flat regardless of cardinality. The engine's
//! aggregate grouper is a hash table for speed; when the group cardinality
//! crosses the spill threshold, it freezes its accumulated groups into an
//! [`EphemeralFile`] chunk and continues with a fresh table — bounded RAM,
//! exact same answers. The final result streams back through a k-way merge
//! of the frozen chunks plus the residual in-memory table.
//!
//! Files live in the OS temp directory (like SQLite's SQLITE_TMPDIR /
//! temp files), carry unique per-process names, and are deleted on drop.
//! A failure to create or write the file is never fatal: the grouper
//! disarms its spill and keeps everything in memory (the pre-spill
//! behavior).
//!
//! Wire format (little-endian throughout):
//!
//! ```text
//! file    := chunk*
//! chunk   := u32 n_records, record*
//! record  := u32 rec_len, rec_bytes[rec_len]
//! ```
//!
//! `rec_bytes` is opaque to this module — the grouper encodes
//! `(seq, group keys, AggState`s)` with its own codec. Chunks are
//! append-only; each chunk is self-delimiting so any number of readers
//! can scan them independently (one per k-way merge stream).

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bound on one reader's buffering (per stream). With a handful of
/// chunk readers alive at once this bounds the read side of a merge at
/// ~256 KiB — the write side buffers similarly inside the chunk writer.
const READER_BUF: usize = 64 * 1024;

static FILE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path() -> PathBuf {
    let n = FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rustqlite-ephemeral-{}-{}-{}.tmp",
        std::process::id(),
        n,
        // A few bits of clock so two engines in one process (and across
        // restarts) never collide on a leftover name.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0),
    ))
}

/// An append-only sequence of chunks in one temp file, deleted on drop.
pub(crate) struct EphemeralFile {
    path: PathBuf,
    file: File,
    /// Byte offset of each frozen chunk's header, in freeze order.
    chunks: Vec<u64>,
}

impl EphemeralFile {
    /// Create the backing file. Errors (read-only /tmp, name collision,
    /// fd exhaustion) propagate to the caller, which disarms the spill.
    pub(crate) fn create() -> io::Result<Self> {
        let path = unique_temp_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            chunks: Vec::new(),
        })
    }

    /// Number of frozen chunks.
    pub(crate) fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Begin a new chunk: returns a writer that streams records out and
    /// seals the chunk with its record count on [`ChunkWriter::finish`].
    pub(crate) fn begin_chunk(&mut self) -> io::Result<ChunkWriter<'_>> {
        let offset = self.file.stream_position()?;
        // Reserve the n_records header now; the writer patches it after
        // the last record is flushed (the file stays consistent because
        // readers are only opened after the whole group-by is built).
        self.file.write_all(&[0u8; 4])?;
        self.chunks.push(offset);
        Ok(ChunkWriter {
            inner: BufWriter::with_capacity(READER_BUF, &mut self.file),
            header_pos: offset,
            n: 0,
        })
    }

    /// Open an independent reader positioned at chunk `ci`'s header.
    /// Each reader clones the fd and seeks, so any number can be in
    /// flight without sharing cursor state.
    pub(crate) fn reader(&self, ci: usize) -> io::Result<RecordReader> {
        let mut f = self.file.try_clone()?;
        f.seek(SeekFrom::Start(self.chunks[ci]))?;
        let mut br = BufReader::with_capacity(READER_BUF, f);
        // Header: how many records this chunk holds.
        let mut hdr = [0u8; 4];
        br.read_exact(&mut hdr)?;
        let n = u32::from_le_bytes(hdr) as u64;
        Ok(RecordReader { inner: br, left: n })
    }
}

impl Drop for EphemeralFile {
    fn drop(&mut self) {
        // Best-effort unlink — a stray temp file is reclaimed by the OS
        // temp cleaner; never panic during unwind.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Streaming writer for one chunk. Each record is length-prefixed so the
/// reader can frame it without trusting the payload's own structure.
pub(crate) struct ChunkWriter<'a> {
    inner: BufWriter<&'a mut File>,
    header_pos: u64,
    n: u32,
}

impl ChunkWriter<'_> {
    /// Append one record's bytes (framing added here).
    pub(crate) fn write_record(&mut self, rec: &[u8]) -> io::Result<()> {
        self.inner.write_all(&(rec.len() as u32).to_le_bytes())?;
        self.inner.write_all(rec)?;
        self.n += 1;
        Ok(())
    }

    /// Flush records, patch the chunk header with the final count, and
    /// push the buffered bytes down to the file.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.inner.flush()?;
        let file = self.inner.get_mut();
        let cur = file.stream_position()?;
        file.seek(SeekFrom::Start(self.header_pos))?;
        file.write_all(&self.n.to_le_bytes())?;
        file.seek(SeekFrom::Start(cur))?;
        Ok(())
    }
}

/// Sequential record reader over one chunk.
pub(crate) struct RecordReader {
    inner: BufReader<File>,
    left: u64,
}

impl RecordReader {
    /// Pull the next record into `buf` (reused capacity). `Ok(None)` at
    /// chunk end. `buf` is cleared first, so a record's bytes are always
    /// exactly `buf` after a successful call.
    pub(crate) fn next_record(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<()>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        let mut hdr = [0u8; 4];
        self.inner.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr) as usize;
        buf.clear();
        buf.resize(len, 0);
        self.inner.read_exact(buf)?;
        Ok(Some(()))
    }
}
