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
use super::reader::wal_path_of;
use super::record::TextEnc;
use super::wal::{append_wal, checkpoint, WalWriter};
use super::writer::{
    build_freelist, splice_object, splice_schema_tree, write_image_atomic, BuiltImage, OutObject,
    SchemaRowCell, SpanKey, SpanRec, SpliceBuild,
};

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
}

/// One object queued for a splice publish: the span it replaces and
/// its freshly collected content.
pub struct SpliceItem {
    pub key: SpanKey,
    pub object: OutObject,
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
    /// Free pages available for reuse (ascending by construction).
    pub free: BTreeSet<u32>,
    /// Pages newer than the main file: WAL frames committed since the
    /// last checkpoint. Bounded by the autocheckpoint threshold.
    pub overlay: HashMap<u32, Vec<u8>>,
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
    ) -> Option<Vec<u8>> {
        if let Some(p) = overlay.get(&pgno) {
            return Some(p.clone());
        }
        use std::io::{Read, Seek};
        if self.file.is_none() {
            self.file = Some(std::fs::File::open(self.path).ok()?);
        }
        let file = self.file.as_mut()?;
        let off = (pgno as u64 - 1) * page_size as u64;
        file.seek(std::io::SeekFrom::Start(off)).ok()?;
        let mut buf = vec![0u8; page_size as usize];
        file.read_exact(&mut buf).ok()?;
        Some(buf)
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
        st.free = free;
        st.overlay.clear();
        st.wal = if st.journal_wal {
            Some(WalWriter::new(ps, 0))
        } else {
            None
        };
        st.record_guard(&self.path);
        Ok(())
    }

    /// Splice preconditions: an established page space with the same
    /// geometry the session expects, and no sign the main file was
    /// touched by anyone but this coordinator since the last commit.
    pub fn can_splice(&self, params: &PublishParams) -> bool {
        let st = self.state.lock();
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
        let meta = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if meta != st.guard.0 {
            trace_decline(&st, params, "file length moved");
            return false;
        }
        match read_header_prefix(&self.path) {
            Some(head) => {
                if u32::from_be_bytes(head[24..28].try_into().unwrap()) != st.guard.1
                    || u32::from_be_bytes(head[28..32].try_into().unwrap()) != st.guard.2
                {
                    trace_decline(&st, params, "header counter/size moved");
                    return false;
                }
            }
            None => {
                trace_decline(&st, params, "header unreadable");
                return false;
            }
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
        st.free.clear();
        st.overlay.clear();
        st.force_full = false;
        st.wal = if params.journal_wal {
            Some(WalWriter::new(built.page_size, 0))
        } else {
            None
        };
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
        st.overlay.clear();
        st.wal = None;
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
        dropped: &[SpanKey],
        schema_changed: bool,
        schema_rows: &[SpliceSchemaRow],
        params: &PublishParams,
    ) -> Result<bool, String> {
        let mut st = self.state.lock();
        let ps = st.page_size;
        if ps == 0 {
            return Err("splice without an established page space".into());
        }
        let n_old = st.n_pages;
        let mut reader = PageReader::new(&self.path);
        let mut changed: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut roots_moved = false;

        // ---- 1a. Dropped objects: their pages join the freelist ----
        for key in dropped {
            if let Some(s) = st.spans.remove(key) {
                for p in s.pages {
                    st.free.insert(p);
                }
            }
        }

        // ---- 1. Splice each object ----
        for item in items {
            let old_span = st.spans.get(&item.key).cloned();
            let mut reuse: Vec<u32> = Vec::new();
            if let Some(s) = &old_span {
                reuse.extend_from_slice(&s.pages);
            }
            reuse.extend(st.free.iter().copied());
            let build: SpliceBuild =
                splice_object(ps, reuse, st.n_pages + 1, &item.object, st.text_enc)?;
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
                match reader.read(*pgno, ps, &st.overlay) {
                    Some(old) if &old == bytes => {}
                    _ => {
                        changed.insert(*pgno, bytes.clone());
                    }
                }
            }
            // Span bookkeeping: consumed free pages leave the freelist;
            // old pages the rebuild no longer needs enter it.
            let new_set: BTreeSet<u32> = build.used.iter().copied().collect();
            for p in &new_set {
                st.free.remove(p);
            }
            if let Some(s) = &old_span {
                for p in &s.pages {
                    if !new_set.contains(p) {
                        st.free.insert(*p);
                    }
                }
            }
            st.n_pages = st.n_pages.max(build.max_page);
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
        // reference pages the freelist now owns.
        let rebuild_schema = roots_moved || schema_changed || !dropped.is_empty();
        if rebuild_schema {
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
            let build = splice_schema_tree(ps, reuse, st.n_pages + 1, &cells, st.text_enc)?;
            for (pgno, bytes) in &build.pages {
                match reader.read(*pgno, ps, &st.overlay) {
                    Some(old) if &old == bytes => {}
                    _ => {
                        changed.insert(*pgno, bytes.clone());
                    }
                }
            }
            let new_set: BTreeSet<u32> = build.used.iter().copied().collect();
            for p in &new_set {
                st.free.remove(p);
            }
            if let Some(s) = &old_span {
                for p in &s.pages {
                    if *p != 1 && !new_set.contains(p) {
                        st.free.insert(*p);
                    }
                }
            }
            st.n_pages = st.n_pages.max(build.max_page);
            let mut pages: Vec<u32> = Vec::with_capacity(build.used.len() + 1);
            pages.push(1);
            pages.extend(build.used.iter().copied().filter(|&p| p != 1));
            st.spans
                .insert(SpanKey::SchemaTree, SpanRec { root: 1, pages });
        }

        // ---- 2. Freelist structure ----
        let free_vec: Vec<u32> = st.free.iter().copied().collect();
        let fl = build_freelist(&free_vec, ps);
        for (pgno, bytes) in &fl.pages {
            match reader.read(*pgno, ps, &st.overlay) {
                Some(old) if &old == bytes => {}
                _ => {
                    changed.insert(*pgno, bytes.clone());
                }
            }
        }

        // ---- 3. Header (page 1) ----
        // No data pages and no header-visible change → nothing to
        // commit (no counter bump — SQLite writes nothing either).
        let header_visible =
            params.user_version != st.user_version || params.application_id != st.application_id;
        if changed.is_empty() && !header_visible {
            if std::env::var_os("RSQL_SPLICE_TRACE").is_some() {
                eprintln!(
                    "[splice] no-op: items={} roots_moved={}",
                    items.len(),
                    roots_moved
                );
            }
            return Ok(false);
        }
        let header = build_header_enc(
            st.journal_wal,
            ps,
            st.n_pages,
            params.change_counter,
            params.schema_cookie,
            params.user_version,
            params.application_id,
            st.text_enc.as_u32(),
            0, // largest root: auto-vacuum files never splice
            0,
            fl.head,
            fl.count,
        );
        // The base page 1: the schema-tree rebuild's FRESH page when
        // this commit rebuilt it (its schema rows carry the new
        // rootpages at offset 100+ — the header patch must land ON that
        // page, never on the pre-rebuild version from the reader, or
        // the rows' rootpage column would silently regress), else the
        // committed page 1 from the reader (header-only commits).
        let mut page1 = match changed.get(&1) {
            Some(p) => p.clone(),
            None => reader.read(1, ps, &st.overlay).ok_or_else(|| {
                format!(
                    "{}: page 1 unreadable for the header patch",
                    self.path.display()
                )
            })?,
        };
        page1[0..100].copy_from_slice(&header);
        changed.insert(1, page1);
        st.user_version = params.user_version;
        st.application_id = params.application_id;
        st.change_counter = params.change_counter;
        st.schema_cookie = params.schema_cookie;

        // ---- 4. Commit protocol ----
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
            let bytes = writer.encode_commit(&frames, db_size);
            let wal_path = wal_path_of(&self.path);
            append_wal(&wal_path, &writer, pre_len, &bytes)?;
            for (pgno, page) in &frames {
                st.overlay.insert(*pgno, page.clone());
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
                checkpoint(&self.path, ps)?;
                st.overlay.clear();
                let seq = writer.ckpt_seq() + 1;
                writer = WalWriter::new(ps, seq);
                st.record_guard(&self.path);
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
                    let bytes = reader.read(*p, ps, &st.overlay).ok_or_else(|| {
                        format!("{}: pre-image of page {p} unreadable", self.path.display())
                    })?;
                    Ok((*p, bytes))
                })
                .collect::<Result<_, String>>()?;
            super::rj::write_journal(&self.path, &old_pages, ps, n_old)?;
            super::rj::write_pages(&self.path, ps, &frames)?;
            std::fs::remove_file(super::rj::journal_path_of(&self.path))
                .map_err(|e| format!("remove journal: {e}"))?;
            // The main file is now the committed view — no overlay.
            st.overlay.clear();
            st.record_guard(&self.path);
        }
        st.epoch += 1;
        Ok(true)
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
            match checkpoint(&self.path, ps) {
                Ok(_) => {
                    let mut st = self.state.lock();
                    st.overlay.clear();
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
    /// can detect an external touch.
    fn record_guard(&mut self, path: &Path) {
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

/// The first 100 header bytes of a SQLite file (best effort).
fn read_header_prefix(path: &Path) -> Option<[u8; 100]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 100];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
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
