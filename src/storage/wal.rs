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

/// Positioned exact-read that works on both Unix and Windows.
/// - Unix: `FileExt::read_exact_at` (pread)
/// - Windows: `FileExt::seek_read` (pread-equivalent; looped for exactness)
/// - other platforms: seek+read (serialized, still correct)
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            ));
        }
        done += n;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    let mut file = file;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

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

/// Process-wide WAL/engine lifecycle guard.
///
/// Serializes the close-time teardown of a retiring engine (`Pager::drop`:
/// checkpoint into the main file + sidecar removal, and
/// `Pager::disable_wal`) against the birth of the NEXT engine generation
/// for the same path (`Database::open`: main-file read + catalog load +
/// legacy-format upgrade; `Pager::enable_wal`: WAL file creation).
///
/// Why this exists — the 2026-09-18 datxevui.com outage: the shared
/// engine's refcount hit zero (sqlx's pool retiring its whole startup
/// cohort together at `max_lifetime`, default 30 min) while a request
/// thread's pool-acquire was already creating the next generation.
/// `Engine::drop` → `Pager::drop` ran `checkpoint + remove_file(<db>-wal)`
/// with no coordination: the removal deleted the WAL file the NEW engine
/// had just created at that path, stranding it on a deleted inode — every
/// subsequent commit landed in the unlinked file (invisible, nothing
/// durable on disk), and the layout eventually degraded into
/// `(code 10) failed to fill whole buffer` and
/// `(code 11) WAL frame salt mismatch` (SQLITE_CORRUPT) on every write.
///
/// Reentrant (same-thread): `Database::open` holds the guard across the
/// legacy-format upgrade, whose temporary pager drops — and therefore
/// checkpoints — on the same thread.
pub fn lifecycle_guard() -> &'static parking_lot::ReentrantMutex<()> {
    static GUARD: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());
    &GUARD
}

/// Per-path engine GENERATION epochs, keyed by the sidecar path (the
/// same key the WAL writer lease uses). Every file-backed `Pager` open
/// under the lifecycle guard bumps its path's epoch; a pager remembers
/// the epoch it was born in; its close-time teardown compares the two
/// and SKIPS the close-time checkpoint + sidecar removal when a newer
/// generation has since opened the path.
///
/// Why — the stale-teardown clobber (the wal_delete_race hunt,
/// 2026-09-26): a dying generation's teardown can run AFTER a successor
/// generation already lived and retired (the teardown's guard
/// acquisition can be arbitrarily delayed past the successor's whole
/// lifetime). Its close-time checkpoint would then write its OWN — by
/// then STALE — committed map over the successor's newer main file
/// (undoing the successor's committed rows), and its inode-identity
/// sidecar removal cannot distinguish "my log" from "the successor's
/// log at the same reused inode". The `retired` flag covers VACUUM's
/// in-place replacement; this covers NORMAL generation boundaries —
/// same hazard class, same fix shape: the old pager never writes after
/// a newer one existed.
///
/// The map is monotone and never shrinks (one small entry per database
/// path ever opened by the process — the engines registry's Weak map
/// already has the same shape).
///
/// The shared per-path epoch map — ONE static behind ONE accessor. (A
/// `static` item inside each fn body is a DIFFERENT map per function:
/// the first cut of this had `bump_generation` and `current_generation`
/// each own a private `GENERATIONS`, so `current` always read an empty
/// map, every pager looked superseded, and every close-time
/// fold+removal silently skipped — caught the same day by the
/// sidecar_probe: the main file never grew, the -wal survived close.)
fn generation_epochs() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, u64>> {
    static GENERATIONS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>,
    > = std::sync::OnceLock::new();
    GENERATIONS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn bump_generation(sidecar: &Path) -> u64 {
    let mut g = generation_epochs()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let next = g.get(sidecar).copied().unwrap_or(0) + 1;
    g.insert(sidecar.to_path_buf(), next);
    next
}

/// The path's current generation epoch (0 = no pager ever bumped it —
/// treat as "no successor exists").
pub fn current_generation(sidecar: &Path) -> u64 {
    let g = generation_epochs()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    g.get(sidecar).copied().unwrap_or(0)
}

/// Stable on-disk file identity: (device, inode) on Unix, (volume serial,
/// file index) on Windows. `None` where the platform or the metadata read
/// cannot provide it — callers must treat `None` as "do NOT remove".
#[cfg(unix)]
pub fn file_identity(file: &File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(windows)]
pub fn file_identity(file: &File) -> Option<(u64, u64)> {
    // GetFileInformationByHandle via raw FFI: std's
    // `MetadataExt::file_index`/`volume_serial_number` are unstable
    // (`windows_by_handle`), which broke the Windows build. The volume
    // serial + 64-bit file index is a solid on-disk identity (ReFS's
    // 128-bit indices are truncated — collision risk is negligible for
    // close-time ownership checks, and the lifecycle guard makes even a
    // collision harmless).
    use std::os::raw::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[allow(non_snake_case)]
    struct ByHandleFileInformation {
        dwFileAttributes: u32,
        ftCreationTime: [u32; 2],
        ftLastAccessTime: [u32; 2],
        ftLastWriteTime: [u32; 2],
        dwVolumeSerialNumber: u32,
        nFileSizeHigh: u32,
        nFileSizeLow: u32,
        nNumberOfLinks: u32,
        nFileIndexHigh: u32,
        nFileIndexLow: u32,
    }
    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            hFile: *mut c_void,
            lpFileInformation: *mut ByHandleFileInformation,
        ) -> i32;
    }
    let mut info = unsafe { std::mem::zeroed::<ByHandleFileInformation>() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as *mut c_void, &mut info) };
    if ok == 0 {
        return None;
    }
    let index = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Some((info.dwVolumeSerialNumber as u64, index))
}

#[cfg(not(any(unix, windows)))]
pub fn file_identity(file: &File) -> Option<(u64, u64)> {
    let _ = file;
    None
}

/// Identity of whatever file currently sits at `path` (`None` if the
/// path is vacant or unreadable).
#[cfg(unix)]
pub fn path_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(windows)]
pub fn path_identity(path: &Path) -> Option<(u64, u64)> {
    // Open the file read-only and identify the handle (std metadata has
    // no stable file-index access on Windows). Failure to open → None →
    // the caller does not remove — fail-safe.
    let file = File::open(path).ok()?;
    file_identity(&file)
}

#[cfg(not(any(unix, windows)))]
pub fn path_identity(path: &Path) -> Option<(u64, u64)> {
    let _ = path;
    None
}

/// Remove the file at `path` ONLY when it is still the same on-disk file
/// as `owned` (the identity of a handle this side owns). A mismatch means
/// a newer engine generation created its own file at this path while the
/// owner was retiring — deleting it would strand that generation on a
/// deleted inode (see [`lifecycle_guard`]). No file / no identity → no
/// removal (fail-safe).
pub fn remove_file_if_owned(path: &Path, owned: Option<(u64, u64)>) {
    let Some(owned) = owned else { return };
    match path_identity(path) {
        Some(current) if current == owned => {
            let _ = std::fs::remove_file(path);
        }
        _ => {
            // Not our file anymore (or already gone) — leave it alone.
        }
    }
}

fn rand_u32() -> u32 {
    // Use system time as a cheap entropy source.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos.wrapping_mul(2654435761).wrapping_add(0x12345678)
}

/// Process-wide WAL WRITER lease registry: WAL sidecar path -> the pager
/// instance id that owns the right to APPEND frames / checkpoint / reset
/// it. One process may hold MANY read pagers on the same sidecar (each
/// `Database::open` of a WAL-mode file attaches; committed frames are
/// plain positioned reads), but concurrent UNSERIALIZED appends from two
/// pagers would interleave at clashing offsets with independent running
/// checksums — silent sidecar corruption. The first appender takes the
/// lease; other pagers' append attempts fail with a clear busy-class
/// error instead of corrupting the log. The lease releases when the
/// owning pager retires (Drop/`retire()`), which also serializes against
/// open/teardown through [`lifecycle_guard`].
///
/// This is an in-process guard only (multi-PROCESS access to native
/// files remains outside the engine's contract; the SQLite-format
/// container is the cross-process interchange path).
pub(crate) fn wal_writer_leases(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, u64>> {
    static LEASES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>> =
        std::sync::OnceLock::new();
    LEASES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Canonicalize a sidecar path for lease keying: two handles opening
/// "db.sqlite" and "./db.sqlite" must map to the same lease entry.
///
/// The PARENT DIRECTORY is canonicalized, never the file itself: the
/// key must be STABLE across the sidecar's own creation and removal.
/// `Pager::drop` checkpoints and REMOVES the sidecar before its final
/// lease release, and the first append acquires before the sidecar
/// exists — a `canonicalize` of a missing file silently falls back to
/// the RAW path, which on macOS (`/var` -> `/private/var`) and Windows
/// (`\\?\` device prefix) differs from the canonical form used at
/// acquisition. The release then missed the acquired entry and the
/// lease LEAKED: every later handle on that path got SQLITE_BUSY
/// forever (CI mac+win, `wal_mode_persists_across_reopen` +
/// `concurrent_second_writer_handle_gets_busy_not_corruption`; Linux
/// was immune — `/tmp` is a real directory, raw == canonical). The
/// parent (the database's directory) survives both events, and
/// canonicalize(parent) + file_name equals canonicalize(file) for
/// every non-symlinked file name — sidecars are engine-created
/// regular files, never symlinks.
pub(crate) fn lease_key(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(p), Some(n)) if !p.as_os_str().is_empty() => std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .join(n),
        _ => path.to_path_buf(),
    }
}

/// Lease acquisition result: `Ours` = the caller already holds it,
/// `Acquired` = it was free and is now held, `Foreign` = another live
/// pager in this process holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WalLease {
    Ours,
    Acquired,
    Foreign(String),
}

/// Try to take (or confirm) the write lease for `owner` on `path`.
pub(crate) fn acquire_wal_lease(path: &Path, owner: u64) -> WalLease {
    let key = lease_key(path);
    let mut leases = wal_writer_leases().lock().unwrap();
    match leases.get(&key) {
        Some(&held) if held != owner => WalLease::Foreign(key.display().to_string()),
        Some(_) => WalLease::Ours,
        None => {
            leases.insert(key, owner);
            WalLease::Acquired
        }
    }
}

/// The busy-class error for a foreign lease, with the actionable hint.
pub(crate) fn wal_lease_busy(foreign: String) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "the WAL writer lease for {foreign} is held by another connection in this \
             process; concurrent multi-handle WAL writes are unsafe — route writes \
             through one connection (or the sqlx driver's shared engine)"
        ),
    )
}

/// Release the lease on `path` when `owner` holds it.
pub(crate) fn release_wal_lease(path: &Path, owner: u64) {
    let key = lease_key(path);
    let mut leases = wal_writer_leases().lock().unwrap();
    if leases.get(&key).copied() == Some(owner) {
        leases.remove(&key);
    }
}

/// The WAL file.
pub struct Wal {
    file: File,
    pub header: WalHeader,
    page_size: u32,
    /// Running checksum state.
    checksum: (u32, u32),
    /// Number of frames written since the last checkpoint.
    n_frames: u32,
    /// The on-disk header is not yet written: this handle was opened on
    /// a fresh/empty sidecar and DEFERRED the 32-byte header write (+ its
    /// fsync) to the first frame append. Read-only sessions of a
    /// WAL-mode file (a reopen after a clean close checkpoints+removes
    /// the sidecar, then auto-attaches) pay NEITHER the header fsync nor
    /// any later one — SQLite creates the -wal at first write, not at
    /// open. The first `append` writes the header before frame 0; a
    /// 0-byte sidecar at recovery is simply a fresh WAL (0 frames).
    header_dirty: bool,
}

impl Wal {
    /// The (dev, ino) identity of this handle's WAL file — used by the
    /// close-time sidecar removal to delete only a file this handle still
    /// owns (see [`lifecycle_guard`] / [`remove_file_if_owned`]).
    pub fn identity(&self) -> Option<(u64, u64)> {
        file_identity(&self.file)
    }

    /// Open or create the WAL file alongside the main database file.
    pub fn open<P: AsRef<Path>>(db_path: P, page_size: u32) -> Result<Self> {
        let path = wal_path_for(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // An existing WAL is scanned for valid (checksummed) frames
            // and appended after — never truncated away here.
            .truncate(false)
            .open(&path)?;

        let mut wal = Self {
            file,
            header: WalHeader::new(page_size),
            page_size,
            checksum: (0, 0),
            n_frames: 0,
            header_dirty: false,
        };

        wal.recover()?;
        Ok(wal)
    }

    /// Recover the WAL: read the header and validate frames.
    fn recover(&mut self) -> Result<()> {
        let file_size = self.file.metadata()?.len();
        if file_size == 0 {
            // Fresh WAL: header stays IN MEMORY (header_dirty) — the
            // 32-byte write + fsync are deferred to the first append.
            // Every read-only reopen of a WAL-mode file lands here;
            // paying a header fsync at open measured as the bulk of the
            // Windows S17 open+first-query gap.
            self.header = WalHeader::new(self.page_size);
            self.checksum = (self.header.checksum1, self.header.checksum2);
            self.n_frames = 0;
            self.header_dirty = true;
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
        let mut last_commit_frame: u32 = 0;
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
            if fh.commit != 0 {
                last_commit_frame = (i + 1) as u32;
            }
            last_valid_frame = (i + 1) as u32;
        }
        // Only frames up to the last COMMIT are durable/visible — a torn
        // transaction (frames appended but never commit-marked) is
        // discarded. This is the WAL's atomic-commit guarantee.
        self.n_frames = last_commit_frame;
        self.checksum = running_checksum;
        // Truncate torn trailing frames so future appends overwrite them.
        if last_commit_frame < last_valid_frame {
            let end = WAL_HEADER_SIZE as u64
                + last_commit_frame as u64 * (FRAME_HEADER_SIZE + self.page_size) as u64;
            self.file.set_len(end)?;
        }
        Ok(())
    }

    /// Build a page-id → frame-offset map of the LATEST committed version
    /// of each page in the WAL — the "WAL-served reads" index. Call after
    /// `open` (recovery already bounded frames to the last commit).
    pub fn committed_page_map(&mut self) -> Result<std::collections::HashMap<PageId, u64>> {
        let mut map = std::collections::HashMap::new();
        let frame_size = (FRAME_HEADER_SIZE + self.page_size) as u64;
        for i in 0..self.n_frames {
            let offset = WAL_HEADER_SIZE as u64 + i as u64 * frame_size;
            let mut fh_buf = [0u8; FRAME_HEADER_SIZE as usize];
            self.file.seek(SeekFrom::Start(offset))?;
            if self.file.read_exact(&mut fh_buf).is_err() {
                break;
            }
            let fh = FrameHeader::decode(&fh_buf)?;
            map.insert(fh.page_id, offset);
        }
        Ok(map)
    }

    /// Reset the WAL to empty. Called after a checkpoint.
    pub fn reset(&mut self) -> Result<()> {
        self.reset_synced(true)
    }

    /// Reset the WAL to a fresh header. `sync=false` skips the header
    /// fsync — `PRAGMA synchronous=OFF` checkpoints must not pay it
    /// (SQLite's WAL reset under OFF is a plain header rewrite; the
    /// durability point is the caller's to decide).
    pub fn reset_synced(&mut self, sync: bool) -> Result<()> {
        self.header = WalHeader::new(self.page_size);
        self.checksum = (self.header.checksum1, self.header.checksum2);
        self.n_frames = 0;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&self.header.encode())?;
        // The header is physically on disk now (an active-writer path:
        // checkpoint reset); nothing left to defer.
        self.header_dirty = false;
        if sync {
            self.file.sync_all()?;
        }
        Ok(())
    }

    /// fsync the WAL file (the durability point of a commit; callers decide
    /// based on `PRAGMA synchronous`).
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Read one frame's page data into `buf`. `offset` is a frame-header
    /// start previously returned by `append` or `committed_page_map`.
    pub fn read_frame_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() != self.page_size as usize {
            return Err(Error::InvalidArgument(
                "read_frame_at: wrong buffer size".to_string(),
            ));
        }
        let mut fh_buf = [0u8; FRAME_HEADER_SIZE as usize];
        read_exact_at(&self.file, &mut fh_buf, offset)?;
        let fh = FrameHeader::decode(&fh_buf)?;
        if fh.salt1 != self.header.salt1 || fh.salt2 != self.header.salt2 {
            return Err(Error::corruption("WAL frame salt mismatch"));
        }
        read_exact_at(&self.file, buf, offset + FRAME_HEADER_SIZE as u64)?;
        Ok(())
    }

    /// Read the FIRST `buf.len()` bytes of a frame's page body — a
    /// prefix read for recovery paths that only need the file header
    /// (first 100 bytes of page 0) and must not allocate a full page
    /// buffer (the autocommit-failure restore runs under a rigged
    /// allocator). Same salt validation as [`Self::read_frame_at`].
    pub fn read_frame_prefix_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() > self.page_size as usize {
            return Err(Error::InvalidArgument(
                "read_frame_prefix_at: buffer larger than a page".to_string(),
            ));
        }
        let mut fh_buf = [0u8; FRAME_HEADER_SIZE as usize];
        read_exact_at(&self.file, &mut fh_buf, offset)?;
        let fh = FrameHeader::decode(&fh_buf)?;
        if fh.salt1 != self.header.salt1 || fh.salt2 != self.header.salt2 {
            return Err(Error::corruption("WAL frame salt mismatch"));
        }
        read_exact_at(&self.file, buf, offset + FRAME_HEADER_SIZE as u64)?;
        Ok(())
    }

    /// Current salts (diagnostics).
    pub fn salts(&self) -> (u32, u32) {
        (self.header.salt1, self.header.salt2)
    }

    /// Append a frame to the WAL. Returns the byte offset of the frame
    /// header so the caller can record it in its page map. `commit` marks
    /// the frame as ending a transaction; syncing is the caller's decision.
    pub fn append(&mut self, page_id: PageId, data: &[u8], commit: bool) -> Result<u64> {
        if data.len() != self.page_size as usize {
            return Err(Error::InvalidArgument(format!(
                "WAL append: data length {} != page_size {}",
                data.len(),
                self.page_size
            )));
        }
        // Deferred fresh-WAL header: the first append materializes it
        // (bytes 0..32 must precede frame 0 for recovery's checksum
        // chain). No fsync here — the caller's commit-boundary sync
        // covers header + frames together; a crash before it leaves a
        // header with zero COMMITTED frames, which recovery discards.
        if self.header_dirty {
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&self.header.encode())?;
            self.header_dirty = false;
        }
        let offset = WAL_HEADER_SIZE as u64
            + self.n_frames as u64 * (FRAME_HEADER_SIZE + self.page_size) as u64;
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
        self.n_frames += 1;
        Ok(offset)
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
        })
        .unwrap();
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
