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
//!
//! POSITION DISCIPLINE (the Windows lesson): every read AND write in
//! this module is POSITIONED (`read_at` / `seek_read`, `write_at` /
//! `seek_write`) against an explicitly tracked byte cursor — nothing
//! consults or mutates the kernel file position, and readers open
//! their OWN handle instead of `try_clone`. Two reasons:
//!
//! 1. `File::try_clone` on Windows (`DuplicateHandle`) yields handles
//!    that SHARE the kernel file-object position with the original —
//!    any pointer-based I/O on one clone (or a `stream_position` on
//!    the writer while a clone's cursor moved) desynchronizes the
//!    writer's append offset and clobbers chunk bytes mid-file. On
//!    Unix a dup'd descriptor has its OWN offset, which is why the
//!    shared-position bug never reproduced there.
//! 2. The first fix (cloned readers + positioned reads) still left the
//!    WRITER on the pointer API (`stream_position` + `write_all` +
//!    header-patch seeks) — correct on Unix, fragile by construction.
//!    The append cursor is now a plain `u64` and every byte lands
//!    through a positioned write.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bound on one reader's buffering (per stream). With a handful of
/// chunk readers alive at once this bounds the read side of a merge at
/// ~256 KiB — the write side buffers similarly inside the chunk writer.
const READER_BUF: usize = 64 * 1024;

/// Write-side buffer for one chunk (positioned flushes).
const WRITER_BUF: usize = 64 * 1024;

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
    /// The append cursor — the file's total length. Chunks only ever
    /// append; the header patch writes back over its own reserved bytes.
    /// NEVER the kernel file position (see the module docs).
    len: u64,
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
            len: 0,
        })
    }

    /// Number of frozen chunks.
    pub(crate) fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Retire the first `n` chunks (bounded-fan-in merge passes consume
    /// them; the bytes stay in the file — only the chunk index shrinks —
    /// and the file is deleted on drop).
    pub(crate) fn retire_front(&mut self, n: usize) {
        self.chunks.drain(0..n.min(self.chunks.len()));
    }

    /// Begin a new chunk at the tracked append cursor: reserve the
    /// n_records header (patched on [`ChunkWriter::finish`]) and hand
    /// back a positioned writer.
    pub(crate) fn begin_chunk(&mut self) -> io::Result<ChunkWriter<'_>> {
        let offset = self.len;
        positioned_write_all(&mut self.file, &[0u8; 4], offset)?;
        self.len += 4;
        self.chunks.push(offset);
        Ok(ChunkWriter {
            file: &mut self.file,
            header_pos: offset,
            n: 0,
            buf: Vec::with_capacity(WRITER_BUF),
            file_len: &mut self.len,
        })
    }

    /// Open an independent reader positioned at chunk `ci`'s header.
    ///
    /// The reader opens its OWN handle (not `try_clone`): on Windows a
    /// duplicated handle shares the kernel file-object position with the
    /// writer, so pointer-adjacent bookkeeping on either side can
    /// desynchronize the other. An independent open has no shared state
    /// at all — the k-way merge interleaves any number of them freely.
    pub(crate) fn reader(&self, ci: usize) -> io::Result<RecordReader> {
        let f = File::open(&self.path)?;
        let mut r = RecordReader {
            file: f,
            pos: self.chunks[ci],
            buf: vec![0u8; READER_BUF],
            buf_start: 0,
            buf_filled: 0,
            buf_pos: 0,
            left: 0,
        };
        // Header: how many records this chunk holds.
        let mut hdr = [0u8; 4];
        r.read_exact(&mut hdr)?;
        r.left = u32::from_le_bytes(hdr) as u64;
        Ok(r)
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
/// All writes are POSITIONED against the tracked cursor; nothing touches
/// the kernel file position.
pub(crate) struct ChunkWriter<'a> {
    file: &'a mut File,
    header_pos: u64,
    n: u32,
    /// Staged record bytes (flushed at WRITER_BUF).
    buf: Vec<u8>,
    /// The file's append cursor (shared with the owner) — advanced by
    /// exactly the bytes flushed.
    file_len: &'a mut u64,
}

impl ChunkWriter<'_> {
    /// Append one record's bytes (framing added here).
    pub(crate) fn write_record(&mut self, rec: &[u8]) -> io::Result<()> {
        self.buf
            .extend_from_slice(&(rec.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(rec);
        self.n += 1;
        if self.buf.len() >= WRITER_BUF {
            self.flush_buf()?;
        }
        Ok(())
    }

    /// Push staged bytes down through one positioned write.
    fn flush_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        positioned_write_all(self.file, &self.buf, *self.file_len)?;
        *self.file_len += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Flush records, patch the chunk header with the final count, and
    /// push the buffered bytes down to the file.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.flush_buf()?;
        positioned_write_all(self.file, &self.n.to_le_bytes(), self.header_pos)
    }
}

/// Sequential record reader over one chunk — POSITIONED reads with a
/// private cursor (see [`EphemeralFile::reader`]'s docs). The 64 KiB
/// buffer keeps the syscall count at one per buffer; a record never
/// spans a buffer seam unmanaged (the refill logic below serves both
/// halves).
pub(crate) struct RecordReader {
    file: File,
    /// Private logical cursor (never the fd offset).
    pos: u64,
    /// Read buffer: valid bytes are `buf[buf_pos .. buf_filled]`, backed
    /// by file bytes at `buf_start .. buf_start + buf_filled`.
    buf: Vec<u8>,
    buf_start: u64,
    buf_filled: usize,
    buf_pos: usize,
    left: u64,
}

impl RecordReader {
    /// Fill the buffer from the private cursor, discarding any consumed
    /// prefix. Short reads at EOF leave `buf_filled < capacity` and are
    /// fine (the caller's exact-reads then fail, surfacing corruption).
    fn refill(&mut self) -> io::Result<()> {
        self.buf_start = self.pos;
        self.buf_pos = 0;
        let n = positioned_read(&self.file, &mut self.buf, self.pos)?;
        self.buf_filled = n;
        self.pos += n as u64;
        Ok(())
    }

    /// Bytes currently buffered ahead of the cursor.
    #[inline]
    fn buffered(&self) -> usize {
        self.buf_filled - self.buf_pos
    }

    /// Read exactly `out.len()` bytes at the private cursor.
    fn read_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < out.len() {
            if self.buffered() == 0 {
                self.refill()?;
                if self.buffered() == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "temp-store chunk truncated",
                    ));
                }
            }
            let take = (out.len() - done).min(self.buffered());
            out[done..done + take].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + take]);
            self.buf_pos += take;
            done += take;
        }
        Ok(())
    }

    /// Pull the next record into `buf` (reused capacity). `Ok(None)` at
    /// chunk end. `buf` is cleared first, so a record's bytes are always
    /// exactly `buf` after a successful call.
    pub(crate) fn next_record(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<()>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        let mut hdr = [0u8; 4];
        self.read_exact(&mut hdr)?;
        let len = u32::from_le_bytes(hdr) as usize;
        buf.clear();
        buf.resize(len, 0);
        self.read_exact(buf)?;
        Ok(Some(()))
    }
}

/// Platform positioned read: reads at `offset` WITHOUT touching the fd's
/// shared file offset (pread on Unix, seek_read on Windows), looping for
/// exactness (a single call may return short — the WAL writer holds the
/// same discipline).
fn positioned_read(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = positioned_read_once(file, &mut buf[done..], offset + done as u64)?;
        if n == 0 {
            break; // EOF
        }
        done += n;
    }
    Ok(done)
}

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
    use std::io::Read;
    let mut f = file.try_clone()?;
    use std::io::Seek;
    use std::io::SeekFrom;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

/// Platform positioned write: writes `buf` at `offset` WITHOUT touching
/// the fd's shared file offset (pwrite on Unix, seek_write on Windows),
/// looping until every byte lands.
fn positioned_write_all(file: &mut File, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = positioned_write_once(file, &buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "temp-store positioned write wrote nothing",
            ));
        }
        done += n;
    }
    Ok(())
}

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
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.write(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writer/reader position discipline: interleaved readers and
    /// an active appender never disturb each other (the Windows
    /// shared-file-object failure class — the first reader's clone
    /// shared the kernel position with the writer's append cursor).
    #[test]
    fn interleaved_reader_writer_position_discipline() {
        let mut ef = EphemeralFile::create().unwrap();
        // Chunk 0: three records.
        let mut w = ef.begin_chunk().unwrap();
        w.write_record(b"alpha").unwrap();
        w.write_record(b"beta").unwrap();
        w.write_record(b"gamma-delta-epsilon").unwrap();
        w.finish().unwrap();
        // Hold a reader open mid-chunk 0...
        let mut r0 = ef.reader(0).unwrap();
        let mut buf = Vec::new();
        r0.next_record(&mut buf).unwrap();
        assert_eq!(&buf, b"alpha");
        // ...while more chunks append (the old bug: the writer's
        // stream_position was polluted by the reader's cursor on
        // Windows, clobbering chunk bytes).
        let mut w1 = ef.begin_chunk().unwrap();
        w1.write_record(b"zeta").unwrap();
        w1.finish().unwrap();
        let mut w2 = ef.begin_chunk().unwrap();
        w2.write_record(b"eta").unwrap();
        w2.write_record(b"theta").unwrap();
        w2.finish().unwrap();
        // Reader 0 continues EXACTLY where it left off.
        r0.next_record(&mut buf).unwrap();
        assert_eq!(&buf, b"beta");
        r0.next_record(&mut buf).unwrap();
        assert_eq!(&buf, b"gamma-delta-epsilon");
        assert!(r0.next_record(&mut buf).unwrap().is_none());
        // Fresh readers over the later chunks.
        let mut r1 = ef.reader(1).unwrap();
        r1.next_record(&mut buf).unwrap();
        assert_eq!(&buf, b"zeta");
        assert!(r1.next_record(&mut buf).unwrap().is_none());
        let mut r2 = ef.reader(2).unwrap();
        r2.next_record(&mut buf).unwrap();
        assert_eq!(&buf, b"eta");
        r2.next_record(&mut buf).unwrap();
        assert_eq!(&buf, b"theta");
        assert!(r2.next_record(&mut buf).unwrap().is_none());
        // Many interleaved readers over one big chunk (buffer refills).
        let mut wb = ef.begin_chunk().unwrap();
        let big = vec![0xA5u8; 200_000];
        for i in 0..64 {
            let mut rec = format!("rec-{i:04}-").into_bytes();
            rec.extend_from_slice(&big[..1000 + i * 512]);
            wb.write_record(&rec).unwrap();
        }
        wb.finish().unwrap();
        let ci = ef.chunk_count() - 1;
        let mut readers: Vec<RecordReader> = (0..8).map(|_| ef.reader(ci).unwrap()).collect();
        // Round-robin reads: every stream advances independently.
        for round in 0..64 {
            for r in readers.iter_mut() {
                r.next_record(&mut buf).unwrap();
                let expect = format!("rec-{round:04}-").into_bytes();
                assert_eq!(&buf[..9], &expect[..], "round {round}");
                assert_eq!(buf.len(), 9 + 1000 + round * 512);
            }
        }
        for r in readers.iter_mut() {
            assert!(r.next_record(&mut buf).unwrap().is_none());
        }
    }

    /// Record framing across the reader-buffer seam: records larger
    /// than one 64 KiB buffer (spanning refills) frame exactly.
    #[test]
    fn records_spanning_buffer_seams() {
        let mut ef = EphemeralFile::create().unwrap();
        let mut w = ef.begin_chunk().unwrap();
        let seam = READER_BUF - 10; // first record ends 10 bytes shy of the seam
        let rec0 = vec![0x11u8; seam];
        w.write_record(&rec0).unwrap();
        let rec1 = vec![0x22u8; 200_000]; // spans several refills
        w.write_record(&rec1).unwrap();
        let rec2 = vec![0x33u8; 3];
        w.write_record(&rec2).unwrap();
        w.finish().unwrap();
        let mut r = ef.reader(0).unwrap();
        let mut buf = Vec::new();
        r.next_record(&mut buf).unwrap();
        assert_eq!(buf, rec0);
        r.next_record(&mut buf).unwrap();
        assert_eq!(buf, rec1);
        r.next_record(&mut buf).unwrap();
        assert_eq!(buf, rec2);
        assert!(r.next_record(&mut buf).unwrap().is_none());
    }
}
