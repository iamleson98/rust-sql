//! Per-file page-space coordinator for the SQLite-format container's
//! INCREMENTAL commits (the "page-diff architecture").
//!
//! Historically every commit boundary re-derived the WHOLE database
//! image: the full object walk materialized every table's rows, the
//! bottom-up builder re-encoded every b-tree, and a byte-wise diff
//! against the last image extracted the changed pages — O(database)
//! CPU and memory per commit, even though only O(changed pages) ever
//! reached the disk.
//!
//! This module replaces that with a persistent, SHARED page space:
//!
//! * A full publish (first commit, DDL, journal-mode switch,
//!   auto-vacuum files, or a fallback) writes the complete image and
//!   records every object's PAGE SPAN — root plus the exact pages its
//!   b-tree occupies.
//! * Every later commit splices only the objects whose b-trees were
//!   written (the engine's per-root change epochs say which — see
//!   `Pager::root_epochs`): the object's tree is rebuilt against a
//!   recycle queue of its own previous pages, then the container
//!   freelist, then fresh tail pages. Untouched objects keep their
//!   pages byte-for-byte; the commit set is KNOWN (no image diff), and
//!   CPU scales with the changed object, never with the database.
//! * Page shrinkage routes into a REAL SQLite freelist (fileformat2
//!   trunk/leaf pages, header fields 32/36) so page identities stay
//!   stable without re-flowing the file — the property SQLite's own
//!   incremental updates rely on, and what makes our WAL frames and
//!   rollback-journal pre-images correct.
//! * MULTI-SESSION: one coordinator per file (process-wide registry,
//!   keyed by canonical path) serializes every session's commits over
//!   the SAME evolving page space. Sessions splice their own changed
//!   objects; another session's untouched objects survive verbatim —
//!   a per-object last-writer-wins merge instead of the old
//!   whole-image clobber. The WAL sidecar is likewise shared: one
//!   checksum chain, commit-granular interleaving, recovery exactly
//!   like a single writer's log.
//! * Memory: instead of holding a full image copy per session, the
//!   coordinator keeps only pages newer than the main file (the WAL
//!   overlay, bounded by the 1000-frame autocheckpoint) and reads
//!   older pages straight from the file — O(changed) per commit.
//!
//! Crash semantics per mode are the existing engine protocols,
//! unchanged: WAL frames (checksum-chained, commit-marked) or the
//! rollback journal (pre-images, in-place writes, journal deletion as
//! the commit point) — now fed with the splice's known page set
//! instead of an image diff.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use super::header::build_header_enc;
use super::mutator::{self, MutateOps, MutateOutcome};
use super::reader::wal_path_of;
use super::record::TextEnc;
use super::wal::{append_wal_open, checkpoint, WalWriter};
use super::writer::{
    build_freelist, splice_object, splice_schema_tree, write_image_atomic, BuiltImage, OutObject,
    SchemaRowCell, SpanKey, SpanRec, SpliceBuild,
};
use std::sync::atomic::Ordering::Relaxed;

/// Session-provided header inputs for one publish (full or splice).
#[derive(Clone, Debug)]
pub struct PublishParams {
    pub page_size: u32,
    pub journal_wal: bool,
    pub text_enc: TextEnc,
    pub auto_vacuum: u8,
    pub user_version: u32,
    pub application_id: u32,
    /// The counters the image/commit will carry (coordinator-issued:
    /// see [`Container::next_counters`]).
    pub change_counter: u32,
    pub schema_cookie: u32,
    /// `PRAGMA synchronous` (0=OFF, 1=NORMAL, 2=FULL/EXTRA). WAL
    /// commits fsync only under FULL — SQLite's own discipline; the
    /// checkpoint syncs under everything but OFF.
    pub synchronous: u8,
}

/// One object queued for a splice publish: the span it replaces and
/// its freshly collected content.
pub struct SpliceItem {
    pub key: SpanKey,
    pub object: OutObject,
}

/// One object queued for PAGE-LEVEL mutation: the deltas apply
/// directly onto the object's existing foreign-format pages (see
/// `mutator`), touching only the changed pages — the next step past
/// the whole-object splice. Offered only when the session's recorded
/// span version matches the coordinator's (no other session moved the
/// object since this session's last publish); any verification
/// mismatch fails the item back to the whole-object splice.
pub struct MutateItem {
    pub key: SpanKey,
    pub ops: MutateOps,
}

/// A splice publish's failure modes.
pub enum SpliceError {
    /// Page-level mutation declined for these objects (stale delta
    /// journal, verification mismatch, unreadable base page). NOTHING
    /// was committed: the caller re-collects those objects as
    /// whole-tree `SpliceItem`s and calls again.
    MutateFallback(Vec<SpanKey>),
    /// Any other failure — the caller's existing recovery applies (the
    /// full publish resets the coordinator state).
    Other(String),
}

impl From<String> for SpliceError {
    fn from(e: String) -> Self {
        SpliceError::Other(e)
    }
}

/// One sqlite_schema row for the splice path's schema-tree rebuild —
/// the SAME descriptor list the full collector walks (see
/// `Database::sqlite_schema_row_descs`); rootpages are resolved from
/// the coordinator's spans at publish time, so the tree reflects the
/// just-spliced layout.
pub struct SpliceSchemaRow {
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    pub sql: Option<String>,
    /// The lowercased span key for rooted rows (tables and indexes);
    /// `None` for views / triggers / virtual tables (rootpage 0).
    pub key: Option<String>,
}

/// The coordinator's page-space state — everything a commit needs to
/// splice onto the CURRENT layout. Guarded by the container mutex.
#[derive(Debug, Default)]
pub struct ContainerState {
    /// Page size (fixed once established).
    pub page_size: u32,
    /// Pages in the database file after the last commit.
    pub n_pages: u32,
    /// Journal mode of the on-disk state (true = WAL sidecar commits).
    pub journal_wal: bool,
    pub text_enc: TextEnc,
    pub auto_vacuum: u8,
    pub user_version: u32,
    pub application_id: u32,
    pub change_counter: u32,
    pub schema_cookie: u32,
    /// Object name → root + pages (the page space).
    pub spans: HashMap<SpanKey, SpanRec>,
    /// Per-span layout version — a UNIQUE number stamped on every span
    /// install (adopt, full publish, splice, mutation). Sessions
    /// record the versions their last publish saw; a mismatch means
    /// another publish replaced the layout in between, and page-level
    /// mutation is declined (the whole-object splice's
    /// last-writer-wins semantics apply instead).
    pub span_versions: HashMap<SpanKey, u64>,
    /// The version generator (monotonic per coordinator).
    next_ver: u64,
    /// Free pages available for reuse (ascending by construction).
    pub free: BTreeSet<u32>,
    /// Pages newer than the main file: WAL frames committed since the
    /// last checkpoint. Bounded by the autocheckpoint threshold.
    pub overlay: HashMap<u32, Vec<u8>>,
    /// The committed view of MAIN-FILE pages (the descent/verify read
    /// cache). A page lives here when the main file is its authoritative
    /// home; WAL commits move it to the overlay, and a checkpoint folds
    /// the overlay back in (the file then holds the same bytes). Softly
    /// bounded — cleared wholesale past the cap.
    pub page_cache: HashMap<u32, Vec<u8>>,
    /// Last-seen `PRAGMA synchronous` (the close-time checkpoint's sync
    /// discipline; set by every splice publish).
    pub synchronous: u8,
    /// The live WAL sidecar writer (WAL mode only).
    pub wal: Option<WalWriter>,
    /// Commit epoch: bumped by every publish; sessions use it as a
    /// cheap "the page space moved" signal for diagnostics.
    pub epoch: u64,
    /// Whether a full publish has established the page space.
    pub established: bool,
    /// Force the next publish through the FULL path (set by
    /// `invalidate`: a re-adopt of the current file must not resurrect
    /// the layout the invalidation meant to discard — e.g. the
    /// close-time Republish of a session whose sidecar was lost).
    pub force_full: bool,
    /// (file length, header change counter, header db size) as the
    /// coordinator last left the MAIN file — the external-touch guard
    /// (any mismatch → the next commit re-establishes with a full
    /// publish instead of splicing onto a foreign layout).
    guard: (u64, u32, u32),
    /// Live sessions (attach count); the last detach checkpoints.
    attached: usize,
    /// Cached open READ handle on the main file — the guard probe's
    /// fast path (one path stat + one positioned 100-byte read instead
    /// of open/stat/read/close per commit). Dropped whenever a protocol
    /// step rewrites the main file (its identity goes stale by
    /// construction).
    main_io: Option<FileIo>,
    /// Cached open APPEND handle on the WAL sidecar — the per-commit
    /// fast path (one path stat + one positioned write, SQLite's own
    /// discipline: the sidecar descriptor stays open for the session's
    /// life). Dropped after checkpoints, full publishes, adopts and
    /// journal-mode switches — everything that rewrites or retires the
    /// sidecar.
    wal_io: Option<FileIo>,
    /// Freelist-skip bookkeeping: the free-set generation the cached
    /// freelist structure (`free_cache.head`, `free_cache.count`,
    /// `free_cache.pages`) was built from. A commit whose mutations
    /// never moved a page in or out of `free` (the overwhelmingly
    /// common data-only shape) reuses the cached structure instead of
    /// rebuilding every freelist trunk page and byte-comparing it
    /// against the committed view — O(1) instead of O(freelist
    /// pages) on the per-commit path.
    free_version: u64,
    /// The free-set generation as of the LAST freelist build.
    free_built_version: u64,
    /// The last built freelist structure (page bytes + head/count for
    /// the header), valid while `free_built_version == free_version`.
    free_cache: BTreeMap<u32, Vec<u8>>,
    /// (head, count) of the cached freelist structure — the header's
    /// fields come from here.
    free_cache_head: (u32, u32),
    /// Reused WAL frame-encoding buffer (one Vec's capacity for the
    /// coordinator's life instead of a ~12 KiB alloc+free per
    /// autocommit). `encode_commit_into` fills it; the publish path
    /// takes it and hands it back after the sidecar append. Capacity
    /// retention is best-effort — error paths may drop it.
    commit_scratch: Vec<u8>,
}

/// An open file handle plus the identity (length, and on unix the
/// inode) it was validated against — the cheap per-commit
/// revalidation compares a fresh `stat(path)` against these; any
/// mismatch (replacement, truncation, growth) demotes to the fully
/// verified slow path.
#[derive(Debug)]
struct FileIo {
    file: std::fs::File,
    len: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIo {
    fn of(meta: &std::fs::Metadata, file: std::fs::File) -> Self {
        FileIo {
            file,
            len: meta.len(),
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                meta.ino()
            },
        }
    }

    /// Whether a path stat's metadata still describes the file this
    /// handle was validated against. Unix compares the inode (a
    /// temp+rename replacement shows a different one even at equal
    /// length); non-unix compares length only.
    fn matches(&self, meta: &std::fs::Metadata) -> bool {
        if self.len != meta.len() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.ino == meta.ino()
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

/// Positioned read that does not move a shared cursor (pread on unix,
/// seek_read on windows) — the cached handles are shared by every
/// session of the container, so cursor moves would race.
fn pread_exact(f: &mut std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                f.read_at(&mut buf[done..], off + done as u64)?
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                f.seek_read(&mut buf[done..], off + done as u64)?
            }
            #[cfg(not(any(unix, windows)))]
            {
                use std::io::{Read, Seek, SeekFrom};
                f.seek(SeekFrom::Start(off + done as u64))?;
                f.read(&mut buf[done..])?
            }
        };
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "positioned read hit EOF",
            ));
        }
        done += n;
    }
    Ok(())
}

/// Positioned write (pwrite / seek_write) — see [`pread_exact`].
fn pwrite_all(f: &mut std::fs::File, buf: &[u8], off: u64) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                f.write_at(&buf[done..], off + done as u64)?
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                f.seek_write(&buf[done..], off + done as u64)?
            }
            #[cfg(not(any(unix, windows)))]
            {
                use std::io::{Seek, SeekFrom, Write};
                f.seek(SeekFrom::Start(off + done as u64))?;
                f.write(&buf[done..])?
            }
        };
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "positioned write wrote nothing",
            ));
        }
        done += n;
    }
    Ok(())
}

/// Page reads over the committed view: the WAL overlay first, then
/// the main file (lazily opened once per commit). `None` = the page
/// is not part of the committed view (beyond the file AND not in the
/// overlay). The overlay is passed PER CALL (never borrowed for the
/// reader's lifetime) so the publish flow can mutate the coordinator
/// state between reads.
struct PageReader<'a> {
    path: &'a Path,
    file: Option<std::fs::File>,
}

impl<'a> PageReader<'a> {
    fn new(path: &'a Path) -> Self {
        PageReader { path, file: None }
    }

    fn read(
        &mut self,
        pgno: u32,
        page_size: u32,
        overlay: &HashMap<u32, Vec<u8>>,
        cache: &mut HashMap<u32, Vec<u8>>,
    ) -> Option<Vec<u8>> {
        if let Some(p) = overlay.get(&pgno) {
            READER_OVERLAY_HITS.fetch_add(1, Relaxed);
            return Some(p.clone());
        }
        if let Some(p) = cache.get(&pgno) {
            READER_CACHE_HITS.fetch_add(1, Relaxed);
            return Some(p.clone());
        }
        let mut buf = vec![0u8; page_size as usize];
        if !self.fill_from_file(pgno, page_size, &mut buf) {
            return None;
        }
        // A main-file page enters the committed-view cache: the next
        // descent over this page is a RAM hit (the cache is the
        // coordinator's, and commits are serialized through its mutex).
        if cache.len() >= PAGE_CACHE_CAP {
            cache.clear();
        }
        cache.insert(pgno, buf.clone());
        Some(buf)
    }

    /// The borrow-returning twin of [`Self::read`]: same lookup order
    /// (overlay → cache → file), but a hit is handed back as a REFERENCE
    /// — the compare-then-maybe-insert publish sites used to clone a
    /// 4 KiB page (heap alloc + memcpy) on every hit just to byte-
    /// compare it and drop it. A file miss reads the page, admits it
    /// into the committed-view cache ONCE, and returns a reference into
    /// the cache (the old path made a second copy to return).
    ///
    /// The returned reference is tied ONLY to `overlay` and `cache`
    /// (not `self`): callers must drop it before mutating either map,
    /// but the reader itself is free as soon as the call returns.
    fn read_ref<'o>(
        &mut self,
        pgno: u32,
        page_size: u32,
        overlay: &'o HashMap<u32, Vec<u8>>,
        cache: &'o mut HashMap<u32, Vec<u8>>,
    ) -> Option<&'o Vec<u8>> {
        if let Some(p) = overlay.get(&pgno) {
            READER_OVERLAY_HITS.fetch_add(1, Relaxed);
            return Some(p);
        }
        if cache.contains_key(&pgno) {
            READER_CACHE_HITS.fetch_add(1, Relaxed);
            return cache.get(&pgno);
        }
        let mut buf = vec![0u8; page_size as usize];
        if !self.fill_from_file(pgno, page_size, &mut buf) {
            return None;
        }
        // A main-file page enters the committed-view cache (see
        // [`Self::read`]); the reference handed back points into it.
        if cache.len() >= PAGE_CACHE_CAP {
            cache.clear();
        }
        cache.insert(pgno, buf);
        cache.get(&pgno)
    }

    /// The file-read tail shared by [`Self::read`] and [`Self::read_ref`]:
    /// lazily open the main file, seek to the page, read it into `buf`.
    /// False = the read failed (the page is not part of the committed
    /// view, or an I/O error struck).
    fn fill_from_file(&mut self, pgno: u32, page_size: u32, buf: &mut Vec<u8>) -> bool {
        use std::io::{Read, Seek};
        if self.file.is_none() {
            READER_FILE_OPENS.fetch_add(1, Relaxed);
            match std::fs::File::open(self.path) {
                Ok(f) => self.file = Some(f),
                Err(_) => return false,
            }
        }
        READER_FILE_READS.fetch_add(1, Relaxed);
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        let off = (pgno as u64 - 1) * page_size as u64;
        if file.seek(std::io::SeekFrom::Start(off)).is_err() {
            return false;
        }
        buf.clear();
        buf.resize(page_size as usize, 0);
        file.read_exact(buf).is_ok()
    }
}

/// Soft cap for the committed-view page cache (8192 pages = 32 MiB at
/// the default 4 KiB page). Past it the cache clears wholesale — one
/// commit pays a few descent reads again, never an ongoing cost.
const PAGE_CACHE_CAP: usize = 8192;

/// Whether a new 100-byte file header differs from the committed one in
/// any field EXCEPT the ones SQLite leaves alone on WAL commits: the
/// change counter (24..28), the in-header database size (28..32, only
/// refreshed when the file grows) and version-valid-for (92..96, always
/// a copy of the counter). Everything else — freelist head/count,
/// schema cookie, encoding, user_version, application_id, reserved
/// bytes, the sqlite version — is real page-1 content.
fn header_diff_beyond_counter_fields(new: &[u8; 100], old: &[u8]) -> bool {
    const RANGES: [(usize, usize); 3] = [(0, 24), (32, 92), (96, 100)];
    RANGES.iter().any(|&(a, b)| new[a..b] != old[a..b])
}

/// Post-checkpoint page-ownership move: the main file now holds every
/// overlay byte, so the overlay's pages fold into the committed-view
/// cache (hot for the next descent) and the overlay empties. The soft
/// cap keeps a long session's cache bounded.
fn fold_overlay_into_cache(st: &mut ContainerState, cache: &mut HashMap<u32, Vec<u8>>) {
    if cache.len() + st.overlay.len() > PAGE_CACHE_CAP {
        cache.clear();
    }
    for (p, b) in st.overlay.drain() {
        cache.insert(p, b);
    }
}

/// Lexical path normalization for the registry key: canonicalize when
/// the file exists; otherwise absolute-ize against the CWD and clean
/// `.`/`..` components (no symlink resolution — both attach sites in
/// one process resolve identically).
fn normalize_key(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<Container>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Weak<Container>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One SQLite-format file's coordinator.
pub struct Container {
    path: PathBuf,
    pub state: Mutex<ContainerState>,
}

/// Attach (or create) the coordinator of one database file. Every
/// SQLite-format `Database` holds one `Arc<Container>` for its life;
/// commits, journal-mode switches and the close-time checkpoint all
/// serialize through it.
pub fn attach(path: &Path) -> Arc<Container> {
    let key = normalize_key(path);
    let mut reg = registry().lock();
    if let Some(c) = reg.get(&key).and_then(Weak::upgrade) {
        return c;
    }
    let c = Arc::new(Container {
        path: path.to_path_buf(),
        state: Mutex::new(ContainerState::default()),
    });
    reg.insert(key, Arc::downgrade(&c));
    c
}

/// What the last-session teardown decided (see
/// `Container::session_detach`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachOutcome {
    /// Other sessions still hold the file — nothing folded.
    NotLast,
    /// The sidecar was folded (or there was nothing to fold).
    Folded,
    /// The session had WAL frames pending, but the sidecar was gone or
    /// damaged — the caller should RE-PUBLISH its committed state from
    /// memory (the writer's knowledge is authoritative for what it
    /// committed).
    Republish,
}

impl Container {
    /// Whether the shared WAL writer currently holds un-checkpointed
    /// committed frames (any session's).
    pub fn wal_has_frames(&self) -> bool {
        self.state
            .lock()
            .wal
            .as_ref()
            .is_some_and(|w| w.has_frames())
    }
    /// The coordinator's next header counters (peek only; the publish
    /// adopts them). Monotonic per file across sessions — SQLite's
    /// change-counter/cookie semantics, previously frozen at 1.
    pub fn next_counters(&self, ddl: bool) -> (u32, u32) {
        let st = self.state.lock();
        let counter = st.change_counter.wrapping_add(1).max(1);
        let cookie = if ddl {
            st.schema_cookie.wrapping_add(1).max(1)
        } else {
            st.schema_cookie.max(1)
        };
        (counter, cookie)
    }

    /// Whether the coordinator holds an established page space the
    /// session can splice onto (mode/encoding/size must also match —
    /// checked in `can_splice`).
    pub fn established(&self) -> bool {
        self.state.lock().established
    }

    /// Whether the coordinator currently holds a span for one object
    /// (the synthesized-sqlite_sequence appearance check on the splice
    /// path).
    pub fn has_span(&self, key: &SpanKey) -> bool {
        self.state.lock().spans.contains_key(key)
    }

    /// Splice preconditions: an established page space with the same
    /// geometry the session expects, and no sign the main file was
    /// touched by anyone but this coordinator since the last commit.
    /// ADOPT an existing file's layout as the coordinator's page space:
    /// the file IS the page space — the coordinator is a cache. After a
    /// process restart (or the last session's close), a fresh session
    /// re-derives every object's span, the freelist and the header
    /// fields straight from the file, so its FIRST commit splices
    /// instead of paying a whole-image re-publish.
    ///
    /// Declines (→ the caller takes the full publish) for anything the
    /// splice architecture does not model: auto-vacuum pointer maps, a
    /// live WAL sidecar (the session folds it via `had_wal`), a hot
    /// journal, header/file-length disagreement, encoding or page-size
    /// mismatch with the session, or a page accounting that does not
    /// cover the file exactly (a foreign shape we do not understand).
    pub fn try_adopt(&self, params: &PublishParams) -> bool {
        let mut st = self.state.lock();
        if st.established {
            return true;
        }
        if st.force_full {
            trace_decline(&st, params, "forced full publish (invalidate)");
            return false;
        }
        if let Err(why) = self.adopt_locked(&mut st, params) {
            if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                eprintln!("[splice] adopt declined: {why}");
            }
            return false;
        }
        st.established = true;
        st.epoch += 1;
        true
    }

    /// [`try_adopt`]'s worker over a locked state.
    fn adopt_locked(&self, st: &mut ContainerState, params: &PublishParams) -> Result<(), String> {
        use std::io::Read;
        let mut file = std::fs::File::open(&self.path)
            .map_err(|e| format!("open {}: {e}", self.path.display()))?;
        let mut hdr = [0u8; 100];
        file.read_exact(&mut hdr)
            .map_err(|e| format!("header: {e}"))?;
        if !super::header::has_magic(&hdr) {
            return Err("not a SQLite file".into());
        }
        let be32 = |o: usize| u32::from_be_bytes(hdr[o..o + 4].try_into().unwrap());
        let mut ps = be16_of(&hdr, 16);
        if ps == 1 {
            ps = 65536;
        }
        if !ps.is_power_of_two() || !(512..=65536).contains(&ps) {
            return Err(format!("bad page size {ps}"));
        }
        if params.page_size != 0 && params.page_size != ps {
            return Err(format!(
                "session page size {} != file {ps}",
                params.page_size
            ));
        }
        if hdr[18] == 2 && hdr[19] == 2 {
            // WAL-mode file: only adoptable with NO live sidecar (a
            // sidecar means un-checkpointed frames — the session's
            // `had_wal` path handles that with a full publish).
            if super::reader::wal_path_of(&self.path).exists() {
                return Err("live WAL sidecar".into());
            }
        }
        if super::rj::journal_path_of(&self.path).exists() {
            return Err("rollback journal present".into());
        }
        let db_size = be32(28);
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_len == 0 || db_size == 0 || file_len != db_size as u64 * ps as u64 {
            return Err(format!("header size {db_size} != file length {file_len}"));
        }
        if be32(52) != 0 {
            return Err("auto-vacuum pointer maps".into());
        }
        let enc = TextEnc::from_u32(be32(56)).map_err(|e| format!("encoding: {e}"))?;
        if enc != params.text_enc {
            return Err("text encoding mismatch".into());
        }

        // ---- Schema tree: rows + the tree's own pages ----
        let mut buf: Vec<u8> = Vec::new();
        let mut schema_pages: BTreeSet<u32> = BTreeSet::new();
        let schema_rows = parse_schema_rows(&mut file, ps, enc, &mut schema_pages, &mut buf)?;
        schema_pages.insert(1);

        // ---- Object spans ----
        let mut spans: HashMap<SpanKey, SpanRec> = HashMap::new();
        let mut accounted: BTreeSet<u32> = schema_pages.clone();
        for row in &schema_rows {
            if row.rootpage == 0 {
                continue;
            }
            let mut pages: BTreeSet<u32> = BTreeSet::new();
            collect_tree_pages(&mut file, ps, row.rootpage, 0, &mut pages, &mut buf)?;
            accounted.extend(pages.iter().copied());
            spans.insert(
                SpanKey::Object(row.name.to_ascii_lowercase()),
                SpanRec {
                    root: row.rootpage,
                    pages: pages.into_iter().collect(),
                },
            );
        }
        spans.insert(
            SpanKey::SchemaTree,
            SpanRec {
                root: 1,
                pages: schema_pages.into_iter().collect(),
            },
        );

        // ---- Freelist (trunk chain + leaves) ----
        let mut free: BTreeSet<u32> = BTreeSet::new();
        let fl_head = be32(32);
        let fl_count = be32(36);
        let mut trunk = fl_head;
        let mut guard = 0u32;
        while trunk != 0 {
            guard += 1;
            if guard > 100_000 {
                return Err("freelist trunk chain loop".into());
            }
            if !read_file_page(&mut file, ps, trunk, &mut buf) {
                return Err(format!("freelist trunk {trunk} unreadable"));
            }
            let next = u32::from_be_bytes(buf[0..4].try_into().unwrap());
            let k = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
            if 8 + k * 4 > buf.len() {
                return Err("freelist trunk overfull".into());
            }
            free.insert(trunk);
            for i in 0..k {
                let leaf = u32::from_be_bytes(buf[8 + i * 4..12 + i * 4].try_into().unwrap());
                if leaf == 0 {
                    return Err("freelist leaf 0".into());
                }
                free.insert(leaf);
            }
            trunk = next;
        }
        if free.len() as u32 != fl_count {
            return Err(format!(
                "freelist count {} != walked {}",
                fl_count,
                free.len()
            ));
        }
        accounted.extend(free.iter().copied());

        // ---- Every page accounted: {1..=db_size} exactly ----
        if accounted.len() != db_size as usize {
            return Err(format!(
                "page accounting: {} of {db_size} pages referenced",
                accounted.len()
            ));
        }
        if accounted.iter().any(|p| *p == 0 || *p > db_size) {
            return Err("page reference out of range".into());
        }

        // ---- Install the adopted page space ----
        st.page_size = ps;
        st.n_pages = db_size;
        st.journal_wal = hdr[18] == 2 && hdr[19] == 2;
        st.text_enc = enc;
        st.auto_vacuum = 0;
        st.user_version = be32(60);
        st.application_id = be32(68);
        st.change_counter = be32(24).max(1);
        st.schema_cookie = be32(40).max(1);
        st.spans = spans;
        let mut next = st.next_ver;
        st.span_versions = st
            .spans
            .keys()
            .map(|k| {
                next += 1;
                (k.clone(), next)
            })
            .collect();
        st.next_ver = next;
        st.free = free;
        // The adopted file's freelist STRUCTURE was written by whoever
        // last committed (possibly real SQLite — a different trunk
        // selection than ours); only the free SET walked above is
        // truth. Reset the freelist cache so the first splice builds
        // and byte-verifies our structure against the file.
        st.free_version = 0;
        st.free_built_version = u64::MAX;
        st.free_cache.clear();
        st.free_cache_head = (0, 0);
        st.overlay.clear();
        st.page_cache.clear();
        st.wal = if st.journal_wal {
            Some(WalWriter::new(ps, 0))
        } else {
            None
        };
        st.wal_io = None;
        st.record_guard(&self.path);
        Ok(())
    }

    /// Splice preconditions: an established page space with the same
    /// geometry the session expects, and no sign the main file was
    /// touched by anyone but this coordinator since the last commit.
    pub fn can_splice(&self, params: &PublishParams) -> bool {
        let t0 = std::time::Instant::now();
        let r = self.can_splice_inner(params);
        CAN_SPLICE_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        r
    }

    fn can_splice_inner(&self, params: &PublishParams) -> bool {
        let mut st = self.state.lock();
        if st.force_full {
            trace_decline(&st, params, "forced full publish");
            return false;
        }
        if !st.established || st.page_size == 0 {
            trace_decline(&st, params, "not established");
            return false;
        }
        if st.page_size != params.page_size {
            trace_decline(&st, params, "page size mismatch");
            return false;
        }
        // Auto-vacuum geometry (pointer maps) is a full-build concern.
        if st.auto_vacuum != 0 || params.auto_vacuum != 0 {
            trace_decline(&st, params, "auto-vacuum geometry");
            return false;
        }
        if st.journal_wal != params.journal_wal {
            trace_decline(&st, params, "journal mode switch");
            return false;
        }
        if st.text_enc != params.text_enc {
            trace_decline(&st, params, "text encoding switch");
            return false;
        }
        // External-touch guard: the main file must be exactly as the
        // coordinator last left it (length + header counter + size).
        // Fast path: one path stat (a replaced file shows a different
        // identity, a truncated/grown one a different length) + one
        // positioned 100-byte read through the cached handle.
        let Some((len, counter, size)) = main_guard_probe(&mut st, &self.path) else {
            trace_decline(&st, params, "header unreadable");
            return false;
        };
        if len != st.guard.0 {
            trace_decline(&st, params, "file length moved");
            return false;
        }
        if counter != st.guard.1 || size != st.guard.2 {
            trace_decline(&st, params, "header counter/size moved");
            return false;
        }
        true
    }

    /// Full publish: write the complete image atomically (the existing
    /// durability protocol — temp + fsync + rename, sidecar retirement)
    /// and install the page space the next splices build on.
    pub fn publish_full(&self, built: &BuiltImage, params: &PublishParams) -> Result<(), String> {
        write_image_atomic(&self.path, &built.bytes)?;
        let mut st = self.state.lock();
        st.page_size = built.page_size;
        st.n_pages = built.n_pages;
        st.journal_wal = params.journal_wal;
        st.text_enc = params.text_enc;
        st.auto_vacuum = params.auto_vacuum;
        st.user_version = params.user_version;
        st.application_id = params.application_id;
        st.change_counter = params.change_counter;
        st.schema_cookie = params.schema_cookie;
        st.spans = built.spans.clone();
        let mut next = st.next_ver;
        st.span_versions = st
            .spans
            .keys()
            .map(|k| {
                next += 1;
                (k.clone(), next)
            })
            .collect();
        st.next_ver = next;
        st.free.clear();
        // A full publish rewrote the file wholesale: the freelist
        // cache describes a previous page space. Reset it.
        st.free_version = 0;
        st.free_built_version = u64::MAX;
        st.free_cache.clear();
        st.free_cache_head = (0, 0);
        st.overlay.clear();
        st.page_cache.clear();
        st.force_full = false;
        st.wal = if params.journal_wal {
            Some(WalWriter::new(built.page_size, 0))
        } else {
            None
        };
        st.wal_io = None;
        st.epoch += 1;
        st.established = true;
        st.record_guard(&self.path);
        Ok(())
    }

    /// Drop the page space (the next commit re-establishes with a full
    /// publish). Called when the file is rewritten OUTSIDE the
    /// coordinator's protocols — VACUUM's compact rebuild, or a
    /// detected external touch.
    pub fn invalidate(&self) {
        let mut st = self.state.lock();
        st.established = false;
        st.force_full = true;
        st.spans.clear();
        st.free.clear();
        // The page space is gone — so is the freelist cache's
        // meaning. Reset it.
        st.free_version = 0;
        st.free_built_version = u64::MAX;
        st.free_cache.clear();
        st.free_cache_head = (0, 0);
        st.overlay.clear();
        st.page_cache.clear();
        st.wal = None;
        st.wal_io = None;
        st.epoch += 1;
    }

    /// Incremental publish — splice `items` into the page space, free
    /// the spans of `dropped` objects, and commit the KNOWN changed-page
    /// set through the journal-mode protocol. `schema_changed` (any
    /// DDL difference since the last publish) or a moved rootpage
    /// rebuilds the sqlite_schema tree in place. Returns `Ok(false)`
    /// when nothing changed (a byte-identical rebuild: no frames, no
    /// counter bump — SQLite's own no-op-transaction shape).
    pub fn publish_splice(
        &self,
        items: &[SpliceItem],
        mutates: &[MutateItem],
        dropped: &[SpanKey],
        schema_changed: bool,
        schema_rows_of: &dyn Fn() -> Vec<SpliceSchemaRow>,
        params: &PublishParams,
    ) -> Result<bool, SpliceError> {
        let t0 = std::time::Instant::now();
        let r = self.publish_splice_inner(
            items,
            mutates,
            dropped,
            schema_changed,
            schema_rows_of,
            params,
        );
        PUBLISH_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        PUBLISH_COUNT.fetch_add(1, Relaxed);
        r
    }

    fn publish_splice_inner(
        &self,
        items: &[SpliceItem],
        mutates: &[MutateItem],
        dropped: &[SpanKey],
        schema_changed: bool,
        schema_rows_of: &dyn Fn() -> Vec<SpliceSchemaRow>,
        params: &PublishParams,
    ) -> Result<bool, SpliceError> {
        let mut st = self.state.lock();
        let ps = st.page_size;
        if ps == 0 {
            return Err(SpliceError::Other(
                "splice without an established page space".into(),
            ));
        }
        let n_old = st.n_pages;
        // SQLite's WAL frame discipline (measured against the real
        // engine — probe_sqlite_wal_shape): a WAL commit NEVER bumps
        // the change counter, and page 1 rides a commit when its
        // content changed beyond the counter/size fields OR the file
        // GREW (real SQLite's bulk-growth txn opens with a page-1
        // frame — the in-header size must move with the file; the
        // non-growing data-only commits are the ones that skip page 1
        // entirely). Rollback commits keep the classic whole-counter
        // rewrite.
        let wal_mode = st.journal_wal;
        let committed_pages = n_old;
        let counter_eff = if wal_mode {
            st.change_counter.max(1)
        } else {
            params.change_counter
        };
        st.synchronous = params.synchronous;
        let mut reader = PageReader::new(&self.path);
        let mut changed: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut roots_moved = false;
        // The committed-view page cache, taken out for the body:
        // `run_mutation` holds `&st` while the cache mutates, and the
        // commit protocol moves page ownership between cache/overlay.
        // Lost on error paths (the fallbacks re-read a few pages).
        let mut cache = std::mem::take(&mut st.page_cache);

        // ---- 1a. Dropped objects: their pages join the freelist ----
        if !dropped.is_empty() {
            st.free_version += 1;
        }
        for key in dropped {
            if let Some(s) = st.spans.remove(key) {
                for p in s.pages {
                    st.free.insert(p);
                }
            }
        }

        // ---- 1b. PAGE-LEVEL MUTATIONS (all-or-nothing scratch) ----
        // Every mutation runs against the committed view FIRST and
        // only its OUTCOME is applied — a failure returns before any
        // coordinator state moves, so the caller can re-offer the
        // failed keys as whole-object splices.
        //
        // The publish's mutation sessions share ONE allocation view
        // (`mut_free` / `mut_tail` / `mut_pending`): each session's
        // fresh/freelist page grants retire from the view before the
        // NEXT session runs — two same-commit objects that both split
        // (a table and its index) must never be handed the same page,
        // and a page one session pruned may be reused by the next.
        // The free-set clone is paid only when a mutation actually
        // runs (the empty-mutates commit skips the O(freelist) copy).
        let mut mutate_outcomes: Vec<(&MutateItem, MutateOutcome)> = Vec::new();
        let mut mut_free: BTreeSet<u32> = if mutates.is_empty() {
            BTreeSet::new()
        } else {
            st.free.clone()
        };
        let mut mut_tail = st.n_pages + 1;
        let mut mut_pending: HashMap<u32, Vec<u8>> = HashMap::new();
        let phase_t0 = std::time::Instant::now();
        for item in mutates {
            match self.run_mutation(
                &st,
                item,
                &mut mut_free,
                &mut mut_tail,
                &mut mut_pending,
                &mut cache,
            ) {
                Ok(out) => mutate_outcomes.push((item, out)),
                Err(why) => {
                    if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                        eprintln!("[splice] mutate declined {:?}: {}", item.key, why.0);
                    }
                    return Err(SpliceError::MutateFallback(vec![item.key.clone()]));
                }
            }
        }
        for (item, out) in mutate_outcomes {
            if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                eprintln!(
                    "[splice] mutate obj {:?}: root={} touched={} +{} -{} freed={}",
                    item.key,
                    out.root,
                    out.pages.len(),
                    out.added.len(),
                    out.removed.len(),
                    out.freed.len()
                );
            }
            let old_root = st.spans.get(&item.key).map(|s| s.root);
            roots_moved |= old_root != Some(out.root);
            // Destructure once: the touched pages MOVE into the commit
            // set (no per-page clone — the outcome is consumed here).
            let MutateOutcome {
                root: out_root,
                pages: out_pages,
                added: out_added,
                removed: out_removed,
                max_page: out_max_page,
                took_from_free: out_took,
                freed: out_freed,
            } = out;
            // Only pages whose bytes actually differ enter the commit set.
            for (pgno, bytes) in out_pages {
                // Borrow-compare: a hit is never cloned (the old path
                // copied 4 KiB per touched page just to compare it).
                match reader.read_ref(pgno, ps, &st.overlay, &mut cache) {
                    Some(old) if old.as_slice() == bytes.as_slice() => {}
                    _ => {
                        changed.insert(pgno, bytes);
                    }
                }
            }
            if !out_took.is_empty() || !out_freed.is_empty() {
                st.free_version += 1;
            }
            for p in &out_took {
                st.free.remove(p);
            }
            for p in &out_freed {
                st.free.insert(*p);
            }
            st.n_pages = st.n_pages.max(out_max_page);
            st.next_ver += 1;
            let ver = st.next_ver;
            st.span_versions.insert(item.key.clone(), ver);
            // Incremental span maintenance: apply the session's page
            // deltas to the existing span record (O(changes); the
            // common commit adds and removes nothing — a page-level
            // splice of a leaf cell moves no page identities).
            match st.spans.get_mut(&item.key) {
                Some(rec) => {
                    rec.root = out_root;
                    for p in &out_removed {
                        if let Ok(i) = rec.pages.binary_search(p) {
                            rec.pages.remove(i);
                        }
                    }
                    for p in &out_added {
                        match rec.pages.binary_search(p) {
                            Ok(_) => {}
                            Err(i) => rec.pages.insert(i, *p),
                        }
                    }
                }
                None => {
                    return Err(SpliceError::Other(format!(
                        "mutation outcome for unknown span {:?}",
                        item.key
                    )));
                }
            }
        }

        // ---- 1. Splice each object ----
        PHASE_MUTATE_NS.fetch_add(phase_t0.elapsed().as_nanos() as u64, Relaxed);
        let phase_t0 = std::time::Instant::now();
        for item in items {
            let old_span = st.spans.get(&item.key).cloned();
            let mut reuse: Vec<u32> = Vec::new();
            if let Some(s) = &old_span {
                reuse.extend_from_slice(&s.pages);
            }
            reuse.extend(st.free.iter().copied());
            let build: SpliceBuild =
                splice_object(ps, reuse, st.n_pages + 1, &item.object, st.text_enc)
                    .map_err(SpliceError::Other)?;
            let moved = old_span.as_ref().map(|s| s.root) != Some(build.root);
            roots_moved |= moved;
            if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                eprintln!(
                    "[splice] obj {:?}: old_root={:?} new_root={} used={:?}",
                    item.key,
                    old_span.as_ref().map(|s| s.root),
                    build.root,
                    &build.used[..build.used.len().min(8)]
                );
            }
            // Only pages whose bytes actually differ enter the commit
            // set (identical rebuilds — a failed-but-restored statement
            // bumped the epoch — cost nothing).
            for (pgno, bytes) in &build.pages {
                match reader.read_ref(*pgno, ps, &st.overlay, &mut cache) {
                    Some(old) if old == bytes => {}
                    _ => {
                        changed.insert(*pgno, bytes.clone());
                    }
                }
            }
            // Span bookkeeping: consumed free pages leave the freelist;
            // old pages the rebuild no longer needs enter it.
            let new_set: BTreeSet<u32> = build.used.iter().copied().collect();
            let mut free_moved = !new_set.is_empty();
            for p in &new_set {
                free_moved |= st.free.remove(p);
            }
            if let Some(s) = &old_span {
                for p in &s.pages {
                    if !new_set.contains(p) {
                        st.free.insert(*p);
                        free_moved = true;
                    }
                }
            }
            if free_moved {
                st.free_version += 1;
            }
            st.n_pages = st.n_pages.max(build.max_page);
            st.next_ver += 1;
            let ver = st.next_ver;
            st.span_versions.insert(item.key.clone(), ver);
            st.spans.insert(
                item.key.clone(),
                SpanRec {
                    root: build.root,
                    pages: build.used,
                },
            );
        }

        // ---- 1.5 Schema tree (a rootpage moved, DDL, or a drop) ----
        // The schema rows' content changed: rebuild the sqlite_schema
        // tree in place (root fixed at page 1), reusing its own previous
        // pages first. An EMPTY row set is a legitimate state (every
        // table dropped) and must still rebuild — a stale row would
        // reference pages the freelist now owns. The row descriptors
        // are produced HERE (lazily): a data-only commit on a stable
        // layout never pays the O(#objects) walk.
        let rebuild_schema = roots_moved || schema_changed || !dropped.is_empty();
        if rebuild_schema {
            let schema_rows = schema_rows_of();
            if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                eprintln!("[splice] schema-tree rebuild (roots moved)");
            }
            let cells: Vec<SchemaRowCell> = schema_rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let root = r
                        .key
                        .as_ref()
                        .and_then(|k| st.spans.get(&SpanKey::Object(k.clone())).map(|s| s.root))
                        .unwrap_or(0);
                    SchemaRowCell {
                        rowid: i as i64 + 1,
                        kind: r.kind.clone(),
                        name: r.name.clone(),
                        tbl_name: r.tbl_name.clone(),
                        root,
                        sql: r.sql.clone(),
                    }
                })
                .collect();
            let old_span = st.spans.get(&SpanKey::SchemaTree).cloned();
            let mut reuse: Vec<u32> = old_span
                .as_ref()
                .map(|s| s.pages.iter().copied().filter(|&p| p != 1).collect())
                .unwrap_or_default();
            reuse.extend(st.free.iter().copied());
            let build = splice_schema_tree(ps, reuse, st.n_pages + 1, &cells, st.text_enc)
                .map_err(SpliceError::Other)?;
            for (pgno, bytes) in &build.pages {
                match reader.read_ref(*pgno, ps, &st.overlay, &mut cache) {
                    Some(old) if old == bytes => {}
                    _ => {
                        changed.insert(*pgno, bytes.clone());
                    }
                }
            }
            let new_set: BTreeSet<u32> = build.used.iter().copied().collect();
            let mut free_moved = !new_set.is_empty();
            for p in &new_set {
                free_moved |= st.free.remove(p);
            }
            if let Some(s) = &old_span {
                for p in &s.pages {
                    if *p != 1 && !new_set.contains(p) {
                        st.free.insert(*p);
                        free_moved = true;
                    }
                }
            }
            if free_moved {
                st.free_version += 1;
            }
            st.n_pages = st.n_pages.max(build.max_page);
            st.next_ver += 1;
            let ver = st.next_ver;
            st.span_versions.insert(SpanKey::SchemaTree, ver);
            let mut pages: Vec<u32> = Vec::with_capacity(build.used.len() + 1);
            pages.push(1);
            pages.extend(build.used.iter().copied().filter(|&p| p != 1));
            st.spans
                .insert(SpanKey::SchemaTree, SpanRec { root: 1, pages });
        }

        // ---- 2. Freelist structure ----
        PHASE_SPLICE_NS.fetch_add(phase_t0.elapsed().as_nanos() as u64, Relaxed);
        let phase_t0 = std::time::Instant::now();
        // The freelist-cache read side. Every `st.free` mutation this
        // commit happened above (each bumps `free_version`); nothing
        // below mutates it. When the generation is unchanged the
        // committed view already holds this exact structure (the
        // commit that installed the cache either wrote those pages or
        // byte-verified them identical), so the O(freelist pages)
        // rebuild + read + compare collapses to two u32 reads. The
        // fresh build is stashed and installed ONLY at successful
        // exits — a failed commit must never leave the cache marked
        // valid for pages that never landed.
        type FreshFreelist = ((u32, u32), Vec<(u32, Vec<u8>)>);
        let mut fl_fresh: Option<FreshFreelist> = None;
        let cache_ok = st.free_built_version == st.free_version
            && std::env::var_os("RSQL_NO_FREECACHE").is_none();
        let (fl_head, fl_count) = if cache_ok {
            st.free_cache_head
        } else {
            let free_vec: Vec<u32> = st.free.iter().copied().collect();
            let fl = build_freelist(&free_vec, ps);
            for (pgno, bytes) in &fl.pages {
                match reader.read_ref(*pgno, ps, &st.overlay, &mut cache) {
                    Some(old) if old == bytes => {}
                    _ => {
                        changed.insert(*pgno, bytes.clone());
                    }
                }
            }
            let head_count = (fl.head, fl.count);
            fl_fresh = Some((head_count, fl.pages));
            head_count
        };

        // ---- 3. Header (page 1) — SQLite's WAL frame discipline ----
        // In WAL mode page 1 rides a commit ONLY when its content
        // changed beyond the counter/size fields (measured against real
        // SQLite: its data-only WAL commits — growth included — carry
        // NO page-1 frame at all; the commit frame's db-size field is
        // the size truth until the checkpoint folds it, and the change
        // counter never moves). Rollback commits keep the classic
        // counter-rewrite shape.
        let header = build_header_enc(
            st.journal_wal,
            ps,
            st.n_pages,
            counter_eff,
            params.schema_cookie,
            params.user_version,
            params.application_id,
            st.text_enc.as_u32(),
            0, // largest root: auto-vacuum files never splice
            0,
            fl_head,
            fl_count,
        );
        let page1_needed = if !wal_mode {
            true
        } else if changed.contains_key(&1) {
            // The schema tree (rooted at page 1) rebuilt this commit.
            true
        } else {
            match reader.read(1, ps, &st.overlay, &mut cache) {
                Some(base) => {
                    let beyond = header_diff_beyond_counter_fields(&header, &base);
                    let grew = st.n_pages > committed_pages;
                    beyond || grew
                }
                None => true, // cannot verify the base — write page 1
            }
        };
        if changed.is_empty() && !page1_needed {
            if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                eprintln!(
                    "[splice] no-op: items={} roots_moved={}",
                    items.len(),
                    roots_moved
                );
            }
            // A successful exit: install the freshly built freelist
            // cache (its pages all matched — the committed view holds
            // them; nothing to write).
            if let Some(((head, count), pages)) = fl_fresh {
                st.free_cache_head = (head, count);
                st.free_cache = pages.into_iter().collect();
                st.free_built_version = st.free_version;
            }
            st.page_cache = cache;
            return Ok(false);
        }
        // The base page 1: the schema-tree rebuild's FRESH page when
        // this commit rebuilt it (its schema rows carry the new
        // rootpages at offset 100+ — the header patch must land ON that
        // page, never on the pre-rebuild version from the reader, or
        // the rows' rootpage column would silently regress), else the
        // committed page 1 from the reader (header-only commits).
        if page1_needed {
            let mut page1 = match changed.get(&1) {
                Some(p) => p.clone(),
                None => reader.read(1, ps, &st.overlay, &mut cache).ok_or_else(|| {
                    SpliceError::Other(format!(
                        "{}: page 1 unreadable for the header patch",
                        self.path.display()
                    ))
                })?,
            };
            page1[0..100].copy_from_slice(&header);
            changed.insert(1, page1);
        }
        st.user_version = params.user_version;
        st.application_id = params.application_id;
        st.change_counter = counter_eff;
        st.schema_cookie = params.schema_cookie;

        // ---- 4. Commit protocol ----
        PHASE_TAIL_NS.fetch_add(phase_t0.elapsed().as_nanos() as u64, Relaxed);
        let phase_t0 = std::time::Instant::now();
        let frames: Vec<(u32, Vec<u8>)> = changed.into_iter().collect();
        if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
            eprintln!(
                "[splice] commit: items={} frames={} wal_mode={}",
                items.len(),
                frames.len(),
                st.journal_wal
            );
        }
        if st.journal_wal {
            let db_size = st.n_pages;
            let mut writer = st.wal.take().unwrap_or_else(|| WalWriter::new(ps, 0));
            let pre_len = writer.wal_len();
            let mut scratch = std::mem::take(&mut st.commit_scratch);
            writer.encode_commit_into(&mut scratch, &frames, db_size);
            let wal_path = wal_path_of(&self.path);
            // The append fsyncs only under synchronous=FULL/EXTRA.
            let sync = params.synchronous >= 2;
            let append_r = append_wal_cached(&mut st, &wal_path, &writer, pre_len, &scratch, sync);
            // Hand the buffer back even on error: the capacity is the
            // point (one Vec for the coordinator's life), the bytes are
            // already consumed by the append attempt.
            st.commit_scratch = scratch;
            append_r.map_err(SpliceError::Other)?;
            for (pgno, page) in &frames {
                st.overlay.insert(*pgno, page.clone());
                // Page ownership moves to the overlay (the sidecar is
                // its committed home until the checkpoint folds it).
                cache.remove(pgno);
            }
            if writer.should_checkpoint() {
                // SQLite's 1000-page autocheckpoint: fold the committed
                // frames back into the main file (O(frames) page
                // copies), then reset the sidecar.
                if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                    eprintln!(
                        "[splice] autocheckpoint firing (frames={})",
                        writer.n_frames()
                    );
                }
                let ck_t0 = std::time::Instant::now();
                checkpoint(&self.path, ps, params.synchronous != 0).map_err(SpliceError::Other)?;
                // The sidecar was just retired: the cached append
                // handle is a ghost until the slow path re-opens it.
                st.wal_io = None;
                fold_overlay_into_cache(&mut st, &mut cache);
                let seq = writer.ckpt_seq() + 1;
                writer = WalWriter::new(ps, seq);
                st.record_guard(&self.path);
                PHASE_CHECKPOINT_NS.fetch_add(ck_t0.elapsed().as_nanos() as u64, Relaxed);
            }
            st.wal = Some(writer);
        } else {
            // Rollback-journal protocol (atomiccommit.html): pre-images
            // of every existing page about to change, in-place writes,
            // journal deletion as the commit point.
            let old_pages: Vec<(u32, Vec<u8>)> = frames
                .iter()
                .filter(|(p, _)| *p <= n_old)
                .map(|(p, _)| {
                    let bytes = reader
                        .read(*p, ps, &st.overlay, &mut cache)
                        .ok_or_else(|| {
                            SpliceError::Other(format!(
                                "{}: pre-image of page {p} unreadable",
                                self.path.display()
                            ))
                        })?;
                    Ok((*p, bytes))
                })
                .collect::<Result<_, SpliceError>>()?;
            super::rj::write_journal(&self.path, &old_pages, ps, n_old)
                .map_err(SpliceError::Other)?;
            super::rj::write_pages(&self.path, ps, &frames).map_err(SpliceError::Other)?;
            std::fs::remove_file(super::rj::journal_path_of(&self.path))
                .map_err(|e| SpliceError::Other(format!("remove journal: {e}")))?;
            // The main file is now the committed view — no overlay, and
            // the in-place writes refresh the page cache (the main file
            // is those pages' home again).
            st.overlay.clear();
            for (p, b) in &frames {
                cache.insert(*p, b.clone());
            }
            st.record_guard(&self.path);
        }
        st.epoch += 1;
        st.page_cache = cache;
        // The commit landed: the committed view now holds every
        // freelist page of the fresh build (written this commit, or
        // byte-verified identical above). Install the cache.
        if let Some(((head, count), pages)) = fl_fresh {
            st.free_cache_head = (head, count);
            st.free_cache = pages.into_iter().collect();
            st.free_built_version = st.free_version;
        }
        PHASE_COMMIT_NS.fetch_add(phase_t0.elapsed().as_nanos() as u64, Relaxed);
        Ok(true)
    }

    /// The span versions for a set of object names (0 when no span).
    /// The session records these at each publish's close.
    pub fn span_versions_of(&self, names: &[String]) -> HashMap<String, u64> {
        let st = self.state.lock();
        names
            .iter()
            .map(|n| {
                let v = st
                    .span_versions
                    .get(&SpanKey::Object(n.clone()))
                    .copied()
                    .unwrap_or(0);
                (n.clone(), v)
            })
            .collect()
    }

    /// The coordinator's current span version for one object
    /// (0 = no span installed). Sessions compare this against the
    /// version their last publish recorded before offering a
    /// page-level mutation.
    pub fn span_version(&self, key: &SpanKey) -> u64 {
        self.state
            .lock()
            .span_versions
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// The object's current span size in pages (0 = no span). The
    /// mutate-vs-splice offer uses it: a huge delta set still beats the
    /// whole-object splice when the object is big enough that
    /// re-collecting every row would cost more than descending per
    /// delta.
    pub fn span_page_count(&self, key: &SpanKey) -> usize {
        self.state
            .lock()
            .spans
            .get(key)
            .map(|s| s.pages.len())
            .unwrap_or(0)
    }

    /// Checkpoint NOW (`PRAGMA wal_checkpoint`): fold the sidecar's
    /// committed frames into the main file and retire it. Returns
    /// (pages written, db size in pages) — (0, 0) when there is nothing
    /// to fold (SQLite reports 0/0/0 too). `sync` follows
    /// `PRAGMA synchronous` (OFF skips the fsync).
    pub fn checkpoint_now(&self, sync: bool) -> Result<(usize, u32), String> {
        let (ps, has_frames) = {
            let st = self.state.lock();
            (
                st.page_size,
                st.journal_wal && st.wal.as_ref().is_some_and(|w| w.has_frames()),
            )
        };
        if ps == 0 || !has_frames {
            return Ok((0, 0));
        }
        let r = checkpoint(&self.path, ps, sync)?;
        let mut st = self.state.lock();
        st.wal_io = None;
        let mut cache = std::mem::take(&mut st.page_cache);
        fold_overlay_into_cache(&mut st, &mut cache);
        st.page_cache = cache;
        let seq = st.wal.as_ref().map(|w| w.ckpt_seq() + 1).unwrap_or(0);
        st.wal = Some(WalWriter::new(ps, seq));
        st.record_guard(&self.path);
        Ok(r)
    }

    /// Run one object's page-level mutation against the committed view
    /// (main file + overlay) — PURE: nothing in `st` moves; the
    /// outcome (touched pages, new span, freelist deltas) is applied
    /// by the caller only after EVERY mutation succeeded.
    #[allow(clippy::too_many_arguments)]
    fn run_mutation(
        &self,
        st: &ContainerState,
        item: &MutateItem,
        free: &mut BTreeSet<u32>,
        tail: &mut u32,
        pending: &mut HashMap<u32, Vec<u8>>,
        cache: &mut HashMap<u32, Vec<u8>>,
    ) -> Result<MutateOutcome, mutator::MutateFallback> {
        let span = st.spans.get(&item.key).ok_or_else(|| {
            mutator::MutateFallback("object has no span (first publish pending?)".into())
        })?;
        // BORROW the span's page list (no O(object-pages) clone per
        // commit — the mutation session reads it for membership checks
        // only and never mutates it).
        let span0: &[u32] = &span.pages;
        let root = span.root;
        let ps = st.page_size;
        let enc = st.text_enc;
        let free_vec: Vec<u32> = free.iter().copied().collect();
        let start_tail = *tail;
        let overlay = &st.overlay;
        // Earlier sessions of THIS publish rewrote pages into `pending`;
        // the scratch view serves them ahead of the committed view (a
        // page one session pruned and another took must read back as
        // the new owner's content, never the stale overlay's).
        let scratch: HashMap<u32, Vec<u8>> = pending.clone();
        let mut reader = PageReader::new(&self.path);
        let mut src = move |pgno: u32| -> Option<Vec<u8>> {
            if let Some(p) = scratch.get(&pgno) {
                return Some(p.clone());
            }
            if let Some(p) = overlay.get(&pgno) {
                return Some(p.clone());
            }
            reader.read(pgno, ps, &HashMap::new(), cache)
        };
        let m = mutator::Mutator::new(ps, enc, &mut src, free_vec, start_tail, span0);
        let out = m.run(root, item.ops.clone())?;
        // Retire this session's grants from the shared view so the next
        // session cannot hand the same pages out (the publish installs
        // the outcomes into the coordinator state only after every
        // session succeeds — the view is the interim truth).
        for p in &out.took_from_free {
            free.remove(p);
        }
        for p in &out.freed {
            free.insert(*p);
        }
        *tail = (*tail).max(out.max_page + 1);
        for (pgno, bytes) in &out.pages {
            pending.insert(*pgno, bytes.clone());
        }
        Ok(out)
    }

    /// Session bookkeeping: a new session attached (the close-time
    /// checkpoint fires when the LAST session detaches).
    pub fn session_attach(&self) {
        self.state.lock().attached += 1;
    }

    /// Session teardown: when the last session leaves, fold the WAL
    /// sidecar into the main file and retire it (the clean-close
    /// physical contract). `session_had_pending` carries the session's
    /// own "commits may live only in the sidecar" flag — when the
    /// sidecar has VANISHED (another session's full publish retired
    /// it) or is damaged, the last session re-publishes its committed
    /// state instead of silently dropping its last transaction.
    pub fn session_detach(&self, session_had_pending: bool) -> DetachOutcome {
        let (is_last, has_frames, ps) = {
            let mut st = self.state.lock();
            st.attached = st.attached.saturating_sub(1);
            let last = st.attached == 0;
            let frames = st.journal_wal && st.wal.as_ref().is_some_and(|w| w.has_frames());
            (last, frames, st.page_size)
        };
        if !is_last {
            return DetachOutcome::NotLast;
        }
        if has_frames && ps > 0 {
            let sync = self.state.lock().synchronous != 0;
            match checkpoint(&self.path, ps, sync) {
                Ok(_) => {
                    let mut st = self.state.lock();
                    st.wal_io = None;
                    let mut cache = std::mem::take(&mut st.page_cache);
                    fold_overlay_into_cache(&mut st, &mut cache);
                    st.page_cache = cache;
                    let seq = st.wal.as_ref().map(|w| w.ckpt_seq() + 1).unwrap_or(0);
                    st.wal = Some(WalWriter::new(ps, seq));
                    st.record_guard(&self.path);
                    return DetachOutcome::Folded;
                }
                Err(_) => {
                    // Damaged/unreadable sidecar: the session's memory is
                    // the authority for what it committed.
                    return if session_had_pending {
                        DetachOutcome::Republish
                    } else {
                        DetachOutcome::Folded
                    };
                }
            }
        }
        // No frames pending in the shared writer: either everything is
        // already checkpointed, or another session's full publish reset
        // the page space under this session's pending frames.
        if session_had_pending {
            return DetachOutcome::Republish;
        }
        DetachOutcome::Folded
    }
}

impl ContainerState {
    /// Record the main-file identity (length, header counter, header
    /// size) after any protocol step that wrote it, so later splices
    /// can detect an external touch. The step just rewrote the file:
    /// the cached read handle's identity is stale by construction, so
    /// it drops here and the next probe re-validates (once per
    /// checkpoint / rollback commit / full publish — never per WAL
    /// commit).
    fn record_guard(&mut self, path: &Path) {
        self.main_io = None;
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let (counter, size) = read_header_prefix(path)
            .map(|h| {
                (
                    u32::from_be_bytes(h[24..28].try_into().unwrap()),
                    u32::from_be_bytes(h[28..32].try_into().unwrap()),
                )
            })
            .unwrap_or((0, 0));
        self.guard = (len, counter, size);
    }
}

/// The external-touch guard's file probe: the main file's current
/// `(length, change counter, db size)`, taken through the cached read
/// handle when the path still names the same file (one stat + one
/// positioned read), or a fresh open otherwise (which then becomes the
/// cached handle).
fn main_guard_probe(st: &mut ContainerState, path: &Path) -> Option<(u64, u32, u32)> {
    let t0 = std::time::Instant::now();
    let r = main_guard_probe_inner(st, path);
    GUARD_PROBE_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
    r
}

fn main_guard_probe_inner(st: &mut ContainerState, path: &Path) -> Option<(u64, u32, u32)> {
    let meta = std::fs::metadata(path).ok()?;
    let head = match st.main_io.as_mut() {
        Some(io) if io.matches(&meta) => {
            let mut buf = [0u8; 100];
            match pread_exact(&mut io.file, &mut buf, 0) {
                Ok(()) => buf,
                // The handle went bad under us (unlinked and evicted,
                // I/O error): reopen through the verified path.
                Err(_) => {
                    let mut f = std::fs::File::open(path).ok()?;
                    let mut buf = [0u8; 100];
                    pread_exact(&mut f, &mut buf, 0).ok()?;
                    st.main_io = Some(FileIo::of(&meta, f));
                    buf
                }
            }
        }
        _ => {
            // Cold cache, or the path no longer names the cached file
            // (replacement / truncation / growth): open the CURRENT
            // file, read through it, and re-cache.
            let mut f = std::fs::File::open(path).ok()?;
            let mut buf = [0u8; 100];
            pread_exact(&mut f, &mut buf, 0).ok()?;
            st.main_io = Some(FileIo::of(&meta, f));
            buf
        }
    };
    Some((
        meta.len(),
        u32::from_be_bytes(head[24..28].try_into().unwrap()),
        u32::from_be_bytes(head[28..32].try_into().unwrap()),
    ))
}

/// The first 100 header bytes of a SQLite file (best effort).
fn read_header_prefix(path: &Path) -> Option<[u8; 100]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 100];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Debug/CI kill switch: force every WAL append through the fully
/// verified slow path (open/stat/salt-check per commit) — an A/B
/// switch for the cached-handle fast path's contribution.
fn wal_fastpath_disabled() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("RSQL_WAL_SLOWPATH").is_some())
}

/// Debug instrumentation: cumulative ns in the commit hot spots,
/// readable via `rustqlite::commit_timer_snapshot()`. Always-on —
/// the atomic adds are single-digit ns against µs-scale paths.
pub static CAN_SPLICE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static GUARD_PROBE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static APPEND_FAST_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static APPEND_SLOW_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PUBLISH_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PUBLISH_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Sub-phase timers inside publish_splice (mutation collection, the
/// splice/verify loop, schema+freelist+header rebuild, the commit
/// protocol block, the autocheckpoint itself).
pub static PHASE_MUTATE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PHASE_SPLICE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PHASE_TAIL_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PHASE_COMMIT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PHASE_CHECKPOINT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Cumulative commit-path instrumentation (see the statics above):
/// `[can_splice_ns, guard_probe_ns, append_fast_ns, append_slow_ns,
/// publish_splice_ns, publish_count]`.
pub fn commit_timer_snapshot() -> [u64; 6] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        CAN_SPLICE_NS.load(Relaxed),
        GUARD_PROBE_NS.load(Relaxed),
        APPEND_FAST_NS.load(Relaxed),
        APPEND_SLOW_NS.load(Relaxed),
        PUBLISH_NS.load(Relaxed),
        PUBLISH_COUNT.load(Relaxed),
    ]
}

/// Reader-path counters for the per-commit probes:
/// `[overlay_hits, cache_hits, file_opens, file_reads]`.
pub static READER_OVERLAY_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static READER_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static READER_FILE_OPENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static READER_FILE_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The reader-path counters (see the statics above).
pub fn reader_counter_snapshot() -> [u64; 4] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        READER_OVERLAY_HITS.load(Relaxed),
        READER_CACHE_HITS.load(Relaxed),
        READER_FILE_OPENS.load(Relaxed),
        READER_FILE_READS.load(Relaxed),
    ]
}

/// Sub-phase timers inside publish_splice — `[mutate_ns, splice_ns,
/// tail_ns (schema+freelist+header), commit_ns (protocol block,
/// incl. checkpoint), checkpoint_ns (the autocheckpoint alone)]`.
pub fn publish_phase_snapshot() -> [u64; 5] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        PHASE_MUTATE_NS.load(Relaxed),
        PHASE_SPLICE_NS.load(Relaxed),
        PHASE_TAIL_NS.load(Relaxed),
        PHASE_COMMIT_NS.load(Relaxed),
        PHASE_CHECKPOINT_NS.load(Relaxed),
    ]
}

/// Append a WAL commit through the cached sidecar handle: one path stat
/// (an external replacement / truncation / growth trips the fully
/// verified slow path) + one positioned write at the writer's tracked
/// length — SQLite's own discipline (the sidecar descriptor stays open
/// for the session's life; the engine never re-opens, re-stats and
/// re-verifies the salts per commit). Falls back to `append_wal_open`
/// whenever the cache is cold or the identity moved, then keeps the
/// handle it used warm for the next commit.
fn append_wal_cached(
    st: &mut ContainerState,
    path: &Path,
    writer: &WalWriter,
    pre_commit_len: u32,
    bytes: &[u8],
    sync: bool,
) -> Result<(), String> {
    let identity_ok = !wal_fastpath_disabled()
        && std::fs::metadata(path)
            .map(|m| st.wal_io.as_ref().map(|io| io.matches(&m)).unwrap_or(false))
            .unwrap_or(false);
    if identity_ok {
        let t0 = std::time::Instant::now();
        let io = st.wal_io.as_mut().expect("checked above");
        if io.len == pre_commit_len as u64 {
            let r = (|| -> Result<(), String> {
                pwrite_all(&mut io.file, bytes, io.len)
                    .map_err(|e| format!("write {}: {e}", path.display()))?;
                io.len += bytes.len() as u64;
                if sync {
                    io.file
                        .sync_all()
                        .map_err(|e| format!("fsync {}: {e}", path.display()))?;
                }
                Ok(())
            })();
            APPEND_FAST_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
            return r;
        }
    }
    // Slow path: cold cache, the sidecar's identity moved under us, or
    // the writer was reset (post-checkpoint generation). The full
    // open/verify/recover append, then keep its handle warm.
    if std::env::var_os("RSQL_WAL_FASTPATH_TRACE").is_some() {
        eprintln!(
            "[wal] slow path: identity_ok={} cached_len={:?} pre_len={}",
            identity_ok,
            st.wal_io.as_ref().map(|io| io.len),
            pre_commit_len
        );
    }
    st.wal_io = None;
    let t0 = std::time::Instant::now();
    let f = append_wal_open(path, writer, pre_commit_len, bytes, sync)?;
    let meta = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    st.wal_io = Some(FileIo::of(&meta, f));
    APPEND_SLOW_NS.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
    Ok(())
}

/// Trace a declined splice (diagnostics under `RSQL_SPLICE_TRACE`).
fn trace_decline(_st: &ContainerState, _params: &PublishParams, why: &str) {
    if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
        eprintln!("[splice] declined: {why}");
    }
}

/// Big-endian u16 at `off` of a header buffer.
fn be16_of(hdr: &[u8; 100], off: usize) -> u32 {
    u16::from_be_bytes([hdr[off], hdr[off + 1]]) as u32
}

/// Positioned full-page read into a reusable buffer.
fn read_file_page(file: &mut std::fs::File, page_size: u32, pgno: u32, buf: &mut Vec<u8>) -> bool {
    use std::io::{Read, Seek};
    let off = (pgno as u64 - 1) * page_size as u64;
    if file.seek(std::io::SeekFrom::Start(off)).is_err() {
        return false;
    }
    buf.clear();
    buf.resize(page_size as usize, 0);
    let mut done = 0usize;
    while done < buf.len() {
        match file.read(&mut buf[done..]) {
            Ok(0) => return false,
            Ok(n) => done += n,
            Err(_) => return false,
        }
    }
    true
}

/// One deferred reference extracted from a page before recursion
/// (recursion reuses the shared page buffer, so nothing may point into
/// it afterwards).
enum PageRef {
    Child(u32),
    /// The first overflow page of a cell (the chain is followed later).
    Overflow(u32),
}

/// Collect every page of one SQLite-format b-tree: interior children,
/// leaves and overflow chains. `hoff` is 100 for the schema root (page
/// 1), 0 otherwise. Loop-safe (visited pages short-circuit).
fn collect_tree_pages(
    file: &mut std::fs::File,
    page_size: u32,
    pgno: u32,
    hoff: usize,
    out: &mut BTreeSet<u32>,
    buf: &mut Vec<u8>,
) -> Result<(), String> {
    if pgno == 0 {
        return Err("b-tree references page 0".into());
    }
    if !out.insert(pgno) {
        return Ok(()); // already visited
    }
    if !read_file_page(file, page_size, pgno, buf) {
        return Err(format!("page {pgno} unreadable"));
    }
    // Snapshot everything the recursion needs BEFORE any recursion
    // clobbers the buffer.
    let page: Vec<u8> = buf.clone();
    if page.len() < hoff + 8 {
        return Err(format!("page {pgno} shorter than its header"));
    }
    let ptype = page[hoff];
    let interior = ptype == 0x05 || ptype == 0x02;
    let n_cells = u16::from_be_bytes([page[hoff + 3], page[hoff + 4]]) as usize;
    let mut refs: Vec<PageRef> = Vec::with_capacity(n_cells + 1);
    if interior {
        if hoff + 12 > page.len() {
            return Err(format!("page {pgno} interior header truncated"));
        }
        refs.push(PageRef::Child(u32::from_be_bytes(
            page[hoff + 8..hoff + 12].try_into().unwrap(),
        )));
    }
    let u = page_size as usize;
    let cp_arr = hoff + if interior { 12 } else { 8 };
    for i in 0..n_cells {
        let cp_off = cp_arr + i * 2;
        if cp_off + 2 > page.len() {
            return Err(format!("page {pgno} cell pointer {i} out of range"));
        }
        let cp = u16::from_be_bytes([page[cp_off], page[cp_off + 1]]) as usize;
        if cp >= page.len() {
            return Err(format!("page {pgno} cell {i} offset {cp} out of range"));
        }
        match ptype {
            0x05 => {
                // Table interior: child(4) + varint key.
                if cp + 4 > page.len() {
                    return Err(format!("page {pgno} interior cell {i} truncated"));
                }
                refs.push(PageRef::Child(u32::from_be_bytes(
                    page[cp..cp + 4].try_into().unwrap(),
                )));
            }
            0x02 | 0x0a => {
                // Index interior (child + entry) / index leaf: varint
                // total, local payload, u32 overflow pointer.
                let base = if ptype == 0x02 { cp + 4 } else { cp };
                let Some((total, used)) = super::varint::read_varint(&page, base) else {
                    return Err(format!("page {pgno} cell {i}: varint"));
                };
                let total = total.max(0) as usize;
                let x = ((u - 12) * 64 / 255) - 23;
                if total > x {
                    let m = ((u - 12) * 32 / 255) - 23;
                    let k = m + ((total - m) % (u - 4));
                    let local = if k <= x { k } else { m };
                    let ptr_at = base + used + local;
                    if ptr_at + 4 > page.len() {
                        return Err(format!("page {pgno} index cell {i} truncated"));
                    }
                    refs.push(PageRef::Overflow(u32::from_be_bytes(
                        page[ptr_at..ptr_at + 4].try_into().unwrap(),
                    )));
                }
                if ptype == 0x02 {
                    if cp + 4 > page.len() {
                        return Err(format!("page {pgno} interior cell {i} truncated"));
                    }
                    refs.push(PageRef::Child(u32::from_be_bytes(
                        page[cp..cp + 4].try_into().unwrap(),
                    )));
                }
            }
            0x0d => {
                // Table leaf: varint total, varint rowid, local, u32
                // overflow pointer.
                let Some((total, used)) = super::varint::read_varint(&page, cp) else {
                    return Err(format!("page {pgno} cell {i}: varint payload"));
                };
                let Some((_rowid, used2)) = super::varint::read_varint(&page, cp + used) else {
                    return Err(format!("page {pgno} cell {i}: varint rowid"));
                };
                let total = total.max(0) as usize;
                let x = u - 35;
                if total > x {
                    let m = ((u - 12) * 32 / 255) - 23;
                    let k = m + ((total - m) % (u - 4));
                    let local = if k <= x { k } else { m };
                    let ptr_at = cp + used + used2 + local;
                    if ptr_at + 4 > page.len() {
                        return Err(format!("page {pgno} cell {i} truncated"));
                    }
                    refs.push(PageRef::Overflow(u32::from_be_bytes(
                        page[ptr_at..ptr_at + 4].try_into().unwrap(),
                    )));
                }
            }
            other => {
                return Err(format!("page {pgno}: bad b-tree type 0x{other:02x}"));
            }
        }
    }
    for r in refs {
        match r {
            PageRef::Child(c) => collect_tree_pages(file, page_size, c, 0, out, buf)?,
            PageRef::Overflow(first) => {
                let mut next = first;
                let mut guard = 0u32;
                while next != 0 {
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err("overflow chain loop".into());
                    }
                    if !out.insert(next) {
                        return Ok(());
                    }
                    if !read_file_page(file, page_size, next, buf) {
                        return Err(format!("overflow page {next} unreadable"));
                    }
                    next = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                }
            }
        }
    }
    Ok(())
}

/// One decoded sqlite_schema row (the adopt path only needs the name
/// and rootpage).
struct AdoptSchemaRow {
    name: String,
    rootpage: u32,
}

/// Walk the sqlite_schema tree (root fixed at page 1) collecting its
/// pages and decoding its rows. Overflow payloads are reassembled
/// (schema rows can be long — big CREATE VIEW bodies).
fn parse_schema_rows(
    file: &mut std::fs::File,
    page_size: u32,
    enc: TextEnc,
    pages: &mut BTreeSet<u32>,
    buf: &mut Vec<u8>,
) -> Result<Vec<AdoptSchemaRow>, String> {
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    collect_schema_payloads(file, page_size, 1, true, pages, buf, &mut payloads)?;
    let mut rows = Vec::with_capacity(payloads.len());
    for payload in payloads {
        // sqlite_schema columns: type, name, tbl_name, rootpage, sql.
        let vals = super::record::decode_record_enc(&payload, 5, enc)
            .map_err(|e| format!("schema record: {e}"))?;
        let name = match vals.get(1) {
            Some(v) => v.as_text().to_string(),
            None => continue,
        };
        let rootpage = match vals.get(3) {
            Some(v) => v.as_integer().max(0) as u32,
            None => 0,
        };
        rows.push(AdoptSchemaRow { name, rootpage });
    }
    Ok(rows)
}

/// [`collect_tree_pages`]'s schema variant that also reassembles cell
/// payloads (the rows' record bytes).
fn collect_schema_payloads(
    file: &mut std::fs::File,
    page_size: u32,
    pgno: u32,
    page1: bool,
    pages: &mut BTreeSet<u32>,
    buf: &mut Vec<u8>,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), String> {
    if pgno == 0 {
        return Err("schema tree references page 0".into());
    }
    if !pages.insert(pgno) {
        return Ok(());
    }
    if !read_file_page(file, page_size, pgno, buf) {
        return Err(format!("schema page {pgno} unreadable"));
    }
    let page: Vec<u8> = buf.clone();
    let hoff = if page1 { 100 } else { 0 };
    if page.len() < hoff + 8 {
        return Err(format!("schema page {pgno} too small"));
    }
    let ptype = page[hoff];
    let n_cells = u16::from_be_bytes([page[hoff + 3], page[hoff + 4]]) as usize;
    let u = page_size as usize;
    match ptype {
        0x0d => {
            let cp_arr = hoff + 8;
            for i in 0..n_cells {
                let cp_off = cp_arr + i * 2;
                if cp_off + 2 > page.len() {
                    return Err(format!("schema page {pgno} cell ptr {i}"));
                }
                let cp = u16::from_be_bytes([page[cp_off], page[cp_off + 1]]) as usize;
                let Some((total, used)) = super::varint::read_varint(&page, cp) else {
                    return Err(format!("schema page {pgno} cell {i} varint"));
                };
                let Some((_rowid, used2)) = super::varint::read_varint(&page, cp + used) else {
                    return Err(format!("schema page {pgno} cell {i} rowid"));
                };
                let total = total.max(0) as usize;
                let body = cp + used + used2;
                let x = u - 35;
                if total <= x {
                    if body + total > page.len() {
                        return Err(format!("schema page {pgno} cell {i} body"));
                    }
                    out.push(page[body..body + total].to_vec());
                } else {
                    let m = ((u - 12) * 32 / 255) - 23;
                    let k = m + ((total - m) % (u - 4));
                    let local = if k <= x { k } else { m };
                    if body + local + 4 > page.len() {
                        return Err(format!("schema page {pgno} cell {i} overflow ptr"));
                    }
                    let mut payload = Vec::with_capacity(total);
                    payload.extend_from_slice(&page[body..body + local]);
                    let mut next = u32::from_be_bytes(
                        page[body + local..body + local + 4].try_into().unwrap(),
                    );
                    let mut remaining = total - local;
                    let mut guard = 0u32;
                    while next != 0 && remaining > 0 {
                        guard += 1;
                        if guard > 1_000_000 {
                            return Err("schema overflow loop".into());
                        }
                        pages.insert(next);
                        if !read_file_page(file, page_size, next, buf) {
                            return Err(format!("schema overflow page {next} unreadable"));
                        }
                        let take = (u - 4).min(remaining);
                        payload.extend_from_slice(&buf[4..4 + take]);
                        remaining -= take;
                        next = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                    }
                    if remaining != 0 {
                        return Err("schema overflow chain short".into());
                    }
                    out.push(payload);
                }
            }
            Ok(())
        }
        0x05 => {
            let cp_arr = hoff + 12;
            let mut children = Vec::with_capacity(n_cells + 1);
            for i in 0..n_cells {
                let cp_off = cp_arr + i * 2;
                if cp_off + 2 > page.len() {
                    return Err(format!("schema page {pgno} cell ptr {i}"));
                }
                let cp = u16::from_be_bytes([page[cp_off], page[cp_off + 1]]) as usize;
                if cp + 4 > page.len() {
                    return Err(format!("schema page {pgno} interior cell {i}"));
                }
                children.push(u32::from_be_bytes(page[cp..cp + 4].try_into().unwrap()));
            }
            children.push(u32::from_be_bytes(
                page[hoff + 8..hoff + 12].try_into().unwrap(),
            ));
            for child in children {
                collect_schema_payloads(file, page_size, child, false, pages, buf, out)?;
            }
            Ok(())
        }
        other => Err(format!("schema page {pgno}: bad type 0x{other:02x}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freelist_shapes() {
        use crate::storage::sqlitefmt::writer::build_freelist;
        // Empty.
        let fl = build_freelist(&[], 4096);
        assert_eq!((fl.head, fl.count), (0, 0));
        assert!(fl.pages.is_empty());
        // One page: a lone trunk.
        let fl = build_freelist(&[7], 4096);
        assert_eq!((fl.head, fl.count), (7, 1));
        assert_eq!(fl.pages.len(), 1);
        assert_eq!(&fl.pages[0].1[0..4], &0u32.to_be_bytes());
        assert_eq!(&fl.pages[0].1[4..8], &0u32.to_be_bytes());
        // A handful: first page is the trunk, the rest are its leaves.
        let fl = build_freelist(&[5, 6, 9, 20], 4096);
        assert_eq!(fl.head, 5);
        assert_eq!(fl.count, 4);
        assert_eq!(fl.pages.len(), 1);
        assert_eq!(&fl.pages[0].1[4..8], &3u32.to_be_bytes());
        assert_eq!(&fl.pages[0].1[8..12], &6u32.to_be_bytes());
        assert_eq!(&fl.pages[0].1[12..16], &9u32.to_be_bytes());
        assert_eq!(&fl.pages[0].1[16..20], &20u32.to_be_bytes());
        // Trunk capacity: (4096-8)/4 = 1022 leaves per trunk. 1023
        // pages = 1 trunk + exactly its 1022 leaves.
        let many: Vec<u32> = (2..=1024).collect(); // 1023 pages
        let fl = build_freelist(&many, 4096);
        assert_eq!(fl.count, 1023);
        assert_eq!(fl.pages.len(), 1);
        assert_eq!(&fl.pages[0].1[4..8], &1022u32.to_be_bytes());
        assert_eq!(fl.pages[0].1[8..8 + 4 * 1022].len(), 4088);
        // One more page: a second trunk, chained.
        let many: Vec<u32> = (2..=1025).collect(); // 1024 pages
        let fl = build_freelist(&many, 4096);
        assert_eq!(fl.count, 1024);
        assert_eq!(fl.pages.len(), 2);
        assert_eq!(&fl.pages[0].1[0..4], &fl.pages[1].0.to_be_bytes());
    }

    #[test]
    fn registry_keys_normalize() {
        let a = normalize_key(Path::new("/tmp/x/db.sqlite"));
        let b = normalize_key(Path::new("/tmp/x/./sub/../db.sqlite"));
        assert_eq!(a, b);
        assert!(normalize_key(Path::new("rel/db.sqlite")).is_absolute());
    }

    #[test]
    fn attach_is_shared_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.db");
        let a = attach(&p);
        a.session_attach();
        let b = attach(&p);
        b.session_attach();
        assert_eq!(a.state.lock().attached, 2);
        assert_eq!(a.session_detach(false), DetachOutcome::NotLast);
        assert_eq!(b.session_detach(false), DetachOutcome::Folded);
        assert_eq!(a.state.lock().attached, 0);
    }
}
