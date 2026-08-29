//! Public API: `Database` and `Connection`.
//!
//! These are the user-facing types. They wrap the lower-level pager, catalog,
//! planner, and executor into a simple rusqlite-style API:
//!
//! ```no_run
//! use rustqlite::{Database, Value};
//! let mut db = Database::open("/tmp/my.db").unwrap();
//! db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
//! db.execute("INSERT INTO users (name) VALUES ('Alice')", []).unwrap();
//! let rows = db.query("SELECT * FROM users", []).unwrap();
//! ```

use crate::error::{Error, Result};
use crate::executor::{execute, ExecContext};
use crate::planner::plan::Plan;
use crate::planner::Planner;
use crate::schema::{build_table, Catalog, Index, Table};
use crate::sql::ast::*;
use crate::sql::parse;
use crate::storage::btree::{Btree, LookupResult};
use crate::storage::pager::Pager;
use crate::storage::row_codec::{decode_row, decode_row_selective, encode_row, encode_row_aliased};
use crate::types::{Row, Value};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The maximum number of pages cached in memory.
const DEFAULT_CACHE_PAGES: usize = 2048;

/// Lightweight phase profiler (nanosecond accumulators). Zero-cost when
/// not being read; used by `examples/phase_profile.rs` to attribute
/// per-statement cost to parse / plan / exec phases.
#[doc(hidden)]
pub mod profile {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub static ENABLED: AtomicBool = AtomicBool::new(false);
    pub static PARSE_NS: AtomicU64 = AtomicU64::new(0);
    pub static PLAN_NS: AtomicU64 = AtomicU64::new(0);
    pub static EXEC_NS: AtomicU64 = AtomicU64::new(0);
    pub static CACHE_NS: AtomicU64 = AtomicU64::new(0);
    pub static COUNT: AtomicU64 = AtomicU64::new(0);
    /// Returns Some(Instant) when profiling is enabled. One relaxed atomic
    /// load when disabled (~1 ns) — safe to call on hot paths.
    #[inline]
    pub fn now() -> Option<std::time::Instant> {
        if ENABLED.load(Ordering::Relaxed) {
            Some(std::time::Instant::now())
        } else {
            None
        }
    }
    #[inline]
    pub fn span(started: Option<std::time::Instant>, c: &AtomicU64) {
        if let Some(t) = started {
            c.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
    pub fn reset() {
        PARSE_NS.store(0, Ordering::Relaxed);
        PLAN_NS.store(0, Ordering::Relaxed);
        EXEC_NS.store(0, Ordering::Relaxed);
        CACHE_NS.store(0, Ordering::Relaxed);
        COUNT.store(0, Ordering::Relaxed);
    }
    pub fn dump() {
        let c = COUNT.load(Ordering::Relaxed) as f64;
        if c == 0.0 {
            println!("profile: no samples");
            return;
        }
        println!(
            "profile: n={:.0}  parse={:.3}us  plan={:.3}us  cache={:.3}us  exec={:.3}us  (sum={:.3}us)",
            c,
            PARSE_NS.load(Ordering::Relaxed) as f64 / c / 1000.0,
            PLAN_NS.load(Ordering::Relaxed) as f64 / c / 1000.0,
            CACHE_NS.load(Ordering::Relaxed) as f64 / c / 1000.0,
            EXEC_NS.load(Ordering::Relaxed) as f64 / c / 1000.0,
            (PARSE_NS.load(Ordering::Relaxed) + PLAN_NS.load(Ordering::Relaxed)
                + CACHE_NS.load(Ordering::Relaxed) + EXEC_NS.load(Ordering::Relaxed)) as f64
                / c / 1000.0,
        );
    }
}

/// Page size: 16 KiB (larger than SQLite's 4 KiB default) to reduce splits.
/// This trades some memory for fewer B+tree splits and better scan locality.
// const DEFAULT_PAGE_SIZE: u32 = 16384;

/// A database. Owns the pager and catalog.
///
/// All mutable state is wrapped in interior-mutability primitives
/// (`RwLock`/`Mutex`/`Atomic*`), so all public read methods take `&self`.
/// This lets N reader threads share a single `&Database` via `Arc<RwLock<Database>>`
/// and run queries concurrently. Writers take the outer write lock to get
/// `&mut Database`, which serializes them — but reads proceed without
/// blocking on the outer lock.
pub struct Database {
    pager: Pager,
    catalog: Catalog,
    path: PathBuf,
    /// Inside an explicit BEGIN..COMMIT/ROLLBACK transaction. Only mutated
    /// by the writer (which holds `&mut self` via the outer write lock),
    /// but wrapped for interior mutability so `&self` query paths can read it.
    in_transaction: AtomicBool,
    /// Snapshot taken at BEGIN, used by ROLLBACK to restore the pager's
    /// state to the pre-transaction point.
    txn_snapshot: Mutex<Option<crate::storage::pager::PagerSnapshot>>,
    /// Combined bookkeeping maps (table root overrides, index roots,
    /// max-rowid cache) behind ONE Arc — a query snapshot is a single
    /// read-lock + one refcount bump (previously three separate
    /// `RwLock<Arc<HashMap>>` fields: 3 locks + 3 atomic bumps per query).
    /// Only writes (DML causing root splits / rowid-cache fills) take the
    /// write lock, and the writer (`&mut self`) detaches the Arc entirely.
    maps: RwLock<std::sync::Arc<crate::executor::StmtMaps>>,
    /// Root page currently persisted in the schema table per object
    /// ("table:name" / "index:name" -> rootpage in the schema row).
    /// `sync_schema_roots` rewrites a schema row only when the live root
    /// diverges from this value — splits are rare, so the amortized cost
    /// is one schema-row rewrite per split.
    schema_root_pages: Mutex<HashMap<String, u32>>,
    /// Prepared-statement cache: SQL text → `CachedStmt`.
    /// Eliminates the parse+plan cost on repeated calls with the same SQL
    /// (the common case in real workloads: `INSERT INTO t VALUES (?)` is
    /// called N times in a loop, and `SELECT … WHERE id = ?` is called per
    /// request). The cache is invalidated on any DDL statement (CREATE/DROP/
    /// ALTER) because Plans hold `Arc<Table>` / `Arc<Index>` references that
    /// become stale when the schema changes.
    ///
    /// `Arc<Statement>` (not `Statement`) so cache hits are O(1) — just an
    /// atomic refcount increment. Previously this was `Statement`, which
    /// deep-cloned the entire AST (Strings, Vecs, nested Exprs) on every
    /// call. For a simple `SELECT * FROM t WHERE id = ?`, that was 10-50
    /// allocations per call, contributing to the 9× point-lookup gap vs
    /// SQLite. The `Plan` is `Clone`-cheap because it only holds `Arc`
    /// references internally.
    ///
    /// `has_subqueries` is precomputed ONCE at plan time so the query path
    /// doesn't re-walk the whole plan tree (which allocated a Vec of expr
    /// references) on every execution.
    ///
    /// RwLock so concurrent readers can hit the cache simultaneously; only
    /// a cache miss takes the brief write lock to insert.
    stmt_cache: RwLock<StmtCacheMap>,
    /// FIFO order of insertion into `stmt_cache`, used for eviction when the
    /// cache reaches `stmt_cache_capacity`. The first item in this Vec is the
    /// oldest entry and the next to be evicted.
    stmt_cache_order: Mutex<Vec<String>>,
    /// Maximum number of entries in the statement cache. Default 64.
    /// Immutable after open (only set via `set_stmt_cache_capacity`).
    stmt_cache_capacity: usize,
    /// Hashes of SQL statements seen at least once ("cache on second
    /// sight" filter). Populating the statement cache costs ~1 µs (two
    /// String allocations for the key + FIFO order entry, write-lock, Arc
    /// clones). For workloads where every statement text is unique —
    /// e.g. literal-inlined `INSERT ... VALUES ('name42', 42)` in a loop —
    /// that insert is pure waste: the entry is evicted before ever being
    /// hit again. With this filter, a statement is only admitted to the
    /// cache the SECOND time we see its hash; one-off statements pay just
    /// a ~5 ns hash insert. Repeated statements (the cache's real clientele)
    /// reach the cache on their 2nd execution and hit it from the 3rd on.
    /// Bounded: cleared wholesale when it grows past `seen_hashes_cap`.
    seen_hashes: Mutex<std::collections::HashSet<u64, FxHashBuild>>,
    seen_hashes_cap: usize,
    /// When true (default: false), the per-statement flush in exec_insert /
    /// exec_update / exec_delete is suppressed. Mirrors SQLite's
    /// `journal_mode=WAL + synchronous=NORMAL` behaviour — dirty pages
    /// accumulate in the pager cache and are flushed only on:
    ///   1. an explicit `Database::flush()` call,
    ///   2. a subsequent SELECT (forces flush for read consistency),
    ///   3. the dirty-page count exceeding `deferred_flush_threshold`.
    ///
    /// Big perf win for OLTP workloads where the user issues many single-row
    /// INSERT/UPDATE/DELETE statements in auto-commit mode: amortizes the
    /// `file.sync_all()` cost across N statements instead of paying it once
    /// per statement. The cost is reduced durability — unflushed writes can
    /// be lost on application crash (but not on transaction abort, since
    /// rollback uses the in-memory snapshot, not the on-disk state).
    deferred_flush: AtomicBool,
    /// Threshold for forcing a flush when `deferred_flush` is enabled.
    /// Default: 1000 dirty pages (~4 MB at 4 KiB page size).
    /// Immutable after open.
    deferred_flush_threshold: usize,
}

/// Default capacity of the statement cache.
const DEFAULT_STMT_CACHE_CAPACITY: usize = 64;

/// Shared empty-maps singleton: cloning costs one refcount bump instead
/// of an ArcBox allocation, which matters on the per-statement detach /
/// attach path in `execute` and on the ROLLBACK reset.
fn empty_maps() -> Arc<crate::executor::StmtMaps> {
    static E: std::sync::OnceLock<Arc<crate::executor::StmtMaps>> = std::sync::OnceLock::new();
    E.get_or_init(|| Arc::new(crate::executor::StmtMaps::empty())).clone()
}

/// Generic FxHash-style string hasher for statement-cache keys.
///
/// The statement cache is consulted on EVERY query/execute: hashing the
/// SQL text with std's default SipHash-1-3 costs ~40-80 ns for a typical
/// 35-60 byte statement; this multiply-rotate scheme is ~10-15 ns with
/// good avalanche for table indexing. Collisions are impossible to fully
/// rule out for any non-crypto hash, but the cache keys are compared by
/// full string equality on hit (HashMap semantics), so a collision costs
/// a wasted comparison — never a wrong result.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            let w = u64::from_le_bytes(buf);
            self.hash = (self.hash.rotate_left(5) ^ w).wrapping_mul(FX_SEED);
        }
    }
    #[inline]
    fn write_u8(&mut self, b: u8) {
        self.hash = (self.hash.rotate_left(5) ^ (b as u64)).wrapping_mul(FX_SEED);
    }
}

#[derive(Clone, Default)]
pub struct FxHashBuild;

impl std::hash::BuildHasher for FxHashBuild {
    type Hasher = FxHasher;
    #[inline]
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

/// Statement-cache map type with the fast string hasher.
pub type StmtCacheMap = HashMap<String, CachedStmt, FxHashBuild>;

/// Fast non-cryptographic hash (FxHash-style, as used by rustc) for the
/// statement "seen" filter. ~5 ns for a typical 60-byte statement; used
/// only as a heuristic gate, so collisions are harmless (they cause one
/// unnecessary cache insert, never a wrong result).
#[inline]
fn quick_sql_hash(s: &str) -> u64 {
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(s.as_bytes());
    h.finish()
}

/// A bound value for a fast-path lookup key, resolved at detection time.
#[derive(Clone, Debug)]
enum FastBound {
    /// Positional parameter index (from a numeric `?N` name).
    Param(usize),
    /// A literal constant.
    Literal(Value),
}

impl FastBound {
    /// Resolve the bound value against the statement's parameters.
    /// Mirrors `evaluate`'s Parameter semantics: missing params are NULL.
    #[inline]
    fn resolve<'p>(&'p self, params: &'p [Value]) -> &'p Value {
        match self {
            FastBound::Param(i) => params.get(*i).unwrap_or(&Value::Null),
            FastBound::Literal(v) => v,
        }
    }
}

/// Pre-compiled ultra-fast execution paths for the two hottest OLTP
/// shapes. Detected once at statement-cache population; execution skips
/// the ExecContext, EvalContext, and Plan dispatch entirely (~200 ns of
/// machinery), going straight to B+tree descent + selective row decode.
///
/// The full pipeline (ExecContext setup, execute() dispatch, Project
/// cloning) measured ~350 ns before the statement even touched a page —
/// larger than SQLite's ENTIRE point lookup (~355 ns). These paths close
/// that gap while keeping identical semantics: they funnel into the same
/// `lookup_table` / `lookup_index` / `decode_row*` routines the general
/// path uses.
#[derive(Clone)]
enum FastPath {
    /// `SELECT cols FROM t WHERE rowid_alias = ?` / `WHERE rowid = literal`
    RowidPoint {
        table: Arc<Table>,
        rowid: FastBound,
        /// `None` = identity projection (all columns). `Some(indices)` =
        /// project these table column indices (selective decode).
        project: Option<Vec<usize>>,
        /// Output column names.
        columns: Arc<[String]>,
    },
    /// `SELECT cols FROM t WHERE indexed_col = ?` (single- or multi-column
    /// unique/non-unique index point lookup).
    IndexPoint {
        table: Arc<Table>,
        index: Arc<Index>,
        keys: Vec<FastBound>,
        project: Option<Vec<usize>>,
        columns: Arc<[String]>,
    },
    /// `SELECT cols FROM t WHERE rowid BETWEEN ? AND ?` (or the planner's
    /// equivalent >= / <= conjunct pair). Skips the full pipeline for the
    /// small-range OLTP shape — the general path's fixed cost (~1 us:
    /// ExecContext setup, plan dispatch, result plumbing) dominated
    /// 1-100-row scans.
    RowidRange {
        table: Arc<Table>,
        start: FastBound,
        end: FastBound,
        project: Option<Vec<usize>>,
        columns: Arc<[String]>,
    },
}

/// precomputed `has_subqueries` flag (mirrors SQLite's OP_Once check —
/// computed once at plan time instead of re-walking the plan tree, which
/// allocated a Vec of expr references, on every execution).
#[derive(Clone)]
struct CachedStmt {
    stmt: Arc<Statement>,
    plan: Option<Arc<crate::planner::plan::Plan>>,
    has_subqueries: bool,
    /// Pre-compiled point-lookup fast path (see `FastPath`).
    fast_path: Option<Arc<FastPath>>,
}

impl FastPath {
    /// The table this fast path reads from.
    #[inline]
    fn table_name(&self) -> &str {
        match self {
            FastPath::RowidPoint { table, .. } => &table.name,
            FastPath::IndexPoint { table, .. } => &table.name,
            FastPath::RowidRange { table, .. } => &table.name,
        }
    }

    /// Output column names.
    #[inline]
    fn output_columns(&self) -> &Arc<[String]> {
        match self {
            FastPath::RowidPoint { columns, .. } => columns,
            FastPath::IndexPoint { columns, .. } => columns,
            FastPath::RowidRange { columns, .. } => columns,
        }
    }
}

/// Decode a row payload with an optional column projection.
/// `None` decodes all columns (identity — SELECT *); `Some(indices)` uses
/// the selective decoder, which skips over un-projected columns without
/// allocating Values for them.
#[inline]
fn decode_projected(payload: &[u8], table: &Table, rowid: i64, project: Option<&[usize]>) -> Result<Row> {
    match project {
        Some(idxs) => {
            let mut out = Vec::with_capacity(idxs.len());
            decode_row_selective(payload, table.n_columns(), idxs, rowid, table.rowid_alias, &mut out)?;
            Ok(out)
        }
        None => decode_row(payload, table.n_columns(), rowid, table.rowid_alias),
    }
}

/// Split a recursive CTE's compound SELECT into (base, set-op, recursive arm).
/// The body must be `... UNION [ALL] <recursive>`; more than two arms or a
/// non-UNION top-level operator is rejected (SQLite requires the same shape).
fn split_compound_cte(
    sel: &SelectStatement,
) -> Result<(
    SelectStatement,
    crate::sql::ast::SetOp,
    SelectStatement,
)> {
    use crate::sql::ast::{SelectBody, SelectStatement as Sel};
    // Rebuild a plain SelectStatement from a SelectBody by cloning the
    // statement shell with the body swapped.
    fn with_body(s: &Sel, body: SelectBody) -> Sel {
        let mut out = s.clone();
        out.body = body;
        out.with = None; // the outer WITH doesn't apply to inner arms
        out
    }
    match &sel.body {
        SelectBody::Binary { op, left, right } => match op {
            crate::sql::ast::SetOp::Union | crate::sql::ast::SetOp::UnionAll => Ok((
                with_body(sel, (**left).clone()),
                *op,
                with_body(sel, (**right).clone()),
            )),
            _ => Err(Error::semantic(
                "WITH RECURSIVE requires UNION or UNION ALL as the compound operator",
            )),
        },
        SelectBody::Simple(_) => Err(Error::semantic(
            "WITH RECURSIVE requires a compound SELECT (base UNION [ALL] recursive)",
        )),
    }
}

impl Database {
    /// Open or create a database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut pager = Pager::open(&path, DEFAULT_CACHE_PAGES)?;
        let mut catalog = Catalog::new();
        catalog.schema_cookie = pager.schema_cookie();
        // Load the schema from page 0 (the schema table root).
        load_schema(&mut pager, &mut catalog)?;
        // Seed the persisted-root map from the loaded schema so
        // sync_schema_roots only rewrites rows when a root actually moves.
        let mut schema_root_pages = HashMap::new();
        for (name, t) in catalog.all_tables() {
            schema_root_pages.insert(format!("table:{}", name), t.root_page);
        }
        for (name, i) in catalog.all_indexes() {
            schema_root_pages.insert(format!("index:{}", name), i.root_page);
        }
        Ok(Self {
            pager,
            catalog,
            path,
            in_transaction: AtomicBool::new(false),
            txn_snapshot: Mutex::new(None),
            maps: RwLock::new(empty_maps()),
            schema_root_pages: Mutex::new(schema_root_pages),
            stmt_cache: RwLock::new(StmtCacheMap::default()),
            stmt_cache_order: Mutex::new(Vec::new()),
            stmt_cache_capacity: DEFAULT_STMT_CACHE_CAPACITY,
            seen_hashes: Mutex::new(std::collections::HashSet::default()),
            seen_hashes_cap: 4096,
            deferred_flush: AtomicBool::new(false),
            deferred_flush_threshold: 1000,
        })
    }

    /// Open an in-memory database (no file). The data is lost when the
    /// `Database` is dropped.
    ///
    /// Uses a tempfile under the hood — but tells the pager to skip fsyncs
    /// since the file lives on tmpfs and will be deleted on close. This
    /// makes `:memory:` mode match SQLite's `:memory:` per-statement
    /// overhead (no fsync syscall round-trip per auto-commit INSERT,
    /// which was the dominant cost in the 4× INSERT gap vs SQLite).
    pub fn open_in_memory() -> Result<Self> {
        let path = PathBuf::from(":memory:");
        // Use a temp file under the hood — we don't support pure in-memory yet.
        let tmp = tempfile::NamedTempFile::new().map_err(|e| Error::Io(e))?;
        let mut db = Self::open(tmp.path())?;
        db.path = path;
        db.pager.set_skip_fsync(true);
        // Lazy write-back: per-statement flushes skip file writes entirely;
        // dirty pages spill to the temp file only on cache eviction. The
        // file is deleted on close, so this is pure win: autocommit
        // INSERTs in :memory: mode drop the write() syscall per statement.
        db.pager.set_lazy_writeback(true);
        Ok(db)
    }

    /// Set the statement cache capacity. A larger cache uses more memory but
    /// reduces parse+plan overhead on more unique SQL strings. Set to 0 to
    /// disable caching entirely.
    pub fn set_stmt_cache_capacity(&mut self, capacity: usize) {
        self.stmt_cache_capacity = capacity;
        // If shrinking, evict excess entries (FIFO).
        let mut cache = self.stmt_cache.write();
        let mut order = self.stmt_cache_order.lock();
        while cache.len() > capacity && !order.is_empty() {
            let oldest = order.remove(0);
            cache.remove(&oldest);
        }
    }

    /// Enable or disable deferred flush mode (lazy commit).
    ///
    /// When enabled, per-statement flushes in exec_insert / exec_update /
    /// exec_delete are suppressed. Dirty pages accumulate in the pager cache
    /// and are flushed only on:
    ///   1. an explicit `Database::flush()` call,
    ///   2. a subsequent `Database::query()` (forces flush for read
    ///      consistency),
    ///   3. the dirty-page count exceeding `deferred_flush_threshold`.
    ///
    /// Mirrors SQLite's `journal_mode=WAL + synchronous=NORMAL` behaviour.
    /// Big perf win for OLTP workloads (5–10× faster for single-row
    /// INSERT/UPDATE/DELETE in auto-commit mode). The trade-off is reduced
    /// durability: unflushed writes can be lost on application crash.
    pub fn set_deferred_flush(&mut self, enabled: bool) {
        self.deferred_flush.store(enabled, Ordering::Release);
    }

    /// Enable or disable FOREIGN KEY enforcement (same as
    /// `PRAGMA foreign_keys = ON/OFF`). Default: off, like SQLite.
    pub fn set_foreign_keys(&mut self, enabled: bool) {
        self.pager.set_foreign_keys_enabled(enabled);
    }

    /// Current FOREIGN KEY enforcement state.
    pub fn foreign_keys_enabled(&self) -> bool {
        self.pager.foreign_keys_enabled()
    }

    /// Set the dirty-page threshold at which a deferred-flush database
    /// auto-flushes. Default: 1000 pages.
    pub fn set_deferred_flush_threshold(&mut self, threshold: usize) {
        self.deferred_flush_threshold = threshold;
    }

    /// Explicitly flush all dirty pages to disk and fsync. No-op when no
    /// pages are dirty. Use this after a burst of writes when
    /// `set_deferred_flush(true)` is enabled.
    pub fn flush(&mut self) -> Result<()> {
        self.pager.flush()
    }

    /// Flush from a `&self` reference — used by concurrent readers when they
    /// need to see unflushed writes. Uses the pager's interior mutability.
    pub fn flush_shared(&self) -> Result<()> {
        self.pager.flush()
    }

    /// Look up the statement cache; on miss, parse + plan, store, and return.
    /// Returns a `CachedStmt` clone (three refcount bumps) so the caller can:
    /// - For SELECT (query path): use just the Plan (cheap Arc clone).
    /// - For DML/DDL (execute path): use the Arc<Statement> (cheap Arc clone).
    /// - The `has_subqueries` flag avoids re-walking the plan tree per query.
    ///
    /// The cached Plan is `Option<Plan>` and is cloned — cheap because
    /// `Plan` is `Clone` with only `Arc` references internally (no deep
    /// copies of large structures).
    ///
    /// On any cache lookup we DO NOT consult `self.catalog` mutably, so the
    /// borrow of `self.stmt_cache` and the immutable borrow of `self.catalog`
    /// don't conflict.
    /// Execute a fast-path INSERT (see `try_fast_insert_parse`).
    ///
    /// Returns `Ok(true)` when executed, `Ok(false)` when the fast path
    /// turns out not to apply (caller falls through to the general path),
    /// `Err` for a real failure. Not-applicable cases: WITHOUT ROWID /
    /// STRICT tables, generated columns, missing DEFAULT evaluation,
    /// duplicate column names (the general path produces nicer errors).
    fn exec_fast_insert(&mut self, fi: FastInsert<'_>) -> Result<bool> {
        let table = match self.catalog.get_table_fast(fi.table) {
            Some(t) => t,
            None => {
                // Unknown table — same error the planner produces.
                return Err(Error::NotFound(format!("table: {}", fi.table)));
            }
        };
        // Table-shape bails: fall through to the general path.
        if table.without_rowid
            || table.strict
            || table.columns.iter().any(|c| c.generated.is_some())
        {
            return Ok(false);
        }
        // If any column has a DEFAULT and the row doesn't supply every
        // column, defaults must be evaluated — general path.
        let supplies_all = fi.columns.is_empty()
            || (fi.columns.len() == table.n_columns());
        if !supplies_all && table.columns.iter().any(|c| c.default.is_some()) {
            return Ok(false);
        }

        // Resolve target column indices ONCE for the whole batch (was: per
        // value, per row). Empty = all columns in declared order.
        let n_cols = table.n_columns();
        let n_values_per_row = fi.values.first().map(|r| r.len()).unwrap_or(0);
        let col_indices: Vec<usize> = if fi.columns.is_empty() {
            if n_values_per_row != n_cols {
                return Err(Error::semantic(format!(
                    "table {} has {} columns but {} values were supplied",
                    table.name, n_cols, n_values_per_row
                )));
            }
            Vec::new()
        } else {
            if n_values_per_row != fi.columns.len() {
                return Err(Error::semantic(format!(
                    "{} VALUES for {} columns",
                    n_values_per_row,
                    fi.columns.len()
                )));
            }
            let mut seen: Vec<usize> = Vec::with_capacity(fi.columns.len());
            for name in fi.columns.iter() {
                match table.find_column(name) {
                    Some(idx) => {
                        if seen.contains(&idx) {
                            // Duplicate column — general path errors nicely.
                            return Ok(false);
                        }
                        seen.push(idx);
                    }
                    None => {
                        return Err(Error::semantic(format!(
                            "table {} has no column named {}",
                            table.name, name
                        )));
                    }
                }
            }
            seen
        };

        // Set up the execution context exactly like `execute` does.
        let in_txn = self.in_transaction.load(Ordering::Acquire);
        let txn_snap = self.txn_snapshot.get_mut().take();
        let deferred_flush = self.deferred_flush.load(Ordering::Acquire);
        let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
        let mut ctx = ExecContext::new(&self.pager, catalog_ptr);
        ctx.in_transaction = in_txn;
        ctx.deferred_flush = deferred_flush;
        ctx.txn_snapshot = txn_snap;
        // DETACH the combined maps (zero-copy): `execute`-family callers
        // hold `&mut self`, so no reader can hold a snapshot concurrently.
        ctx.shared = std::mem::replace(self.maps.get_mut(), empty_maps());
        let result = crate::executor::fast_insert_literal_rows(&mut ctx, &table, &col_indices, fi.values);

        // Epilogue: same write-backs as `execute` (merge into the
        // detached maps in place, then attach back).
        self.in_transaction.store(ctx.in_transaction, Ordering::Release);
        *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
        if let Ok(n) = result {
            crate::executor::change_counters::record(n);
        }
        // Merge overlays back regardless of success — a failed statement
        // may still have split a B+tree (page writes are not undone), so
        // dropping the root override would lose data. ROLLBACK is the only
        // path that legitimately discards them.
        if ctx.roots_changed {
            Arc::make_mut(&mut ctx.shared).roots.extend(ctx.root_overrides.drain());
        }
        if ctx.max_rowids_changed {
            Arc::make_mut(&mut ctx.shared).max_rowids.extend(ctx.max_rowids.drain());
        }
        if ctx.index_roots_changed {
            Arc::make_mut(&mut ctx.shared).index_roots.extend(ctx.index_roots.drain());
        }
        *self.maps.get_mut() = ctx.shared;
        if result.is_ok() && ctx.roots_changed {
            self.sync_schema_roots()?;
        }
        result?;
        // Auto-commit: flush outside explicit transactions (no-op in lazy
        // write-back / in-memory mode). With deferred flush enabled, honor
        // the dirty-page threshold exactly like the general path.
        if !in_txn && !deferred_flush {
            self.pager.flush()?;
        } else if deferred_flush && !in_txn {
            let dirty = self.pager.dirty_page_count();
            if dirty >= self.deferred_flush_threshold {
                let _ = self.pager.flush();
            }
        }
        Ok(true)
    }

    /// True when the statement carries a WITH clause — its plan embeds
    /// materialized CTE rows that must be recomputed per execution, so it
    /// can never come from (or go into) the statement cache.
    fn stmt_needs_cte_materialization(stmt: &Statement) -> bool {
        match stmt {
            Statement::Select(s) => s.with.is_some(),
            _ => false,
        }
    }

    fn get_or_cache_stmt(&self, sql: &str) -> Result<CachedStmt> {
        if self.stmt_cache_capacity == 0 {
            // Caching disabled — parse + plan every time.
            let t0 = profile::now();
            let stmt = parse(sql)?;
            profile::span(t0, &profile::PARSE_NS);
            let t1 = profile::now();
            let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
            profile::span(t1, &profile::PLAN_NS);
            let plan_arc = plan_opt.map(Arc::new);
            let has_subq = plan_arc
                .as_ref()
                .map(|p| crate::executor::plan_has_subqueries(p))
                .unwrap_or(false);
            let fast_path = plan_arc.as_ref().and_then(|p| Self::detect_fast_path(p)).map(Arc::new);
            return Ok(CachedStmt { stmt: Arc::new(stmt), plan: plan_arc, has_subqueries: has_subq, fast_path });
        }
        // Fast path: read lock — concurrent readers can hit the cache
        // simultaneously without serializing.
        {
            let cache = self.stmt_cache.read();
            if let Some(cached) = cache.get(sql) {
                // Clone the Arcs, NOT the Plan — for an INSERT plan with
                // Plan::Values { rows: Vec<Vec<Expr>> }, deep-cloning was
                // 3+ heap allocations per cache hit. Arc clone is one atomic
                // increment. For a 1k-statement INSERT batch, this saves
                // ~3k heap allocations.
                return Ok(cached.clone());
            }
        }
        // Miss: parse + plan. Then decide whether to populate the cache.
        let t0 = profile::now();
        let stmt = parse(sql)?;
        profile::span(t0, &profile::PARSE_NS);
        // WITH-clause statements are NEVER cached (and never hit): their
        // plans embed materialized CTE rows that must be recomputed per
        // execution. Planning here would also be wrong — the caller
        // materializes the CTEs first and plans with them in scope.
        if Self::stmt_needs_cte_materialization(&stmt) {
            return Ok(CachedStmt {
                stmt: Arc::new(stmt),
                plan: None,
                has_subqueries: false,
                fast_path: None,
            });
        }
        let t1 = profile::now();
        let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
        profile::span(t1, &profile::PLAN_NS);
        let plan_arc = plan_opt.map(Arc::new);
        let has_subq = plan_arc
            .as_ref()
            .map(|p| crate::executor::plan_has_subqueries(p))
            .unwrap_or(false);
        let fast_path = plan_arc.as_ref().and_then(|p| Self::detect_fast_path(p)).map(Arc::new);
        let entry = CachedStmt { stmt: Arc::new(stmt), plan: plan_arc, has_subqueries: has_subq, fast_path };
        let t2 = profile::now();
        // "Cache on second sight": only populate the cache for SQL text we
        // have seen before. A first sighting returns the freshly-parsed
        // statement WITHOUT inserting — populating the cache for one-off
        // statement text (literal-inlined values are the common case) is
        // pure overhead: ~1 µs of allocations + locking for an entry that
        // will never be hit before eviction.
        let h = quick_sql_hash(sql);
        let should_cache = {
            let mut seen = self.seen_hashes.lock();
            if seen.len() >= self.seen_hashes_cap {
                seen.clear();
            }
            // insert() returns false when the hash was already present —
            // i.e. this is (at least) the second sighting → cache it.
            !seen.insert(h)
        };
        if should_cache {
            let mut cache = self.stmt_cache.write();
            // Double-check: another thread may have inserted while we waited.
            if cache.get(sql).is_none() {
                // Evict FIFO if at capacity.
                if cache.len() >= self.stmt_cache_capacity {
                    let mut order = self.stmt_cache_order.lock();
                    if let Some(oldest) = order.first().cloned() {
                        cache.remove(&oldest);
                        order.remove(0);
                    }
                }
                cache.insert(sql.to_string(), entry.clone());
                self.stmt_cache_order.lock().push(sql.to_string());
            }
        }
        profile::span(t2, &profile::CACHE_NS);
        Ok(entry)
    }

    /// Persist root-page moves (from B+tree splits) into the schema rows.
    ///
    /// The catalog's `Arc<Table>`/`Arc<Index>` are immutable, so live roots
    /// are tracked in `root_overrides` / `index_roots`. The schema row of
    /// each object holds the root at CREATE time — if a split moved the
    /// root and we never rewrite that row, a REOPENED database would
    /// descend from the stale root and silently see only the first
    /// subtree's rows (e.g. 10k-row table visible as ~1.8k after reopen).
    ///
    /// Rewrites only happen when the live root differs from the value we
    /// last persisted (tracked in `schema_root_pages`), so the cost is one
    /// schema-row rewrite per actual split — O(1) amortized.
    fn sync_schema_roots(&self) -> Result<()> {
        let (tables, indexes): (Vec<(String, u32)>, Vec<(String, u32)>) = {
            let m = self.maps.read();
            (
                m.roots.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                m.index_roots.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            )
        };
        if tables.is_empty() && indexes.is_empty() {
            return Ok(());
        }
        let mut synced = self.schema_root_pages.lock();
        let mut dirty = false;
        for (name, root) in tables {
            let key = format!("table:{}", name);
            if synced.get(&key).copied() != Some(root) {
                self.rewrite_schema_row_root("table", &name, root)?;
                synced.insert(key, root);
                dirty = true;
            }
        }
        for (name, root) in indexes {
            let key = format!("index:{}", name);
            if synced.get(&key).copied() != Some(root) {
                self.rewrite_schema_row_root("index", &name, root)?;
                synced.insert(key, root);
                dirty = true;
            }
        }
        if dirty {
            // The schema rows changed — cached plans may hold stale roots.
            self.invalidate_stmt_cache();
            // Flush the rewritten schema rows UNLESS we're inside a
            // transaction (dirty pages must stay in cache until COMMIT so
            // ROLLBACK can discard them — flushing here broke
            // rollback semantics).
            if !self.in_transaction.load(Ordering::Acquire) {
                let _ = self.pager.flush();
            }
        }
        Ok(())
    }

    /// Rewrite one schema row's rootpage: read the existing row (preserving
    /// its name/table/sql columns), delete it, and insert the updated row.
    fn rewrite_schema_row_root(&self, kind: &str, name: &str, new_root: u32) -> Result<()> {
        let mut bt = Btree::new(&self.pager, 0, false);
        let mut found: Option<(i64, Vec<Value>)> = None;
        bt.scan_table(|rowid, payload| {
            if let Ok(row) = decode_row(payload, 5, 0, None) {
                if let Some((k, n, _, _, _)) = crate::schema::decode_schema_row(&row) {
                    if k == kind && n.eq_ignore_ascii_case(name) {
                        found = Some((rowid, row));
                        return false;
                    }
                }
            }
            true
        })?;
        drop(bt);
        if let Some((old_rowid, mut row)) = found {
            if row.len() >= 4 {
                row[3] = Value::Integer(new_root as i64);
            }
            let mut bt = Btree::new(&self.pager, 0, false);
            bt.delete_table(old_rowid)?;
            let payload = encode_row(&row);
            bt.insert_table(old_rowid, &payload)?;
        }
        Ok(())
    }

    /// Invalidate the statement cache. Called after any DDL statement
    /// (CREATE/DROP TABLE/INDEX/VIEW/TRIGGER) because the cached Plans hold
    /// `Arc<Table>` / `Arc<Index>` references that become stale when the
    /// schema changes.
    fn invalidate_stmt_cache(&self) {
        self.stmt_cache.write().clear();
        self.stmt_cache_order.lock().clear();
    }

    /// Execute a statement that does not return rows (INSERT/UPDATE/DELETE/CREATE/...).
    ///
    /// Takes `&mut self` because the outer `RwLock<Database>` write lock
    /// must be held to ensure single-writer semantics — but the actual state
    /// mutations go through interior mutability so the body could in principle
    /// be `&self`. We keep `&mut self` for API clarity (writers serialize).
    pub fn execute<P: Params>(&mut self, sql: &str, params: P) -> Result<()> {
        profile::COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // ---- FAST INSERT PATH ----
        // Single-row literal VALUES inserts are the hottest statement shape
        // in OLTP. A dedicated byte scanner recognizes them without the
        // tokenizer/parser/planner/statement-cache pipeline (~1.3 us per
        // statement of pure overhead). The scanner is conservative: any
        // deviation (UPSERT, RETURNING, non-literals, multi-row) falls
        // through to the general path below.
        {
            let first = sql.as_bytes().iter().find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
            if first == Some(&b'I') || first == Some(&b'i') {
                if let Some(fi) = try_fast_insert_parse(sql) {
                    if self.exec_fast_insert(fi)? {
                        return Ok(());
                    }
                }
            }
        }
        let is_ddl = is_ddl_sql(sql);
        let t_cache = profile::now();
        let cached = self.get_or_cache_stmt(sql)?;
        profile::span(t_cache, &profile::CACHE_NS);
        // WITH-clause SELECT via the execute path: same CTE machinery as
        // query(); the result rows are simply discarded.
        if let Statement::Select(sel) = cached.stmt.as_ref() {
            if sel.with.is_some() {
                let in_txn = self.in_transaction.load(Ordering::Acquire);
                let txn_snap = self.txn_snapshot.get_mut().take();
                let deferred_flush = self.deferred_flush.load(Ordering::Acquire);
                let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
                let mut ctx = ExecContext::new(&self.pager, catalog_ptr);
                ctx.in_transaction = in_txn;
                ctx.deferred_flush = deferred_flush;
                ctx.txn_snapshot = txn_snap;
                ctx.shared = std::mem::replace(self.maps.get_mut(), empty_maps());
                for v in params.into_iter() {
                    ctx.bind_positional(v);
                }
                let res = self.exec_select_with_ctes(&mut ctx, sel, &HashMap::new());
                self.in_transaction.store(ctx.in_transaction, Ordering::Release);
                *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
                if ctx.max_rowids_changed {
                    Arc::make_mut(&mut ctx.shared).max_rowids.extend(ctx.max_rowids.drain());
                }
                *self.maps.get_mut() = ctx.shared;
                res?;
                return Ok(());
            }
        }
        // Deref the Arc<Statement> to a &Statement for execute_statement_static.
        // (The Arc itself stays alive on the stack for the duration of the call.)
        let stmt_ref: &Statement = &cached.stmt;
        let plan_opt = cached.plan;
        let in_txn = self.in_transaction.load(Ordering::Acquire);
        let txn_snap = self.txn_snapshot.get_mut().take();
        let deferred_flush = self.deferred_flush.load(Ordering::Acquire);
        let deferred_flush_threshold = self.deferred_flush_threshold;
        // Move root_overrides/max_rowids out of the RwLock into a local; we'll
        // write them back after the statement completes. The write lock on the
        // outer `RwLock<Database>` (held by the caller's `execute(&mut self)`)
        // guarantees exclusive access, so we can safely move.
        //
        // OPTIMIZATION: use `RwLock::get_mut()` instead of `.write()` to skip
        // the lock acquisition. Since we have `&mut self`, we have exclusive
        // access — the lock is redundant. For a 1k-row INSERT batch (each
        // statement goes through this path), that's 4k avoided lock
        // acquisitions on `root_overrides` + `max_rowids`.
        let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
        let mut ctx = ExecContext::new(&self.pager, catalog_ptr);
        ctx.in_transaction = in_txn;
        ctx.deferred_flush = deferred_flush;
        ctx.txn_snapshot = txn_snap;
        // DETACH the combined maps (zero-copy): `execute` holds `&mut self`,
        // so no reader can hold a snapshot concurrently. The statement owns
        // the maps via `ctx.shared` and hands them back in the epilogue.
        // (Cloning the Arc snapshot here instead would keep a second
        // refcount alive and force `Arc::make_mut` to deep-copy the maps
        // on every write-back — a ~30% insert regression.)
        ctx.shared = std::mem::replace(self.maps.get_mut(), empty_maps());
        for v in params.into_iter() {
            ctx.bind_positional(v);
        }
        // Use the cached plan if available — execute_statement_static
        // otherwise re-parses + re-plans, which for a 1k-row INSERT batch
        // means 1k wasted planning passes (each builds a Plan::Insert
        // containing Plan::Values { Vec<Vec<Expr>> }, several heap
        // allocations). With the cached plan, we skip all of that.
        let t_exec = profile::now();
        let result = if let Some(plan) = plan_opt {
            // Substitute uncorrelated subqueries (scalar / IN / EXISTS) with
            // their materialized results before execution — mirrors
            // SQLite's OP_Once evaluation. Skipped entirely (zero cost)
            // when the plan has no subquery expressions (flag precomputed
            // at plan time — no per-statement plan walk).
            let plan_local;
            let plan_ref: &crate::planner::plan::Plan = if cached.has_subqueries {
                plan_local = crate::executor::rewrite_plan_subqueries(&plan, &mut ctx)?;
                &plan_local
            } else {
                &plan
            };
            // Execute the (possibly rewritten) plan directly. Only Insert/Update/Delete/
            // Select produce plans (DDL returns None), so this branch only
            // fires for those statement types.
            execute(plan_ref, &mut ctx).map(|_| ())
        } else {
            // No cached plan — DDL statement. execute_statement_static
            // handles DDL directly (CREATE/DROP/ALTER/ATTACH/DETACH/
            // VACUUM/PRAGMA) without a Plan.
            Self::execute_statement_static(stmt_ref, &mut ctx, &mut self.catalog, sql)
        };
        profile::span(t_exec, &profile::EXEC_NS);
        self.in_transaction.store(ctx.in_transaction, Ordering::Release);
        *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
        crate::executor::change_counters::record(ctx.changes);
        // Merge local overlay entries into the DETACHED maps (in place —
        // the statement is the sole owner, so make_mut never clones) and
        // attach them back to the Database. Merge regardless of `result`:
        // a failed statement may still have split a B+tree (page writes
        // are not undone by error propagation); ROLLBACK is the only path
        // that legitimately discards them.
        if ctx.roots_changed {
            Arc::make_mut(&mut ctx.shared).roots.extend(ctx.root_overrides.drain());
        }
        if ctx.max_rowids_changed {
            Arc::make_mut(&mut ctx.shared).max_rowids.extend(ctx.max_rowids.drain());
        }
        if ctx.index_roots_changed {
            Arc::make_mut(&mut ctx.shared).index_roots.extend(ctx.index_roots.drain());
        }
        *self.maps.get_mut() = ctx.shared;
        if result.is_ok() && ctx.rolled_back {
            // ROLLBACK discarded in-transaction schema-row rewrites; reset
            // the persisted-root map to the catalog's (CREATE-time) values,
            // which match the rolled-back file. The shared root/max-rowid
            // snapshots may also hold stale entries from the transaction —
            // clear them so the next statement rescans.
            let mut synced = self.schema_root_pages.lock();
            synced.clear();
            for (name, t) in self.catalog.all_tables() {
                synced.insert(format!("table:{}", name), t.root_page);
            }
            for (name, i) in self.catalog.all_indexes() {
                synced.insert(format!("index:{}", name), i.root_page);
            }
            *self.maps.get_mut() = empty_maps();
        }
        // Persist any root-page moves (B+tree splits) to the schema rows so
        // a reopened database sees the full tree. Without this, every table
        // or index that split lost all data beyond the stale root on reopen.
        // Gated on ctx.roots_changed — roots only move on splits, which are
        // rare; previously this ran (two read locks + two Vec<(String,u32)>
        // collects with String clones) after EVERY statement.
        if result.is_ok() && ctx.roots_changed {
            self.sync_schema_roots()?;
        }
        // DDL changes the schema → cached plans hold stale Arc<Table>/Arc<Index>.
        if result.is_ok() && is_ddl {
            self.invalidate_stmt_cache();
        }
        // If deferred_flush is enabled, force a flush when the dirty-page
        // count exceeds the threshold.
        if result.is_ok() && deferred_flush {
            let dirty = self.pager.dirty_page_count();
            if dirty >= deferred_flush_threshold {
                let _ = self.pager.flush();
            }
        }
        result
    }

    /// Execute a query and return all rows.
    ///
    /// Takes `&self` — concurrent readers can call this simultaneously when
    /// the outer `Arc<RwLock<Database>>` is held with a read lock. All mutations
    /// required (cache miss insert, page cache fill on miss) go through
    /// interior mutability on `stmt_cache` and `pager`.
    ///
    /// `root_overrides` and `max_rowids` are snapshot-cloned (read lock) for
    /// the duration of the query — a SELECT never writes them back.
    pub fn query<P: Params>(&self, sql: &str, params: P) -> Result<Vec<Row>> {
        // In deferred_flush mode, a SELECT must see all writes that
        // happened since the last flush.
        if self.deferred_flush.load(Ordering::Acquire) && self.pager.has_dirty_pages() {
            let _ = self.pager.flush();
        }
        let cached = self.get_or_cache_stmt(sql)?;
        // WITH-clause statements: materialize CTEs, plan with them in
        // scope, execute — never cached (rows are recomputed per call).
        if let Statement::Select(sel) = cached.stmt.as_ref() {
            if sel.with.is_some() {
                let in_txn = self.in_transaction.load(Ordering::Acquire);
                let txn_snap = if in_txn {
                    self.txn_snapshot.lock().clone()
                } else {
                    None
                };
                let shared = self.maps.read().clone();
                let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
                let mut ctx = ExecContext::new_reader(&self.pager, catalog_ptr, shared);
                ctx.in_transaction = in_txn;
                ctx.deferred_flush = self.deferred_flush.load(Ordering::Acquire);
                ctx.txn_snapshot = txn_snap;
                for v in params.into_iter() {
                    ctx.bind_positional(v);
                }
                let res = self.exec_select_with_ctes(&mut ctx, sel, &HashMap::new())?;
                return Ok(res.rows);
            }
        }
        if let Some(plan) = cached.plan {
            // Pre-compiled point-lookup fast path: skips the ExecContext /
            // EvalContext / Plan dispatch entirely. Only fires for the
            // exact shapes detected at cache time (bare-column projections
            // over a rowid / index point lookup).
            if let Some(fp) = &cached.fast_path {
                let params_v: Vec<Value> = params.into_iter().collect();
                let rows = self.run_fast_path(fp, &params_v)?;
                return Ok(rows);
            }
            let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
            let in_txn = self.in_transaction.load(Ordering::Acquire);
            // Skip the txn-snapshot Mutex entirely when not inside a
            // transaction — the snapshot is only ever Some(...) between
            // BEGIN and COMMIT/ROLLBACK, and the lock was ~15-20 ns on
            // every single query outside that window.
            let txn_snap = if in_txn {
                self.txn_snapshot.lock().clone()
            } else {
                None
            };
            // ONE read-lock + ONE refcount bump for all three bookkeeping
            // maps (was three separate `RwLock<Arc<HashMap>>` reads).
            let shared = self.maps.read().clone();
            let mut ctx = ExecContext::new_reader(&self.pager, catalog_ptr, shared);
            ctx.in_transaction = in_txn;
            ctx.deferred_flush = self.deferred_flush.load(Ordering::Acquire);
            ctx.txn_snapshot = txn_snap;
            for v in params.into_iter() {
                ctx.bind_positional(v);
            }
            // Substitute uncorrelated subqueries before execution (flag
            // precomputed at plan time — no per-query plan walk).
            let plan_local;
            let plan_ref: &crate::planner::plan::Plan = if cached.has_subqueries {
                plan_local = crate::executor::rewrite_plan_subqueries(&plan, &mut ctx)?;
                &plan_local
            } else {
                &plan
            };
            let res = execute(plan_ref, &mut ctx)?;
            // For SELECT, root_overrides/max_rowids don't change. For DML
            // with RETURNING (INSERT..RETURNING etc. via the query path),
            // the root pages may have moved due to B+tree splits — write
            // them back so subsequent statements see the new roots.
            if matches!(
                plan.as_ref(),
                crate::planner::plan::Plan::Insert { .. }
                    | crate::planner::plan::Plan::Update { .. }
                    | crate::planner::plan::Plan::Delete { .. }
            ) {
                // DML via the query path (e.g. INSERT..RETURNING): merge
                // any root moves / max-rowid updates back.
                if ctx.roots_changed || ctx.max_rowids_changed || ctx.index_roots_changed {
                    let mut m = self.maps.write();
                    let bk = Arc::make_mut(&mut *m);
                    if ctx.roots_changed {
                        bk.roots.extend(ctx.root_overrides.drain());
                    }
                    if ctx.max_rowids_changed {
                        bk.max_rowids.extend(ctx.max_rowids.drain());
                    }
                    if ctx.index_roots_changed {
                        bk.index_roots.extend(ctx.index_roots.drain());
                    }
                }
                self.sync_schema_roots()?;
            } else if ctx.max_rowids_changed {
                // Pure SELECTs can still populate the max-rowid scan cache
                // (used by the index-range merge-scan heuristic) — merge it
                // back so repeated queries don't rescan.
                let mut m = self.maps.write();
                Arc::make_mut(&mut *m).max_rowids.extend(ctx.max_rowids.drain());
            }
            Ok(res.rows)
        } else {
            Ok(Vec::new())
        }
    }

    /// Alias for `query()` — for use when callers want to emphasize that
    /// they're sharing a `&Database` reference across threads.
    pub fn query_shared<P: Params>(&self, sql: &str, params: P) -> Result<Vec<Row>> {
        self.query(sql, params)
    }

    /// Execute a query and return (column_names, rows).
    ///
    /// Takes `&self` — concurrent readers can call this simultaneously.
    pub fn query_with_columns<P: Params>(&self, sql: &str, params: P) -> Result<(Vec<String>, Vec<Row>)> {
        if self.deferred_flush.load(Ordering::Acquire) && self.pager.has_dirty_pages() {
            let _ = self.pager.flush();
        }
        let cached = self.get_or_cache_stmt(sql)?;
        if let Statement::Select(sel) = cached.stmt.as_ref() {
            if sel.with.is_some() {
                let in_txn = self.in_transaction.load(Ordering::Acquire);
                let txn_snap = if in_txn {
                    self.txn_snapshot.lock().clone()
                } else {
                    None
                };
                let shared = self.maps.read().clone();
                let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
                let mut ctx = ExecContext::new_reader(&self.pager, catalog_ptr, shared);
                ctx.in_transaction = in_txn;
                ctx.deferred_flush = self.deferred_flush.load(Ordering::Acquire);
                ctx.txn_snapshot = txn_snap;
                for v in params.into_iter() {
                    ctx.bind_positional(v);
                }
                let res = self.exec_select_with_ctes(&mut ctx, sel, &HashMap::new())?;
                return Ok((res.columns.to_vec(), res.rows));
            }
        }
        if let Some(plan) = cached.plan {
            // Pre-compiled point-lookup fast path (see query()).
            if let Some(fp) = &cached.fast_path {
                let params_v: Vec<Value> = params.into_iter().collect();
                let rows = self.run_fast_path(fp, &params_v)?;
                return Ok((fp.output_columns().to_vec(), rows));
            }
            let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
            let in_txn = self.in_transaction.load(Ordering::Acquire);
            let txn_snap = if in_txn {
                self.txn_snapshot.lock().clone()
            } else {
                None
            };
            // ONE read-lock + ONE refcount bump (see query()).
            let shared = self.maps.read().clone();
            let mut ctx = ExecContext::new_reader(&self.pager, catalog_ptr, shared);
            ctx.in_transaction = in_txn;
            ctx.deferred_flush = self.deferred_flush.load(Ordering::Acquire);
            ctx.txn_snapshot = txn_snap;
            for v in params.into_iter() {
                ctx.bind_positional(v);
            }
            // Substitute uncorrelated subqueries before execution (flag
            // precomputed at plan time — no per-query plan walk).
            let plan_local;
            let plan_ref: &crate::planner::plan::Plan = if cached.has_subqueries {
                plan_local = crate::executor::rewrite_plan_subqueries(&plan, &mut ctx)?;
                &plan_local
            } else {
                &plan
            };
            let res = execute(plan_ref, &mut ctx)?;
            if matches!(
                plan.as_ref(),
                crate::planner::plan::Plan::Insert { .. }
                    | crate::planner::plan::Plan::Update { .. }
                    | crate::planner::plan::Plan::Delete { .. }
            ) {
                if ctx.roots_changed || ctx.max_rowids_changed || ctx.index_roots_changed {
                    let mut m = self.maps.write();
                    let bk = Arc::make_mut(&mut *m);
                    if ctx.roots_changed {
                        bk.roots.extend(ctx.root_overrides.drain());
                    }
                    if ctx.max_rowids_changed {
                        bk.max_rowids.extend(ctx.max_rowids.drain());
                    }
                    if ctx.index_roots_changed {
                        bk.index_roots.extend(ctx.index_roots.drain());
                    }
                }
                self.sync_schema_roots()?;
            } else if ctx.max_rowids_changed {
                // Pure SELECTs can still populate the max-rowid scan cache
                // (used by the index-range merge-scan heuristic) — merge it
                // back so repeated queries don't rescan.
                let mut m = self.maps.write();
                Arc::make_mut(&mut *m).max_rowids.extend(ctx.max_rowids.drain());
            }
            Ok((res.columns.to_vec(), res.rows))
        } else {
            Ok((Vec::new(), Vec::new()))
        }
    }

    /// Get the last inserted rowid.
    pub fn last_insert_rowid(&self) -> i64 {
        // The ExecContext owns this; we'd need to expose it. For now, return 0.
        // A real impl would track this on `Database`.
        0
    }

    /// Number of pages in the database file.
    pub fn page_count(&self) -> u32 {
        self.pager.n_pages()
    }

    /// Page size in bytes.
    pub fn page_size(&self) -> u32 {
        self.pager.page_size()
    }

    /// Cache statistics.
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.pager.cache_size(), self.pager.cache_capacity())
    }

    /// Path to the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get a reference to the catalog (for debugging/testing).
    pub fn catalog_ref(&self) -> &Catalog {
        &self.catalog
    }

    /// Get a mutable pointer to the pager (for debugging/testing).
    pub fn pager_mut(&mut self) -> *mut Pager {
        &mut self.pager as *mut Pager
    }

    // ====================================================================
    // CTE (WITH clause) materialization
    // ====================================================================

    /// Execute a SELECT statement with a set of pre-materialized CTEs in
    /// scope. Returns the ExecResult (columns + rows).
    fn exec_select_with_ctes(
        &self,
        ctx: &mut ExecContext<'_>,
        select: &SelectStatement,
        outer_ctes: &HashMap<String, (Arc<Vec<Row>>, Arc<[String]>)>,
    ) -> Result<crate::executor::ExecResult> {
        // Materialize THIS select's own WITH clause (nested WITH), layered
        // on top of the outer map (inner names shadow outer names).
        let cte_map = if let Some(with) = &select.with {
            let mut m = outer_ctes.clone();
            let own = self.materialize_ctes(with, &m, ctx)?;
            m.extend(own);
            m
        } else {
            outer_ctes.clone()
        };
        let mut planner = Planner::new(&self.catalog);
        planner.set_ctes(cte_map.clone());
        let plan = planner.plan_select(select)?;
        let mut plan = plan;
        // Make the CTEs visible to subquery planning inside this statement.
        ctx.ctes = Some(cte_map);
        // Uncorrelated subquery substitution (same as the general path).
        if crate::executor::plan_has_subqueries(&plan) {
            plan = crate::executor::rewrite_plan_subqueries(&plan, ctx)?;
        }
        let res = crate::executor::execute(&plan, ctx);
        ctx.ctes = None;
        res
    }

    /// Materialize every CTE of a WITH clause into (rows, qualified column
    /// names). Later CTEs see earlier ones; WITH RECURSIVE iterates the
    /// recursive arm until no new rows appear.
    fn materialize_ctes(
        &self,
        with: &WithClause,
        outer_ctes: &HashMap<String, (Arc<Vec<Row>>, Arc<[String]>)>,
        ctx: &mut ExecContext<'_>,
    ) -> Result<HashMap<String, (Arc<Vec<Row>>, Arc<[String]>)>> {
        let mut map: HashMap<String, (Arc<Vec<Row>>, Arc<[String]>)> = outer_ctes.clone();
        for cte in &with.ctes {
            let name_lc = cte.name.to_ascii_lowercase();
            let (rows, cols) = if with.recursive {
                self.materialize_recursive_cte(cte, &map, ctx)?
            } else {
                let res = self.exec_select_with_ctes(ctx, &cte.select, &map)?;
                (res.rows, res.columns)
            };
            // Apply the explicit column list (WITH name(a, b) AS ...) — the
            // rename happens at the CTE boundary.
            let cols: Arc<[String]> = match &cte.columns {
                Some(list) if list.len() == cols.len() => list
                    .iter()
                    .map(|c| format!("{}.{}", cte.name, c))
                    .collect::<Vec<String>>()
                    .into(),
                Some(list) => {
                    return Err(Error::semantic(format!(
                        "CTE {} declares {} columns but its SELECT produces {}",
                        cte.name,
                        list.len(),
                        cols.len()
                    )));
                }
                None => {
                    // Qualify with the CTE name so `cte.col` references
                    // resolve; unqualified refs match by suffix.
                    cols.iter()
                        .map(|c| {
                            let suffix = c.rsplit('.').next().unwrap_or(c);
                            format!("{}.{}", cte.name, suffix)
                        })
                        .collect::<Vec<String>>()
                        .into()
                }
            };
            map.insert(name_lc, (Arc::new(rows), cols));
        }
        Ok(map)
    }

    /// WITH RECURSIVE: the CTE body is `base UNION [ALL] recursive`. The
    /// base arm executes once; the recursive arm (which references the CTE
    /// by name) executes repeatedly, each time seeing ALL rows accumulated
    /// so far, until it produces no new rows. UNION dedups against the
    /// accumulated set; UNION ALL appends everything. A hard iteration cap
    /// guards against non-terminating recursions (SQLite errors too).
    fn materialize_recursive_cte(
        &self,
        cte: &Cte,
        outer_ctes: &HashMap<String, (Arc<Vec<Row>>, Arc<[String]>)>,
        ctx: &mut ExecContext<'_>,
    ) -> Result<(Vec<Row>, Arc<[String]>)> {
        let name_lc = cte.name.to_ascii_lowercase();
        // Split the compound body: the LAST UNION [ALL] arm is the
        // recursive one; everything before it is the base. (SQLite's rule:
        // exactly one recursive reference, in the arm after the UNION.)
        let (base, recursive_op, recursive_arm) = split_compound_cte(&cte.select)?;
        // Base: execute with the CTE visible but EMPTY (a recursive
        // reference in the base arm is an error in SQLite; empty keeps it
        // simple and correct for well-formed queries).
        let mut scope = outer_ctes.clone();
        scope.insert(
            name_lc.clone(),
            (Arc::new(Vec::new()), Arc::from(vec![format!("{}.", cte.name)])),
        );
        let base_res = self.exec_select_with_ctes(ctx, &base, &scope)?;
        let mut rows: Vec<Row> = base_res.rows;
        let base_cols: Arc<[String]> = base_res.columns;
        // Canonical output columns for the CTE.
        let out_cols: Arc<[String]> = match &cte.columns {
            Some(list) if list.len() == base_cols.len() => list
                .iter()
                .map(|c| format!("{}.{}", cte.name, c))
                .collect::<Vec<String>>()
                .into(),
            Some(list) => {
                return Err(Error::semantic(format!(
                    "CTE {} declares {} columns but its SELECT produces {}",
                    cte.name,
                    list.len(),
                    base_cols.len()
                )));
            }
            None => base_cols
                .iter()
                .map(|c| {
                    let suffix = c.rsplit('.').next().unwrap_or(c);
                    format!("{}.{}", cte.name, suffix)
                })
                .collect::<Vec<String>>()
                .into(),
        };
        // Dedup set for UNION semantics.
        let mut seen: std::collections::HashSet<String> = rows
            .iter()
            .map(|r| format!("{:?}", r))
            .collect();
        // Iterate the recursive arm with QUEUE semantics (SQLite's model):
        // each iteration's arm sees ONLY the rows produced by the PREVIOUS
        // iteration (the frontier), not the full accumulation — otherwise
        // UNION ALL recursions never terminate (every iteration re-derives
        // rows that are "new" again as duplicates).
        let mut frontier: Vec<Row> = rows.clone();
        const MAX_ITERS: usize = 1_000_000;
        for _ in 0..MAX_ITERS {
            if rows.len() > 10_000_000 {
                return Err(Error::semantic(format!(
                    "recursive CTE {} exceeded 10,000,000 rows",
                    cte.name
                )));
            }
            if frontier.is_empty() {
                break;
            }
            let mut scope = outer_ctes.clone();
            scope.insert(name_lc.clone(), (Arc::new(frontier), out_cols.clone()));
            let new_res = self.exec_select_with_ctes(ctx, &recursive_arm, &scope)?;
            let mut next_frontier: Vec<Row> = Vec::with_capacity(new_res.rows.len());
            for r in new_res.rows {
                if recursive_op == crate::sql::ast::SetOp::Union {
                    let key = format!("{:?}", r);
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key);
                }
                next_frontier.push(r.clone());
                rows.push(r);
            }
            frontier = next_frontier;
        }
        Ok((rows, out_cols))
    }

    fn plan_for_statement(catalog: &Catalog, stmt: &Statement) -> Result<Option<crate::planner::plan::Plan>> {
        match stmt {
            Statement::Select(s) => {
                let mut planner = Planner::new(catalog);
                Ok(Some(planner.plan_select(s)?))
            }
            Statement::Insert(_) => Ok(Some(Self::plan_insert(catalog, stmt)?)),
            Statement::Update(_) => Ok(Some(Self::plan_update(catalog, stmt)?)),
            Statement::Delete(_) => Ok(Some(Self::plan_delete(catalog, stmt)?)),
            _ => Ok(None),
        }
    }

    /// Detect a pre-compilable point-lookup fast path in a plan (see
    /// `FastPath`). Conservative: any shape other than
    /// `Project(RowidLookup)` / `Project(IndexLookup)` with bare-column
    /// projections falls back to the general pipeline.
    fn detect_fast_path(plan: &Plan) -> Option<FastPath> {
        /// Resolve a projection list to table column indices. Returns None
        /// unless EVERY projection expr is a bare column of the table.
        /// `project == None` in the result means identity (all columns,
        /// in table order) — used for `SELECT *` / `SELECT t.*`.
        fn resolve_projection(
            columns: &[crate::planner::plan::ProjectExpr],
            table: &Table,
        ) -> Option<(Option<Vec<usize>>, Arc<[String]>)> {
            // `SELECT *` / `SELECT t.*` plans as a single pseudo-column
            // named "*" — identity projection, decode the full row.
            if columns.len() == 1 {
                if let Expr::Column { name, .. } = &columns[0].expr {
                    if name == "*" {
                        return Some((None, table.col_names.clone()));
                    }
                }
            }
            let mut idxs = Vec::with_capacity(columns.len());
            let mut names: Vec<String> = Vec::with_capacity(columns.len());
            for pe in columns {
                match &pe.expr {
                    Expr::Column { name, .. } => {
                        let idx = table.find_column(name)?;
                        idxs.push(idx);
                        names.push(pe.alias.clone().unwrap_or_else(|| table.columns[idx].name.clone()));
                    }
                    _ => return None,
                }
            }
            Some((Some(idxs), names.into()))
        }

        /// Bind an expression used as a lookup key: positional parameter
        /// (`?`/`?N` — numeric names) or literal.
        fn bind_expr(e: &Expr) -> Option<FastBound> {
            match e {
                Expr::Parameter(p) => p.parse::<usize>().ok().map(FastBound::Param),
                Expr::Literal(v) => Some(FastBound::Literal(v.clone())),
                _ => None,
            }
        }

        match plan {
            // Bare `SELECT * FROM t WHERE id = ?` (no Project node).
            Plan::RowidLookup { table, rowid, .. } => {
                let rowid = bind_expr(rowid)?;
                Some(FastPath::RowidPoint {
                    table: table.clone(),
                    rowid,
                    project: None,
                    columns: table.col_names.clone(),
                })
            }
            // Bare `SELECT * FROM t WHERE id BETWEEN ? AND ?`.
            Plan::RowidRange { table, start: Some(s), end: Some(e), residual: None, .. } => {
                let start = bind_expr(s)?;
                let end = bind_expr(e)?;
                Some(FastPath::RowidRange {
                    table: table.clone(),
                    start,
                    end,
                    project: None,
                    columns: table.col_names.clone(),
                })
            }
            Plan::Project { input, columns } => match input.as_ref() {
                Plan::RowidLookup { table, rowid, .. } => {
                    let rowid = bind_expr(rowid)?;
                    let (project, cols) = resolve_projection(columns, table)?;
                    Some(FastPath::RowidPoint {
                        table: table.clone(),
                        rowid,
                        project,
                        columns: cols,
                    })
                }
                Plan::IndexLookup { table, index, key_exprs, .. } => {
                    let keys = key_exprs.iter().map(bind_expr).collect::<Option<Vec<_>>>()?;
                    let (project, cols) = resolve_projection(columns, table)?;
                    Some(FastPath::IndexPoint {
                        table: table.clone(),
                        index: index.clone(),
                        keys,
                        project,
                        columns: cols,
                    })
                }
                Plan::RowidRange { table, start: Some(s), end: Some(e), residual: None, .. } => {
                    let start = bind_expr(s)?;
                    let end = bind_expr(e)?;
                    let (project, cols) = resolve_projection(columns, table)?;
                    Some(FastPath::RowidRange {
                        table: table.clone(),
                        start,
                        end,
                        project,
                        columns: cols,
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Execute a pre-compiled point-lookup fast path: B+tree descent +
    /// selective decode, with NO ExecContext / EvalContext / Plan dispatch.
    /// Semantics identical to the general path (same `lookup_table` /
    /// `lookup_index` / `decode_row*` calls, same `.as_integer()` binding).
    fn run_fast_path(&self, fp: &FastPath, params: &[Value]) -> Result<Vec<Row>> {
        // One combined snapshot read for root overrides (tables + indexes).
        // Cheap: usually the maps are empty and this is a failed lookup on
        // an empty HashMap behind one read lock.
        let (table_root, index_root) = {
            let m = self.maps.read();
            let name = fp.table_name();
            let t = m.roots.get(name).copied()
                .or_else(|| m.roots.get(&name.to_ascii_lowercase()).copied());
            let i = match fp {
                FastPath::IndexPoint { index, .. } => m.index_roots.get(&index.name).copied()
                    .or_else(|| m.index_roots.get(&index.name.to_ascii_lowercase()).copied()),
                _ => None,
            };
            (t, i)
        };
        match fp {
            FastPath::RowidPoint { table, rowid, project, columns: _ } => {
                let rid = rowid.resolve(params).as_integer();
                let root = table_root.unwrap_or(table.root_page);
                let mut bt = Btree::new(&self.pager, root, false);
                match bt.lookup_table(rid)? {
                    LookupResult::Found(payload) => {
                        let row = decode_projected(&payload, table, rid, project.as_deref())?;
                        Ok(vec![row])
                    }
                    LookupResult::NotFound => Ok(Vec::new()),
                }
            }
            FastPath::IndexPoint { table, index, keys, project, columns: _ } => {
                // Encode the key (same order-preserving encoding as the
                // general path's exec_index_lookup).
                let mut key_bytes = Vec::with_capacity(keys.len() * 8);
                for k in keys {
                    k.resolve(params).encode_order_key_into(&mut key_bytes);
                }
                let iroot = index_root.unwrap_or(index.root_page);
                let mut ibt = Btree::new(&self.pager, iroot, true);
                let rowids = ibt.lookup_index(&key_bytes)?;
                if rowids.is_empty() {
                    return Ok(Vec::new());
                }
                let troot = table_root.unwrap_or(table.root_page);
                let mut tbt = Btree::new(&self.pager, troot, false);
                let mut rows = Vec::with_capacity(rowids.len());
                for rid in rowids {
                    if let LookupResult::Found(payload) = tbt.lookup_table(rid)? {
                        rows.push(decode_projected(&payload, table, rid, project.as_deref())?);
                    }
                }
                Ok(rows)
            }
            FastPath::RowidRange { table, start, end, project, columns: _ } => {
                let lo = start.resolve(params).as_integer();
                let hi = end.resolve(params).as_integer();
                let root = table_root.unwrap_or(table.root_page);
                let mut bt = Btree::new(&self.pager, root, false);
                let mut rows = Vec::new();
                bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
                    // Match the general path: skip undecodable rows.
                    if let Ok(row) = decode_projected(payload, table, rowid, project.as_deref()) {
                        rows.push(row);
                    }
                    true
                })?;
                Ok(rows)
            }
        }
    }

    pub(crate) fn plan_insert(catalog: &Catalog, stmt: &Statement) -> Result<crate::planner::plan::Plan> {
        let ins = match stmt {
            Statement::Insert(i) => i,
            _ => unreachable!(),
        };
        let table = catalog.get_table(&ins.table).ok_or_else(|| Error::NotFound(format!("table: {}", ins.table)))?;
        // Plan the source.
        let source_plan = match &ins.source {
            InsertSource::Values(rows) => {
                let plan = crate::planner::plan::Plan::Values { rows: rows.clone() };
                plan
            }
            InsertSource::Select(s) => {
                let mut planner = Planner::new(catalog);
                planner.plan_select(s)?
            }
            InsertSource::DefaultValues => {
                crate::planner::plan::Plan::Values { rows: vec![vec![]] }
            }
        };
        let columns: Option<Vec<usize>> = if let Some(cols) = &ins.columns {
            let mut v = Vec::with_capacity(cols.len());
            for c in cols {
                let idx = table.find_column(c).ok_or_else(|| Error::semantic(format!("column {} not in table {}", c, table.name)))?;
                v.push(idx);
            }
            Some(v)
        } else {
            None
        };
        let on_conflict = ins.or.unwrap_or(ConflictResolution::Abort);
        Ok(crate::planner::plan::Plan::Insert {
            table,
            source: Box::new(source_plan),
            columns,
            on_conflict,
            upsert: ins.upsert.clone(),
            returning: ins.returning.clone(),
        })
    }

    pub(crate) fn plan_update(catalog: &Catalog, stmt: &Statement) -> Result<crate::planner::plan::Plan> {
        let upd = match stmt {
            Statement::Update(u) => u,
            _ => unreachable!(),
        };
        let table = catalog.get_table(&upd.table).ok_or_else(|| Error::NotFound(format!("table: {}", upd.table)))?;
        let scan = crate::planner::plan::Plan::Scan {
            table: table.clone(),
            alias: upd.alias.clone(),
            index: None,
            predicate: None,
        };
        // Use apply_where_for_scan so that `UPDATE t SET ... WHERE id = ?`
        // picks RowidLookup instead of a full table scan. Previously this
        // built a Filter{Scan, predicate} which forced an O(n) scan per
        // UPDATE — a ~743x regression on the UPDATE-by-PK benchmark.
        let source = if let Some(pred) = &upd.where_clause {
            crate::planner::apply_where_for_scan(catalog, scan, pred)
        } else {
            scan
        };
        let assignments: Vec<(usize, Expr)> = upd.set.iter().map(|(col, expr)| {
            let idx = table.find_column(col).unwrap_or(0);
            (idx, expr.clone())
        }).collect();
        Ok(crate::planner::plan::Plan::Update {
            table,
            source: Box::new(source),
            assignments,
            returning: upd.returning.clone(),
        })
    }

    pub(crate) fn plan_delete(catalog: &Catalog, stmt: &Statement) -> Result<crate::planner::plan::Plan> {
        let del = match stmt {
            Statement::Delete(d) => d,
            _ => unreachable!(),
        };
        let table = catalog.get_table(&del.from).ok_or_else(|| Error::NotFound(format!("table: {}", del.from)))?;
        let scan = crate::planner::plan::Plan::Scan {
            table: table.clone(),
            alias: del.alias.clone(),
            index: None,
            predicate: None,
        };
        let source = if let Some(pred) = &del.where_clause {
            // Same fix as plan_update: route through apply_where_for_scan so
            // `DELETE FROM t WHERE id = ?` uses RowidLookup, not a full scan.
            crate::planner::apply_where_for_scan(catalog, scan, pred)
        } else {
            scan
        };
        Ok(crate::planner::plan::Plan::Delete {
            table,
            source: Box::new(source),
            returning: del.returning.clone(),
        })
    }

    fn execute_statement_static(stmt: &Statement, ctx: &mut ExecContext, catalog: &mut Catalog, original_sql: &str) -> Result<()> {
        match stmt {
            Statement::Create(c) => Self::execute_create(c.clone(), ctx, catalog, original_sql),
            Statement::Drop(d) => Self::execute_drop(d.clone(), ctx, catalog),
            Statement::Begin(_) => {
                // Snapshot the pager's mutable state NOW so ROLLBACK can
                // restore to this point. We also flip in_transaction so the
                // executor's INSERT/UPDATE/DELETE skip per-statement flushes
                // (so dirty pages stay in cache only, never reaching disk).
                //
                // In lazy write-back mode (in-memory DBs), flush() doesn't
                // write dirty pages — but ROLLBACK restores by CLEARING the
                // cache and reading pages back from the file, which requires
                // the file to hold the pre-BEGIN state. So at BEGIN we force
                // one real write-back of all dirty pages (BEGIN is rare
                // compared to autocommit statements, so this is cheap
                // amortized) and THEN take the snapshot.
                ctx.pager.flush_before_snapshot()?;
                ctx.in_transaction = true;
                ctx.txn_snapshot = Some(ctx.pager.snapshot());
                Ok(())
            }
            Statement::Commit => {
                ctx.in_transaction = false;
                ctx.txn_snapshot = None;
                ctx.pager.flush()?;
                Ok(())
            }
            Statement::Rollback(_) => {
                // Restore the pager to the snapshot taken at BEGIN.
                if let Some(snap) = ctx.txn_snapshot.take() {
                    ctx.pager.rollback_to(&snap)?;
                }
                ctx.in_transaction = false;
                // Root overrides, index roots, and max_rowids cached during
                // the txn are now stale (their pages may not exist anymore);
                // clear them so the next op rescans.
                ctx.root_overrides.clear();
                ctx.index_roots.clear();
                ctx.max_rowids.clear();
                ctx.rolled_back = true;
                Ok(())
            }
            Statement::Pragma(p) => Self::execute_pragma(p.clone(), ctx),
            Statement::Alter(a) => Self::execute_alter(a.clone(), ctx, catalog),
            Statement::Attach(_) | Statement::Detach(_) => Ok(()),
            Statement::Vacuum(_) => Ok(()),
            Statement::Explain(_) => Ok(()),
            Statement::Select(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                // These produce rows; for `execute`, we just discard them.
                let plan_opt = match stmt {
                    Statement::Select(s) => {
                        let mut planner = Planner::new(catalog);
                        Some(planner.plan_select(s)?)
                    }
                    Statement::Insert(_) => Some(Self::plan_insert(catalog, stmt)?),
                    Statement::Update(_) => Some(Self::plan_update(catalog, stmt)?),
                    Statement::Delete(_) => Some(Self::plan_delete(catalog, stmt)?),
                    _ => None,
                };
                if let Some(plan) = plan_opt {
                    let _ = execute(&plan, ctx)?;
                }
                Ok(())
            }
        }
    }

    fn execute_create(c: CreateStatement, ctx: &mut ExecContext, catalog: &mut Catalog, original_sql: &str) -> Result<()> {
        match c {
            CreateStatement::Table { name, columns, constraints, without_rowid, strict, if_not_exists } => {
                if let Some(_) = catalog.get_table(&name.name) {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(Error::AlreadyExists(format!("table: {}", name.name)));
                }
                let root_page = ctx.pager.allocate_page()?;
                {
                    let page = ctx.pager.get_page(root_page)?;
                    page.lock().init_leaf_table();
                }
                let table = build_table(&name.name, &columns, &constraints, root_page, without_rowid, strict, original_sql)?;
                let schema_row = crate::schema::encode_schema_row(
                    "table",
                    &table.name,
                    &table.name,
                    root_page,
                    &table.create_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                catalog.add_table(table.clone());

                // Implicit UNIQUE indexes — mirrors SQLite's
                // `sqlite_autoindex_<table>_<n>` behavior:
                //   1. Column-level UNIQUE constraints.
                //   2. Table-level UNIQUE (a, b, ...) constraints.
                //   3. Table-level PRIMARY KEY that is NOT a rowid alias
                //      (composite PKs, or a single non-INTEGER PK column).
                //
                // Without these, UNIQUE constraints in CREATE TABLE were
                // silently unenforced (no index → no conflict detection).
                // We synthesize a parseable `CREATE UNIQUE INDEX` statement
                // as the schema SQL so the index round-trips on reopen.
                let mut implicit: Vec<Vec<crate::sql::ast::IndexedColumn>> = Vec::new();
                for col in &columns {
                    if col.constraints.iter().any(|c| matches!(c, crate::sql::ast::ColumnConstraint::Unique)) {
                        implicit.push(vec![crate::sql::ast::IndexedColumn {
                            name: col.name.clone(),
                            order: crate::sql::ast::Order::Asc,
                            collation: None,
                        }]);
                    }
                }
                for c in &constraints {
                    match c {
                        crate::sql::ast::TableConstraint::Unique(cols) => {
                            implicit.push(cols.clone());
                        }
                        crate::sql::ast::TableConstraint::PrimaryKey { columns: cols } => {
                            // Skip the single INTEGER PK (rowid alias).
                            let is_rowid_alias = cols.len() == 1
                                && table.rowid_alias.is_some()
                                && columns
                                    .iter()
                                    .position(|cc| cc.name.eq_ignore_ascii_case(&cols[0].name))
                                    .map(|ci| table.rowid_alias == Some(ci))
                                    .unwrap_or(false);
                            if !is_rowid_alias {
                                implicit.push(cols.clone());
                            }
                        }
                        _ => {}
                    }
                }
                for (n, cols) in implicit.iter().enumerate() {
                    let idx_name = format!("sqlite_autoindex_{}_{}", name.name, n + 1);
                    let col_list = cols
                        .iter()
                        .map(|ic| format!("\"{}\"", ic.name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let idx_sql = format!(
                        "CREATE UNIQUE INDEX \"{}\" ON \"{}\"({})",
                        idx_name, name.name, col_list
                    );
                    let idx_root = ctx.pager.allocate_page()?;
                    {
                        let page = ctx.pager.get_page(idx_root)?;
                        page.lock().init_leaf_index();
                    }
                    let idx_columns = crate::schema::build_index_columns(cols, &table)?;
                    let index = crate::schema::Index {
                        name: idx_name.clone(),
                        table: table.name.clone(),
                        columns: idx_columns,
                        root_page: idx_root,
                        unique: true,
                        partial_expr: None,
                        create_sql: idx_sql.clone(),
                    };
                    let schema_row = crate::schema::encode_schema_row(
                        "index",
                        &index.name,
                        &index.table,
                        idx_root,
                        &index.create_sql,
                    );
                    insert_schema_row(ctx.pager, &schema_row)?;
                    catalog.add_index(index);
                }
                ctx.pager.flush()?;
                Ok(())
            }
            CreateStatement::Index { unique, if_not_exists, name, table: table_name, columns, where_clause } => {
                if catalog.get_index(&name).is_some() {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(Error::AlreadyExists(format!("index: {}", name)));
                }
                let table = catalog.get_table(&table_name).ok_or_else(|| Error::NotFound(format!("table: {}", table_name)))?;
                let root_page = ctx.pager.allocate_page()?;
                {
                    let page = ctx.pager.get_page(root_page)?;
                    page.lock().init_leaf_index();
                }
                let idx_columns = crate::schema::build_index_columns(&columns, &table)?;
                let index = crate::schema::Index {
                    name: name.clone(),
                    table: table_name.clone(),
                    columns: idx_columns,
                    root_page,
                    unique,
                    partial_expr: where_clause,
                    create_sql: original_sql.to_string(),
                };
                // ---- BACKFILL ----
                // Populate the index from rows that already exist in the
                // table. Previously CREATE INDEX registered an EMPTY B+tree
                // and only new writes maintained it — every existing row was
                // invisible to index scans (WHERE col = ? returned nothing;
                // UPDATE ... WHERE indexed_col > ? silently matched zero
                // rows). Rows are decoded once and each entry is inserted
                // into the index B+tree with its rowid.
                //
                // For UNIQUE indexes, a duplicate key in existing data
                // aborts the CREATE INDEX (SQLite semantics). For partial
                // indexes, only rows matching the WHERE clause are indexed.
                let final_root;
                {
                    let table_root = ctx.table_root(&table);
                    let n_cols = table.n_columns();
                    let alias = table.rowid_alias;
                    let col_names: Vec<String> =
                        table.columns.iter().map(|c| c.name.clone()).collect();
                    let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
                    let mut index_bt = crate::storage::btree::Btree::new(ctx.pager, root_page, true);
                    let partial = index.partial_expr.clone();
                    let is_unique = index.unique;
                    let mut backfill_err: Option<crate::error::Error> = None;
                    let mut table_bt = crate::storage::btree::Btree::new(ctx.pager, table_root, false);
                    table_bt.scan_table_borrowed(|rowid, payload| {
                        if crate::storage::row_codec::decode_row_into(
                            payload, n_cols, rowid, alias, &mut row_buf,
                        )
                        .is_err()
                        {
                            return true; // skip corrupt rows
                        }
                        // Partial index: only rows matching the WHERE clause.
                        if let Some(pred) = &partial {
                            match crate::executor::eval_row_public(
                                pred,
                                &row_buf,
                                &col_names,
                                &ctx.params,
                                &ctx.named_params,
                            ) {
                                Ok(v) if v.is_truthy() => {}
                                _ => return true,
                            }
                        }
                        let key = crate::executor::encode_index_key(&index, &table, &row_buf);
                        if is_unique {
                            match index_bt.lookup_index(&key) {
                                Ok(existing) if !existing.is_empty() => {
                                    backfill_err = Some(Error::semantic(format!(
                                        "UNIQUE constraint failed: {}.{}",
                                        table_name,
                                        index.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
                                    )));
                                    return false; // stop the scan
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    backfill_err = Some(e);
                                    return false;
                                }
                            }
                        }
                        if let Err(e) = index_bt.insert_index(&key, rowid) {
                            backfill_err = Some(e);
                            return false;
                        }
                        true
                    })?;
                    if let Some(e) = backfill_err {
                        return Err(e);
                    }
                    // Splits during the backfill may have moved the root.
                    final_root = index_bt.root;
                }
                let index = crate::schema::Index {
                    root_page: final_root,
                    ..index
                };
                let schema_row = crate::schema::encode_schema_row(
                    "index",
                    &index.name,
                    &index.table,
                    final_root,
                    &index.create_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                catalog.add_index(index);
                ctx.pager.flush()?;
                Ok(())
            }
            CreateStatement::View { name, columns, select, if_not_exists } => {
                if catalog.get_view(&name.name).is_some() {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(Error::AlreadyExists(format!("view: {}", name.name)));
                }
                let view = crate::schema::View {
                    name: name.name.clone(),
                    columns,
                    select: *select,
                    create_sql: original_sql.to_string(),
                };
                let schema_row = crate::schema::encode_schema_row(
                    "view",
                    &view.name,
                    &view.name,
                    0,
                    &view.create_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                catalog.add_view(view);
                ctx.pager.flush()?;
                Ok(())
            }
            CreateStatement::Trigger(t) => {
                let trig = crate::schema::Trigger {
                    name: t.name.clone(),
                    table: t.table.clone(),
                    when: t.when,
                    events: t.events,
                    for_each_row: t.for_each_row,
                    when_clause: t.when_clause,
                    body: t.body,
                    create_sql: original_sql.to_string(),
                };
                let schema_row = crate::schema::encode_schema_row(
                    "trigger",
                    &trig.name,
                    &trig.table,
                    0,
                    &trig.create_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                catalog.add_trigger(trig);
                ctx.pager.flush()?;
                Ok(())
            }
        }
    }

    fn execute_drop(d: DropStatement, ctx: &mut ExecContext, catalog: &mut Catalog) -> Result<()> {
        match d.kind {
            DropKind::Table => {
                // Capture indexes BEFORE the catalog drop removes them.
                let indexes_on_it = catalog.indexes_on_table(&d.name);
                let table = catalog.drop_table(&d.name).ok_or_else(|| Error::NotFound(format!("table: {}", d.name)))?;
                ctx.pager.free_page(table.root_page)?;
                delete_schema_row(ctx.pager, "table", &d.name)?;
                // Also free root pages and delete schema rows for every
                // index on this table — including implicit UNIQUE indexes.
                // Previously these rows survived, so reopening the file
                // resurrected orphaned indexes pointing at freed pages.
                for idx in &indexes_on_it {
                    let _ = ctx.pager.free_page(idx.root_page);
                }
                let mut bt = Btree::new(ctx.pager, 0, false);
                let mut to_delete = Vec::new();
                bt.scan_table(|rowid, payload| {
                    if let Ok(row) = decode_row(payload, 5, 0, None) {
                        if let Some((kind, _n, tbl_name, _rootpage, _sql)) = crate::schema::decode_schema_row(&row) {
                            if kind == "index" && tbl_name.eq_ignore_ascii_case(&d.name) {
                                to_delete.push(rowid);
                            }
                        }
                    }
                    true
                })?;
                for rowid in to_delete {
                    bt.delete_table(rowid)?;
                }
                ctx.pager.flush()?;
                Ok(())
            }
            DropKind::Index => {
                let idx = catalog.drop_index(&d.name).ok_or_else(|| Error::NotFound(format!("index: {}", d.name)))?;
                ctx.pager.free_page(idx.root_page)?;
                delete_schema_row(ctx.pager, "index", &d.name)?;
                ctx.pager.flush()?;
                Ok(())
            }
            DropKind::View => {
                catalog.drop_view(&d.name);
                delete_schema_row(ctx.pager, "view", &d.name)?;
                ctx.pager.flush()?;
                Ok(())
            }
            DropKind::Trigger => {
                catalog.drop_trigger(&d.name);
                delete_schema_row(ctx.pager, "trigger", &d.name)?;
                ctx.pager.flush()?;
                Ok(())
            }
        }
    }

    /// ALTER TABLE — RENAME TO (catalog + schema row rewrite, index
    /// tbl_name fixups) and ADD COLUMN (catalog + schema rewrite +
    /// physical back-fill of the new column's default into existing rows,
    /// matching SQLite's read-time default semantics with a one-time
    /// write). RENAME COLUMN / DROP COLUMN are parsed but unsupported.
    fn execute_alter(
        a: crate::sql::ast::AlterStatement,
        ctx: &mut ExecContext,
        catalog: &mut Catalog,
    ) -> Result<()> {
        use crate::sql::ast::AlterAction;
        match a.action {
            AlterAction::RenameTable { new_name } => {
                // Fetch first for validation + name rewriting (fetching by
                // Arc so the original stays in the catalog until the move).
                let table = catalog
                    .get_table(&a.table)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", a.table)))?;
                if catalog.get_table(&new_name).is_some() {
                    return Err(Error::AlreadyExists(format!("table: {}", new_name)));
                }
                let old_name = table.name.clone();
                let root = table.root_page;

                // Rewrite the CREATE TABLE statement's table name and the
                // qualified column names; everything else (columns, FKs,
                // checks) is unchanged.
                let new_sql = rewrite_create_table_name(&table.create_sql, &new_name);
                let mut rebuilt = (*table).clone();
                rebuilt.name = new_name.clone();
                rebuilt.create_sql = new_sql.clone();
                rebuilt.qualified_col_names = rebuilt
                    .columns
                    .iter()
                    .map(|c| format!("{}.{}", new_name, c.name))
                    .collect::<Vec<_>>()
                    .into();

                // Move the catalog entry (indexes + triggers follow).
                catalog.rename_table(&old_name, &new_name).ok_or_else(|| {
                    Error::AlreadyExists(format!("table: {}", new_name))
                })?;
                // Replace the moved entry's Arc with the rebuilt table.
                catalog.replace_table(&new_name, rebuilt);

                // Other tables' REFERENCES clauses follow the rename
                // (SQLite modern rename mode rewrites them).
                catalog.rename_fk_references(&old_name, &new_name);

                // Schema row: delete old, insert new (kind=table,
                // name/tbl_name=new, same root, new SQL).
                delete_schema_row(ctx.pager, "table", &old_name)?;
                let schema_row = crate::schema::encode_schema_row(
                    "table", &new_name, &new_name, root, &new_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;

                // Index schema rows keep name and SQL but tbl_name follows.
                rewrite_index_tbl_names(ctx.pager, &old_name, &new_name)?;
                rewrite_trigger_tbl_names(ctx.pager, &old_name, &new_name)?;

                ctx.pager.flush()?;
                Ok(())
            }
            AlterAction::AddColumn { column } => {
                let table = catalog
                    .get_table(&a.table)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", a.table)))?;
                let root = table.root_page;
                // SQLite restrictions: the new column may not be PRIMARY
                // KEY or UNIQUE, and must either be nullable or carry a
                // DEFAULT.
                let has_pk = column
                    .constraints
                    .iter()
                    .any(|c| matches!(c, crate::sql::ast::ColumnConstraint::PrimaryKey { .. }));
                let has_unique = column
                    .constraints
                    .iter()
                    .any(|c| matches!(c, crate::sql::ast::ColumnConstraint::Unique));
                if has_pk || has_unique {
                    return Err(Error::semantic(
                        "Cannot add a PRIMARY KEY or UNIQUE column with ALTER TABLE",
                    ));
                }
                let has_default = column
                    .constraints
                    .iter()
                    .any(|c| matches!(c, crate::sql::ast::ColumnConstraint::Default(_)));
                let not_null = column
                    .constraints
                    .iter()
                    .any(|c| matches!(c, crate::sql::ast::ColumnConstraint::NotNull));
                if not_null && !has_default {
                    return Err(Error::semantic(
                        "Cannot add a NOT NULL column with no DEFAULT value",
                    ));
                }

                // The value existing rows will carry: the DEFAULT (or NULL).
                let default_value: Value = if has_default {
                    let expr = column
                        .constraints
                        .iter()
                        .find_map(|c| match c {
                            crate::sql::ast::ColumnConstraint::Default(e) => Some(e.clone()),
                            _ => None,
                        })
                        .unwrap();
                    let names: Vec<String> =
                        table.columns.iter().map(|c| c.name.clone()).collect();
                    crate::executor::eval_row_public(
                        &expr,
                        &[],
                        &names,
                        &[],
                        &std::collections::HashMap::new(),
                    )?
                } else {
                    Value::Null
                };

                // Rewrite the CREATE TABLE SQL to include the new column.
                let new_sql = rewrite_create_table_add_column(&table.create_sql, &column);
                let mut rebuilt = (*table).clone();
                // Rebuild via build_table so affinity/constraints parse
                // consistently (parse the new SQL and take its columns).
                if let Ok(stmt) = crate::sql::parser::parse(&new_sql) {
                    if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
                        columns, constraints, without_rowid, strict, ..
                    }) = stmt
                    {
                        rebuilt = build_table(&table.name, &columns, &constraints, root, without_rowid, strict, &new_sql)?;
                    }
                }
                let table_name = rebuilt.name.clone();
                catalog.drop_table(&a.table);
                catalog.add_table(rebuilt);

                // Schema row rewrite.
                delete_schema_row(ctx.pager, "table", &table.name)?;
                let schema_row =
                    crate::schema::encode_schema_row("table", &table_name, &table_name, root, &new_sql);
                insert_schema_row(ctx.pager, &schema_row)?;

                // Physical back-fill: append the default to every existing
                // row. (SQLite stores the default in the schema and
                // materializes it at read time; a one-time rewrite is the
                // same observable behavior.)
                if !default_value.is_null() {
                    let n_cols = table.n_columns();
                    let alias = table.rowid_alias;
                    let mut updates: Vec<(i64, Vec<u8>)> = Vec::new();
                    {
                        let mut bt = Btree::new(ctx.pager, root, false);
                        bt.scan_table(|rowid, payload| {
                            if let Ok(mut row) = decode_row(payload, n_cols, rowid, alias) {
                                row.push(default_value.clone());
                                let new_payload = encode_row_aliased(&row, alias);
                                updates.push((rowid, new_payload));
                            }
                            true
                        })?;
                    }
                    let mut bt = Btree::new(ctx.pager, root, false);
                    for (rowid, payload) in updates {
                        let did = bt.update_table(rowid, &payload).unwrap_or(false);
                        if !did {
                            bt.delete_table(rowid)?;
                            bt.insert_table(rowid, &payload)?;
                        }
                    }
                    if bt.root != root {
                        ctx.set_table_root(&table.name, bt.root);
                    }
                }
                ctx.pager.flush()?;
                Ok(())
            }
            AlterAction::RenameColumn { .. } | AlterAction::DropColumn { .. } => Err(
                Error::Unsupported("ALTER TABLE RENAME COLUMN / DROP COLUMN"),
            ),
        }
    }

    fn execute_pragma(p: PragmaStatement, ctx: &mut ExecContext) -> Result<()> {
        // Honored pragmas:
        //   foreign_keys = 0/1/on/off   — toggle FK enforcement (SQLite
        //                                 default: off)
        //   cache_size = N              — page-cache capacity hint (floor 1)
        //   synchronous, journal_mode, temp_store, locking_mode,
        //   legacy_alter_table, recursive_triggers, defer_foreign_keys —
        //   parsed and accepted, semantics unchanged.
        let name = p.name.to_ascii_lowercase();
        if let Some(value) = &p.value {
            // Evaluate the value expression with no row context.
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = crate::executor::EvalContext::new(
                &empty_row,
                &empty_cols,
                &ctx.params,
                &ctx.named_params,
            );
            let v = crate::executor::evaluate(&value_as_expr(value), &eval_ctx)?;
            match name.as_str() {
                "foreign_keys" => {
                    let on = v.is_truthy();
                    ctx.pager.set_foreign_keys_enabled(on);
                }
                "cache_size" => {
                    // Advisory: the pager's cache capacity is set at open.
                    // Accept and ignore (no error) — SQLite treats this as
                    // a hint too.
                }
                _ => {}
            }
        }
        Ok(())
    }
}


/// Extract the expression from a PRAGMA value (plain expr or parenthesized
/// call form).
fn value_as_expr(v: &crate::sql::ast::PragmaValue) -> &crate::sql::ast::Expr {
    match v {
        crate::sql::ast::PragmaValue::Expr(e) => e,
        crate::sql::ast::PragmaValue::Call(e) => e,
    }
}



/// Minimal Expr → SQL text renderer (enough for DEFAULT / CHECK clauses
/// replayed through ALTER TABLE ADD COLUMN's schema rewrite).
fn expr_to_sql(e: &Expr) -> String {
    use crate::sql::ast::Expr as E;
    match e {
        E::Literal(v) => match v {
            Value::Null => "NULL".into(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) => r.to_string(),
            Value::Text(t) => format!("'{}'", t.replace('\'', "''")),
            Value::Blob(b) => {
                let hex: String = b.iter().map(|x| format!("{:02X}", x)).collect();
                format!("X'{}'", hex)
            }
        },
        E::Column { table: None, name } => name.clone(),
        E::Column { table: Some(t), name } => format!("{}.{}", t, name),
        E::Binary { op, left, right } => {
            format!("{} {:?} {}", expr_to_sql(left), op, expr_to_sql(right))
        }
        E::Unary { op, expr } => format!("{:?} {}", op, expr_to_sql(expr)),
        E::Function { name, args, .. } => {
            let inner: Vec<String> = args.iter().map(expr_to_sql).collect();
            format!("{}({})", name, inner.join(", "))
        }
        E::Row(items) => {
            let inner: Vec<String> = items.iter().map(expr_to_sql).collect();
            format!("({})", inner.join(", "))
        }
        _ => "?".to_string(),
    }
}

/// Rewrite `CREATE TABLE <old> ...` → `CREATE TABLE <new> ...` preserving
/// the rest of the statement text.
fn rewrite_create_table_name(sql: &str, new_name: &str) -> String {
    // Find the table-name token after "CREATE TABLE" (case-insensitive),
    // possibly with IF NOT EXISTS.
    let lower = sql.to_ascii_lowercase();
    let ct = match lower.find("create table") {
        Some(i) => i + "create table".len(),
        None => return sql.to_string(),
    };
    let rest = &sql[ct..];
    let trimmed = rest.trim_start();
    let ws = rest.len() - trimmed.len();
    let after_ws = ct + ws;
    if trimmed.to_ascii_lowercase().starts_with("if not exists") {
        let kw_len = "if not exists".len();
        let rest2 = &sql[after_ws + kw_len..];
        let t2 = rest2.trim_start();
        let ws2 = rest2.len() - t2.len();
        let name_len = t2
            .find(|c: char| c.is_whitespace() || c == '(')
            .unwrap_or(t2.len());
        let name_start = after_ws + kw_len + ws2;
        return format!("{}{}{}", &sql[..name_start], new_name, &sql[name_start + name_len..]);
    }
    let name_len = trimmed
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(trimmed.len());
    format!("{}{}{}", &sql[..after_ws], new_name, &sql[after_ws + name_len..])
}

/// Append `, <column-def>` to a CREATE TABLE statement's column list (just
/// before the closing paren).
fn rewrite_create_table_add_column(
    sql: &str,
    column: &crate::sql::ast::ColumnDef,
) -> String {
    // Render the column definition back to SQL text.
    let mut col_sql = column.name.clone();
    if !column.type_name.is_empty() {
        col_sql.push(' ');
        col_sql.push_str(&column.type_name);
    }
    for c in &column.constraints {
        use crate::sql::ast::ColumnConstraint::*;
        col_sql.push(' ');
        match c {
            PrimaryKey { autoincrement, .. } => {
                col_sql.push_str("PRIMARY KEY");
                if *autoincrement {
                    col_sql.push_str(" AUTOINCREMENT");
                }
            }
            NotNull => col_sql.push_str("NOT NULL"),
            Null => col_sql.push_str("NULL"),
            Unique => col_sql.push_str("UNIQUE"),
            Check(e) => col_sql.push_str(&format!("CHECK ({})", expr_to_sql(e))),
            Default(e) => col_sql.push_str(&format!("DEFAULT {}", expr_to_sql(e))),
            Collate(c) => col_sql.push_str(&format!("COLLATE {}", c)),
            References { table, columns, on_delete, on_update } => {
                col_sql.push_str(&format!("REFERENCES {}", table));
                if !columns.is_empty() {
                    col_sql.push_str(&format!("({})", columns.join(", ")));
                }
                use crate::sql::ast::ForeignKeyAction::*;
                let act = |a: &crate::sql::ast::ForeignKeyAction| match a {
                    NoAction => "NO ACTION".to_string(),
                    Restrict => "RESTRICT".to_string(),
                    SetNull => "SET NULL".to_string(),
                    SetDefault => "SET DEFAULT".to_string(),
                    Cascade => "CASCADE".to_string(),
                };
                if !matches!(on_delete, NoAction) {
                    col_sql.push_str(&format!(" ON DELETE {}", act(on_delete)));
                }
                if !matches!(on_update, NoAction) {
                    col_sql.push_str(&format!(" ON UPDATE {}", act(on_update)));
                }
            }
            GeneratedAs { expr, stored } => {
                col_sql.push_str(&format!(
                    "GENERATED ALWAYS AS ({}){}",
                    expr_to_sql(expr),
                    if *stored { " STORED" } else { "" }
                ));
            }
        }
    }
    // Insert before the LAST ')' of the statement.
    match sql.rfind(')') {
        Some(close) => {
            // trim trailing whitespace/comma before the close paren
            let before = &sql[..close];
            let trimmed = before.trim_end();
            format!("{}, {})", trimmed, col_sql)
        }
        None => sql.to_string(),
    }
}

/// Rewrite the `tbl_name` column of every trigger schema row on `old_table`
/// (ALTER TABLE RENAME TO keeps triggers attached to the renamed table).
fn rewrite_trigger_tbl_names(pager: &Pager, old_table: &str, new_table: &str) -> Result<()> {
    let mut bt = Btree::new(pager, 0, false);
    let mut updates: Vec<(i64, Vec<Value>)> = Vec::new();
    bt.scan_table(|rowid, payload| {
        if let Ok(row) = decode_row(payload, 5, 0, None) {
            if let Some((kind, name, tbl_name, rootpage, sql)) = crate::schema::decode_schema_row(&row) {
                if kind == "trigger" && tbl_name.eq_ignore_ascii_case(old_table) {
                    let new_sql = rewrite_on_table(&sql, old_table, new_table);
                    updates.push((
                        rowid,
                        crate::schema::encode_schema_row("trigger", &name, new_table, rootpage, &new_sql),
                    ));
                }
            }
        }
        true
    })?;
    for (rowid, row) in updates {
        let payload = encode_row(&row);
        bt.delete_table(rowid)?;
        bt.insert_table(rowid, &payload)?;
    }
    Ok(())
}


/// Rewrite `... ON <old> ...` → `... ON <new> ...` in a CREATE INDEX /
/// CREATE TRIGGER statement (case-insensitive table name, word boundaries).
fn rewrite_on_table(sql: &str, old: &str, new: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let on_kw = " on ";
    let mut out = sql.to_string();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find(on_kw) {
        let on_pos = search_from + rel + on_kw.len();
        // The table name runs from on_pos to the next delimiter.
        let rest = &sql[on_pos..];
        let name_len = rest
            .find(|c: char| c.is_whitespace() || c == '(' || c == ';')
            .unwrap_or(rest.len());
        let name = &sql[on_pos..on_pos + name_len];
        if name.eq_ignore_ascii_case(old) {
            out = format!("{}{}{}", &sql[..on_pos], new, &sql[on_pos + name_len..]);
            // Rebuild the lowercase view for the next iteration.
            return rewrite_on_table(&out, old, new);
        }
        search_from = on_pos + name_len.max(1);
    }
    out
}

/// Rewrite the `tbl_name` column of every index schema row that belongs to
/// `old_table` (used by ALTER TABLE RENAME TO).
fn rewrite_index_tbl_names(pager: &Pager, old_table: &str, new_table: &str) -> Result<()> {
    let mut bt = Btree::new(pager, 0, false);
    let mut updates: Vec<(i64, Vec<Value>)> = Vec::new();
    bt.scan_table(|rowid, payload| {
        if let Ok(row) = decode_row(payload, 5, 0, None) {
            if let Some((kind, name, tbl_name, rootpage, sql)) = crate::schema::decode_schema_row(&row) {
                if kind == "index" && tbl_name.eq_ignore_ascii_case(old_table) {
                    let new_sql = rewrite_on_table(&sql, old_table, new_table);
                    updates.push((
                        rowid,
                        crate::schema::encode_schema_row("index", &name, new_table, rootpage, &new_sql),
                    ));
                }
            }
        }
        true
    })?;
    for (rowid, row) in updates {
        let payload = encode_row(&row);
        bt.delete_table(rowid)?;
        bt.insert_table(rowid, &payload)?;
    }
    Ok(())
}

/// Insert a row into the schema table (rooted at page 0).
fn insert_schema_row(pager: &Pager, row: &[Value]) -> Result<()> {
    // Find max rowid in the schema table.
    let mut max_rowid = 0i64;
    let mut bt = Btree::new(pager, 0, false);
    bt.scan_table(|rowid, _| {
        if rowid > max_rowid {
            max_rowid = rowid;
        }
        true
    })?;
    let rowid = max_rowid + 1;
    let row_vec: Vec<Value> = row.to_vec();
    let payload = encode_row(&row_vec);
    bt.insert_table(rowid, &payload)?;
    Ok(())
}

/// Delete a schema row by (kind, name).
fn delete_schema_row(pager: &Pager, kind: &str, name: &str) -> Result<()> {
    let mut bt = Btree::new(pager, 0, false);
    let mut to_delete = Vec::new();
    bt.scan_table(|rowid, payload| {
        if let Ok(row) = decode_row(payload, 5, 0, None) {
            if let Some((k, n, _, _, _)) = crate::schema::decode_schema_row(&row) {
                if k == kind && n.eq_ignore_ascii_case(name) {
                    to_delete.push(rowid);
                }
            }
        }
        true
    })?;
    for rowid in to_delete {
        bt.delete_table(rowid)?;
    }
    Ok(())
}

/// Load the schema from the schema table (page 0) into the catalog.
fn load_schema(pager: &Pager, catalog: &mut Catalog) -> Result<()> {
    let mut bt = Btree::new(pager, 0, false);
    let mut entries = Vec::new();
    bt.scan_table(|_rowid, payload| {
        if let Ok(row) = decode_row(payload, 5, 0, None) {
            entries.push(row);
        }
        true
    })?;
    // Two passes: TABLES first, then everything else. ALTER TABLE RENAME
    // re-inserts the table's schema row at the END of the schema B+tree
    // (delete + insert), so an index's row can precede its table's row —
    // a single pass would see the index before the table exists.
    // Decode into owned tuples so the row Vecs can drop.
    let mut tables_first: Vec<(String, String, String, u32, String)> = Vec::new();
    let mut others: Vec<(String, String, String, u32, String)> = Vec::new();
    for row in entries {
        if let Some((kind, _name, tbl_name, rootpage, sql)) = crate::schema::decode_schema_row(&row) {
            let owned = (kind.to_string(), _name.to_string(), tbl_name.to_string(), rootpage, sql.to_string());
            if kind == "table" {
                tables_first.push(owned);
            } else {
                others.push(owned);
            }
        }
    }
    let ordered = tables_first.into_iter().chain(others.into_iter());
    for (kind, _name, tbl_name, rootpage, sql) in ordered {
        let kind = kind.as_str();
        let sql = sql.as_str();
        let _ = &_name;
        let _ = &tbl_name;
        {
            match kind {
                "table" => {
                    if let Ok(stmt) = parse(sql) {
                        if let Statement::Create(CreateStatement::Table { name: tn, columns, constraints, without_rowid, strict, .. }) = stmt {
                            let table = build_table(&tn.name, &columns, &constraints, rootpage, without_rowid, strict, sql)?;
                            catalog.add_table(table);
                        }
                    }
                }
                "index" => {
                    if let Ok(stmt) = parse(sql) {
                        if let Statement::Create(CreateStatement::Index { unique, name: idx_name, table, columns, where_clause, .. }) = stmt {
                            let table_obj = catalog.get_table(&table).ok_or_else(|| Error::corruption(format!("index {} references missing table {}", idx_name, table)))?;
                            let idx_columns = crate::schema::build_index_columns(&columns, &table_obj)?;
                            catalog.add_index(crate::schema::Index {
                                name: idx_name,
                                table,
                                columns: idx_columns,
                                root_page: rootpage,
                                unique,
                                partial_expr: where_clause,
                                create_sql: sql.to_string(),
                            });
                        }
                    }
                }
                "view" => {
                    if let Ok(stmt) = parse(sql) {
                        if let Statement::Create(CreateStatement::View { name: vn, columns, select, .. }) = stmt {
                            catalog.add_view(crate::schema::View {
                                name: vn.name,
                                columns,
                                select: *select,
                                create_sql: sql.to_string(),
                            });
                        }
                    }
                }
                "trigger" => {
                    if let Ok(stmt) = parse(sql) {
                        if let Statement::Create(CreateStatement::Trigger(t)) = stmt {
                            catalog.add_trigger(crate::schema::Trigger {
                                name: t.name,
                                table: t.table,
                                when: t.when,
                                events: t.events,
                                for_each_row: t.for_each_row,
                                when_clause: t.when_clause,
                                body: t.body,
                                create_sql: sql.to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
            let _ = tbl_name;
        }
    }
    Ok(())
}

/// Heuristic: does this SQL string start a DDL statement (CREATE/DROP/ALTER)?
/// Used to invalidate the statement cache after schema changes. We only need
/// a cheap prefix check — the parser is the source of truth, and the cache
/// will be re-populated on the next call.

// ============================================================================
// Fast INSERT path
// ============================================================================

/// Pieces of a fast-path INSERT statement, sliced directly out of the SQL
/// text (no tokenizer, no AST, no Plan).
struct FastInsert<'a> {
    /// Table name as written (matched case-insensitively).
    table: &'a str,
    /// Column names as written; empty = all columns in declared order.
    columns: Vec<&'a str>,
    /// Literal values in order — one Vec per VALUES row.
    values: Vec<Vec<Value>>,
}

/// Ultra-lightweight scanner for the single hottest statement shape in
/// OLTP workloads:
///
/// ```text
/// INSERT INTO <table> [(<col>, <col>, ...)] VALUES (<literal>, ...)
/// ```
///
/// Recognizes ONLY single-row literal VALUES inserts. Anything else —
/// multi-row VALUES, non-literal expressions, `?` parameters, ON CONFLICT /
/// UPSERT / RETURNING clauses, INSERT OR ..., DEFAULT VALUES, SELECT
/// sources, quoted identifiers, trailing garbage — returns None and the
/// caller falls back to the full parse path. This skips the entire
/// tokenizer -> parser -> planner -> statement-cache pipeline (~1.3 us per
/// statement), which dominates single-row INSERT cost.
#[inline]
fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Match keyword `kw` (ASCII, case-insensitive) at `i`; requires the
/// following byte to be a non-identifier char so `INTO` doesn't match
/// `INTOXICATED`. Returns the index after the keyword.
#[inline]
fn match_word_ci(b: &[u8], i: usize, kw: &str) -> Option<usize> {
    let kb = kw.as_bytes();
    if i + kb.len() > b.len() {
        return None;
    }
    for (j, &k) in kb.iter().enumerate() {
        if b[i + j] != k && b[i + j] != (k ^ 0x20) {
            return None;
        }
    }
    let after = i + kb.len();
    if after < b.len() {
        let c = b[after];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            return None;
        }
    }
    Some(after)
}

#[inline]
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

#[inline]
fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// Read an unquoted identifier starting at `i`. Returns (start, end).
#[inline]
fn read_ident(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i >= b.len() || !is_ident_start(b[i]) {
        return None;
    }
    let start = i;
    let mut j = i;
    while j < b.len() && is_ident_char(b[j]) {
        j += 1;
    }
    Some((start, j))
}

/// Parse one literal at `i`. Supports NULL, TRUE, FALSE, integers (decimal
/// and 0x hex), floats, signed numbers, and single-quoted strings with
/// `''` escapes (UTF-8-safe). Anything else -> None.
fn parse_fast_literal(b: &[u8], i: usize) -> Option<(Value, usize)> {
    if i >= b.len() {
        return None;
    }
    // Keywords
    if let Some(j) = match_word_ci(b, i, "NULL") {
        return Some((Value::Null, j));
    }
    if let Some(j) = match_word_ci(b, i, "TRUE") {
        return Some((Value::Integer(1), j));
    }
    if let Some(j) = match_word_ci(b, i, "FALSE") {
        return Some((Value::Integer(0), j));
    }
    // Sign
    let mut j = i;
    let neg = match b[j] {
        b'-' => {
            j += 1;
            true
        }
        b'+' => {
            j += 1;
            false
        }
        _ => false,
    };
    // Hex
    if j + 1 < b.len() && b[j] == b'0' && (b[j + 1] | 0x20) == b'x' {
        let hs = j + 2;
        let mut he = hs;
        while he < b.len() && b[he].is_ascii_hexdigit() {
            he += 1;
        }
        if he == hs {
            return None;
        }
        let text = std::str::from_utf8(&b[hs..he]).ok()?;
        let v = i64::from_str_radix(text, 16).ok()?;
        return Some((Value::Integer(if neg { -v } else { v }), he));
    }
    // Decimal / float
    if j < b.len() && (b[j].is_ascii_digit() || (b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit())) {
        let ns = j;
        let mut ne = j;
        while ne < b.len() && b[ne].is_ascii_digit() {
            ne += 1;
        }
        let mut is_float = false;
        if ne < b.len() && b[ne] == b'.' {
            is_float = true;
            ne += 1;
            while ne < b.len() && b[ne].is_ascii_digit() {
                ne += 1;
            }
        }
        if ne < b.len() && (b[ne] | 0x20) == b'e' {
            let save = ne;
            ne += 1;
            if ne < b.len() && (b[ne] == b'+' || b[ne] == b'-') {
                ne += 1;
            }
            if ne < b.len() && b[ne].is_ascii_digit() {
                is_float = true;
                while ne < b.len() && b[ne].is_ascii_digit() {
                    ne += 1;
                }
            } else {
                ne = save; // not an exponent — leave 'e' for the caller to reject
            }
        }
        let text = std::str::from_utf8(&b[ns..ne]).ok()?;
        if is_float {
            let v: f64 = text.parse().ok()?;
            Some((Value::Real(if neg { -v } else { v }), ne))
        } else {
            match text.parse::<i64>() {
                Ok(v) => Some((Value::Integer(if neg { -v } else { v }), ne)),
                // Out-of-i64-range integer literal: SQLite treats it as REAL.
                Err(_) => {
                    let v: f64 = text.parse().ok()?;
                    Some((Value::Real(if neg { -v } else { v }), ne))
                }
            }
        }
    } else if b[i] == b'\'' {
        // String literal with '' escapes; UTF-8-safe byte collection.
        let mut k = i + 1;
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            if k >= b.len() {
                return None; // unterminated
            }
            let c = b[k];
            if c == b'\'' {
                if k + 1 < b.len() && b[k + 1] == b'\'' {
                    bytes.push(b'\'');
                    k += 2;
                } else {
                    k += 1;
                    break;
                }
            } else {
                bytes.push(c);
                k += 1;
            }
        }
        let s = String::from_utf8(bytes).ok()?;
        Some((Value::Text(s), k))
    } else {
        None
    }
}

fn try_fast_insert_parse(sql: &str) -> Option<FastInsert<'_>> {
    let b = sql.as_bytes();
    let mut i = skip_ws(b, 0);
    i = match_word_ci(b, i, "INSERT")?;
    i = skip_ws(b, i);
    i = match_word_ci(b, i, "INTO")?;
    i = skip_ws(b, i);
    let (ts, te) = read_ident(b, i)?;
    let table = &sql[ts..te];
    i = skip_ws(b, te);
    // Optional column list.
    let mut columns: Vec<&str> = Vec::new();
    if i < b.len() && b[i] == b'(' {
        i += 1;
        loop {
            i = skip_ws(b, i);
            let (cs, ce) = read_ident(b, i)?;
            columns.push(&sql[cs..ce]);
            i = skip_ws(b, ce);
            if i < b.len() && b[i] == b',' {
                i += 1;
                continue;
            }
            break;
        }
        if i >= b.len() || b[i] != b')' {
            return None;
        }
        i += 1;
        i = skip_ws(b, i);
    }
    i = match_word_ci(b, i, "VALUES")?;
    i = skip_ws(b, i);
    // Multi-row VALUES: ALL rows are scanned up front (a syntax error in
    // row 37 must insert nothing, matching SQLite's parse-then-execute —
    // the general path's parser also rejects the whole statement before
    // any row is executed).
    let mut values: Vec<Vec<Value>> = Vec::new();
    loop {
        if i >= b.len() || b[i] != b'(' {
            return None;
        }
        i += 1;
        let mut row: Vec<Value> = Vec::new();
        loop {
            i = skip_ws(b, i);
            let (v, ni) = parse_fast_literal(b, i)?;
            row.push(v);
            i = skip_ws(b, ni);
            if i < b.len() && b[i] == b',' {
                i += 1;
                continue;
            }
            break;
        }
        if i >= b.len() || b[i] != b')' {
            return None;
        }
        i += 1;
        values.push(row);
        i = skip_ws(b, i);
        if i < b.len() && b[i] == b',' {
            i = skip_ws(b, i + 1);
            continue;
        }
        break;
    }
    // Optional single trailing semicolon, then end-of-statement.
    if i < b.len() && b[i] == b';' {
        i = skip_ws(b, i + 1);
    }
    if i != b.len() {
        return None;
    }
    if values.is_empty() {
        return None;
    }
    Some(FastInsert { table, columns, values })
}

fn is_ddl_sql(sql: &str) -> bool {
    // Fast path: scan at most ~10 chars, case-insensitive ASCII compare
    // without allocating a new String. The previous version called
    // `to_ascii_uppercase()` which allocated a String per call — for a
    // 1k-statement INSERT batch, that was 1k String allocations just for
    // DDL detection.
    let bytes = sql.as_bytes();
    let mut i = 0;
    // Skip leading whitespace + '(' repeats.
    while i < bytes.len() {
        let b = bytes[i];
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' || b == b'(' {
            i += 1;
            continue;
        }
        break;
    }
    // Now compare case-insensitively against the DDL keywords.
    let rest = &bytes[i..];
    fn eq_ci(p: &[u8], kw: &[u8]) -> bool {
        if p.len() < kw.len() {
            return false;
        }
        for j in 0..kw.len() {
            let mut c = p[j];
            // ASCII to_upper
            if c >= b'a' && c <= b'z' {
                c -= 32;
            }
            if c != kw[j] {
                return false;
            }
        }
        true
    }
    eq_ci(rest, b"CREATE ")
        || eq_ci(rest, b"DROP ")
        || eq_ci(rest, b"ALTER ")
        || eq_ci(rest, b"ATTACH ")
        || eq_ci(rest, b"DETACH ")
        || eq_ci(rest, b"VACUUM")
}

/// A trait for things that can be converted into a sequence of bound parameters.
pub trait Params {
    type Iter: Iterator<Item = Value>;
    fn into_iter(self) -> Self::Iter;
}

impl Params for () {
    type Iter = std::iter::Empty<Value>;
    fn into_iter(self) -> Self::Iter {
        std::iter::empty()
    }
}

impl Params for Vec<Value> {
    type Iter = std::vec::IntoIter<Value>;
    fn into_iter(self) -> Self::Iter {
        <Vec<Value> as IntoIterator>::into_iter(self)
    }
}

impl<const N: usize> Params for [Value; N] {
    type Iter = std::array::IntoIter<Value, N>;
    fn into_iter(self) -> Self::Iter {
        std::array::IntoIter::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memdb() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn create_insert_select() {
        let mut db = memdb();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
        db.execute("INSERT INTO users (name) VALUES ('Alice')", []).unwrap();
        db.execute("INSERT INTO users (name) VALUES ('Bob')", []).unwrap();
        let rows = db.query("SELECT id, name FROM users", []).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Text("Alice".into()));
        assert_eq!(rows[1][1], Value::Text("Bob".into()));
    }

    #[test]
    fn update_and_delete() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", []).unwrap();
        db.execute("UPDATE t SET x = x + 1", []).unwrap();
        let rows = db.query("SELECT x FROM t ORDER BY id", []).unwrap();
        assert_eq!(rows, vec![
            vec![Value::Integer(11)],
            vec![Value::Integer(21)],
            vec![Value::Integer(31)],
        ]);
        db.execute("DELETE FROM t WHERE x = 21", []).unwrap();
        let rows = db.query("SELECT x FROM t ORDER BY id", []).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn aggregate() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (1), (2), (3), (4), (5)", []).unwrap();
        let rows = db.query("SELECT SUM(x), COUNT(*), MIN(x), MAX(x), AVG(x) FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(15));
        assert_eq!(rows[0][1], Value::Integer(5));
        assert_eq!(rows[0][2], Value::Integer(1));
        assert_eq!(rows[0][3], Value::Integer(5));
    }

    #[test]
    fn join() {
        let mut db = memdb();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
        db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)", []).unwrap();
        db.execute("INSERT INTO users (name) VALUES ('Alice'), ('Bob')", []).unwrap();
        db.execute("INSERT INTO orders (user_id, total) VALUES (1, 100), (1, 200), (2, 50)", []).unwrap();
        let rows = db.query("SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id", []).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn group_by() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (k, v) VALUES ('a', 1), ('a', 2), ('b', 3), ('b', 4)", []).unwrap();
        let rows = db.query("SELECT k, SUM(v) FROM t GROUP BY k", []).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn reopen_persists() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut db = Database::open(tmp.path()).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
            db.execute("INSERT INTO t (name) VALUES ('Alice')", []).unwrap();
        }
        let mut db = Database::open(tmp.path()).unwrap();
        let rows = db.query("SELECT name FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Text("Alice".into()));
    }

    // ========================================================================
    // UPSERT / RETURNING / CHECK / NOT NULL / date-time integration tests
    // ========================================================================

    #[test]
    fn upsert_do_nothing() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT UNIQUE)", []).unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a')", []).unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 'b') ON CONFLICT (id) DO NOTHING",
            [],
        ).unwrap();
        let rows = db.query("SELECT id, val FROM t", []).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("a".into()));
    }

    #[test]
    fn upsert_do_update() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT, n INTEGER)", []).unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a', 10)", []).unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 'b', 5) ON CONFLICT (id) DO UPDATE SET n = n + excluded.n",
            [],
        ).unwrap();
        let rows = db.query("SELECT id, val, n FROM t", []).unwrap();
        assert_eq!(rows.len(), 1);
        // SET doesn't touch val → old value 'a'; n = 10 + 5 = 15.
        assert_eq!(rows[0][1], Value::Text("a".into()));
        assert_eq!(rows[0][2], Value::Integer(15));

        // Upsert with a direct excluded reference replacing the column.
        db.execute(
            "INSERT INTO t VALUES (1, 'z', 100) ON CONFLICT (id) DO UPDATE SET val = excluded.val, n = excluded.n",
            [],
        ).unwrap();
        let rows = db.query("SELECT val, n FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Text("z".into()));
        assert_eq!(rows[0][1], Value::Integer(100));
    }

    #[test]
    fn upsert_unique_index_target() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT)", []).unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a@x.com', 'Alice')", []).unwrap();
        // Conflict on the UNIQUE(email) index — targeted upsert.
        db.execute(
            "INSERT INTO t VALUES (2, 'a@x.com', 'Bob') ON CONFLICT (email) DO UPDATE SET name = excluded.name",
            [],
        ).unwrap();
        let rows = db.query("SELECT id, name FROM t", []).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[0][1], Value::Text("Bob".into()));
    }

    #[test]
    fn upsert_where_guard() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", []).unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)", []).unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 99) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE n < 50",
            [],
        ).unwrap();
        let rows = db.query("SELECT n FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(99));
        // Guard fails (99 >= 50) → no-op.
        db.execute(
            "INSERT INTO t VALUES (1, 1000) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE n < 50",
            [],
        ).unwrap();
        let rows = db.query("SELECT n FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(99));
    }

    #[test]
    fn upsert_bad_target_errors() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)", []).unwrap();
        let err = db.execute(
            "INSERT INTO t VALUES (1, 1, 1) ON CONFLICT (a, b) DO NOTHING",
            [],
        );
        assert!(err.is_err());
    }

    #[test]
    fn insert_returning() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        let rows = db.query(
            "INSERT INTO t (x) VALUES (10), (20) RETURNING id, x",
            [],
        ).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[0][1], Value::Integer(10));
        assert_eq!(rows[1][0], Value::Integer(2));
        assert_eq!(rows[1][1], Value::Integer(20));
    }

    #[test]
    fn insert_returning_star() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        let rows = db.query(
            "INSERT INTO t (x) VALUES (7) RETURNING *",
            [],
        ).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Integer(1), Value::Integer(7)]);
    }

    #[test]
    fn update_returning() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", []).unwrap();
        let rows = db.query(
            "UPDATE t SET x = x * 2 WHERE x > 15 RETURNING id, x",
            [],
        ).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(2));
        assert_eq!(rows[0][1], Value::Integer(40));
        assert_eq!(rows[1][1], Value::Integer(60));
    }

    #[test]
    fn delete_returning() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", []).unwrap();
        let rows = db.query(
            "DELETE FROM t WHERE x <= 20 RETURNING x",
            [],
        ).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(10));
        assert_eq!(rows[1][0], Value::Integer(20));
        let left = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(left[0][0], Value::Integer(1));
    }

    #[test]
    fn check_constraint_enforced() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0))", []).unwrap();
        db.execute("INSERT INTO t (age) VALUES (25)", []).unwrap();
        let err = db.execute("INSERT INTO t (age) VALUES (-1)", []);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("CHECK constraint failed"));
        // UPDATE violating the CHECK fails too.
        let err = db.execute("UPDATE t SET age = -5 WHERE age = 25", []);
        assert!(err.is_err());
        // Table still has the valid row.
        let rows = db.query("SELECT age FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(25));
    }

    #[test]
    fn table_level_check_constraint() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (a INTEGER, b INTEGER, CHECK (a < b))",
            [],
        ).unwrap();
        let ok = db.execute("INSERT INTO t VALUES (1, 2)", []);
        assert!(ok.is_ok());
        let err = db.execute("INSERT INTO t VALUES (2, 1)", []);
        assert!(err.is_err());
    }

    #[test]
    fn not_null_constraint_enforced() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)", []).unwrap();
        let err = db.execute("INSERT INTO t (name) VALUES (NULL)", []);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("NOT NULL constraint failed"));
        // UPDATE to NULL also fails.
        db.execute("INSERT INTO t (name) VALUES ('a')", []).unwrap();
        let err = db.execute("UPDATE t SET name = NULL", []);
        assert!(err.is_err());
    }

    #[test]
    fn datetime_functions_end_to_end() {
        let mut db = memdb();
        let rows = db.query("SELECT date('2023-07-14'), datetime('2023-07-14 13:45:28'), time('23:59:59')", []).unwrap();
        assert_eq!(rows[0][0], Value::Text("2023-07-14".into()));
        assert_eq!(rows[0][1], Value::Text("2023-07-14 13:45:28".into()));
        assert_eq!(rows[0][2], Value::Text("23:59:59".into()));

        let rows = db.query("SELECT julianday('1970-01-01'), unixepoch('2023-01-01 00:00:00')", []).unwrap();
        match &rows[0][0] {
            Value::Real(f) => assert!((f - 2440587.5).abs() < 1e-6),
            v => panic!("expected real, got {:?}", v),
        }
        assert_eq!(rows[0][1], Value::Integer(1672531200));

        let rows = db.query(
            "SELECT date('2023-01-31', '+1 day'), date('2023-07-14', 'start of month', '+2 days')",
            [],
        ).unwrap();
        assert_eq!(rows[0][0], Value::Text("2023-02-01".into()));
        assert_eq!(rows[0][1], Value::Text("2023-07-03".into()));

        let rows = db.query(
            "SELECT strftime('%Y|%m|%d %H:%M:%S', '2023-07-14 09:08:07'), strftime('%s', '1970-01-01 00:00:00')",
            [],
        ).unwrap();
        assert_eq!(rows[0][0], Value::Text("2023|07|14 09:08:07".into()));
        assert_eq!(rows[0][1], Value::Text("0".into()));
    }

    #[test]
    fn datetime_where_filter() {
        let mut db = memdb();
        db.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, day TEXT)", []).unwrap();
        db.execute("INSERT INTO events (day) VALUES ('2023-01-01'), ('2023-06-15'), ('2024-03-20')", []).unwrap();
        let rows = db.query(
            "SELECT day FROM events WHERE day > date('2023-01-01', '+90 days') ORDER BY day",
            [],
        ).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Text("2023-06-15".into()));
    }

    // ========================================================================
    // Subquery tests (scalar / IN / EXISTS)
    // ========================================================================

    #[test]
    fn scalar_subquery_in_select() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", []).unwrap();
        let rows = db.query("SELECT (SELECT MAX(x) FROM t) AS m", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(30));
        // Scalar subquery in WHERE.
        let rows = db.query("SELECT x FROM t WHERE x > (SELECT AVG(x) FROM t)", []).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(30));
    }

    #[test]
    fn scalar_subquery_empty_returns_null() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        let rows = db.query("SELECT (SELECT x FROM t WHERE id = 99)", []).unwrap();
        assert_eq!(rows[0][0], Value::Null);
    }

    #[test]
    fn in_subquery() {
        let mut db = memdb();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
        db.execute("INSERT INTO a (v) VALUES (1), (2), (3), (4)", []).unwrap();
        db.execute("INSERT INTO b (v) VALUES (2), (4), (6)", []).unwrap();
        let rows = db.query("SELECT v FROM a WHERE v IN (SELECT v FROM b) ORDER BY v", []).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(2));
        assert_eq!(rows[1][0], Value::Integer(4));
        // NOT IN
        let rows = db.query("SELECT v FROM a WHERE v NOT IN (SELECT v FROM b) ORDER BY v", []).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[1][0], Value::Integer(3));
    }

    #[test]
    fn exists_subquery() {
        let mut db = memdb();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
        db.execute("INSERT INTO a (v) VALUES (1), (2)", []).unwrap();
        // Empty b → EXISTS false, NOT EXISTS true.
        let rows = db.query("SELECT EXISTS (SELECT 1 FROM b)", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(0));
        let rows = db.query("SELECT NOT EXISTS (SELECT 1 FROM b)", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(1));
        db.execute("INSERT INTO b (v) VALUES (5)", []).unwrap();
        let rows = db.query("SELECT EXISTS (SELECT 1 FROM b)", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(1));
        // EXISTS in WHERE.
        let rows = db.query("SELECT v FROM a WHERE EXISTS (SELECT 1 FROM b)", []).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn nested_subqueries() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (5), (15), (25)", []).unwrap();
        // Subquery within a subquery: inner MIN=5, middle MIN(x WHERE x>5)=15.
        let rows = db.query(
            "SELECT x FROM t WHERE x > (SELECT MIN(x) FROM t WHERE x > (SELECT MIN(x) FROM t)) ORDER BY x",
            [],
        ).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(25));
    }

    #[test]
    fn subquery_with_parameters() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", []).unwrap();
        let rows = db.query(
            "SELECT x FROM t WHERE x > (SELECT AVG(x) FROM t WHERE x < ?)",
            vec![Value::Integer(30)],
        ).unwrap();
        // Subquery: AVG over rows where x < 30 → 15. Rows where x > 15: 20, 30.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(20));
        assert_eq!(rows[1][0], Value::Integer(30));
    }

    #[test]
    fn correlated_subquery_errors_cleanly() {
        let mut db = memdb();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)", []).unwrap();
        db.execute("INSERT INTO a (v) VALUES (1)", []).unwrap();
        // Correlated (a.v referenced inside subquery) → clean error, not a panic.
        let result = db.query("SELECT v FROM a WHERE v = (SELECT MAX(v) FROM b WHERE b.v = a.v)", []);
        assert!(result.is_err());
    }

    // ========================================================================
    // IndexRange tests (range predicates on indexed columns)
    // ========================================================================

    #[test]
    fn index_range_scan_select() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        db.execute("INSERT INTO t (val) VALUES (3), (1), (4), (1), (5), (9), (2), (6)", []).unwrap();
        // val > 3 → 4, 5, 6, 9 in index order.
        let rows = db.query("SELECT val FROM t WHERE val > 3", []).unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![4, 5, 6, 9]);
        // val >= 4 → same.
        let rows = db.query("SELECT val FROM t WHERE val >= 4", []).unwrap();
        assert_eq!(rows.len(), 4);
        // val < 3 → 1, 1, 2.
        let rows = db.query("SELECT val FROM t WHERE val < 3", []).unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![1, 1, 2]);
        // val <= 2 → same.
        let rows = db.query("SELECT val FROM t WHERE val <= 2", []).unwrap();
        assert_eq!(rows.len(), 3);
        // BETWEEN.
        let rows = db.query("SELECT val FROM t WHERE val BETWEEN 2 AND 5", []).unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![2, 3, 4, 5]);
        // Both bounds.
        let rows = db.query("SELECT val FROM t WHERE val > 1 AND val < 5", []).unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![2, 3, 4]);
    }

    #[test]
    fn index_range_with_residual() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, cat TEXT)", []).unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        db.execute("INSERT INTO t (val, cat) VALUES (1, 'a'), (2, 'b'), (3, 'a'), (4, 'b'), (5, 'a')", []).unwrap();
        // Range on val + residual on cat.
        let rows = db.query("SELECT val FROM t WHERE val > 1 AND cat = 'a'", []).unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![3, 5]);
    }

    #[test]
    fn index_range_update() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)", []).unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        let mut sql = String::from("INSERT INTO t (val, score) VALUES ");
        for i in 0..100 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, 0.0)", i + 1));
        }
        db.execute(&sql, []).unwrap();
        // UPDATE with a range predicate on the indexed column.
        db.execute("UPDATE t SET score = score + 1.0 WHERE val > 90", []).unwrap();
        let rows = db.query("SELECT COUNT(*) FROM t WHERE score > 0.5", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(10));
        // Also verify all rows with val <= 90 still have score 0.
        let rows = db.query("SELECT COUNT(*) FROM t WHERE score = 0.0", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(90));
        // DELETE with a range predicate.
        db.execute("DELETE FROM t WHERE val >= 95", []).unwrap();
        let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(94));
    }

    #[test]
    fn index_range_negative_and_real_values() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x REAL)", []).unwrap();
        db.execute("CREATE INDEX idx_x ON t(x)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (-5.5), (-2.0), (0.0), (1.5), (3.25), (7.0)", []).unwrap();
        let rows = db.query("SELECT x FROM t WHERE x > -2.5", []).unwrap();
        let vals: Vec<String> = rows.iter().map(|r| r[0].to_string()).collect();
        assert_eq!(vals, vec!["-2.0", "0.0", "1.5", "3.25", "7.0"]);
        let rows = db.query("SELECT x FROM t WHERE x < 1.5", []).unwrap();
        assert_eq!(rows.len(), 3);
        let rows = db.query("SELECT x FROM t WHERE x >= 1.5", []).unwrap();
        assert_eq!(rows.len(), 3);
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    fn memdb() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn roots_persist_across_reopen() {
        // Regression: B+tree splits moved table/index roots but the schema
        // rows kept the CREATE-time root — after reopen only the first
        // subtree was visible (10k-row table read back as ~1.8k rows).
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let mut db = Database::open(path).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 1..=10_000i64 {
                db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)]).unwrap();
            }
            db.execute("COMMIT", []).unwrap();
            db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
            let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
            assert_eq!(c[0][0], Value::Integer(10_000));
        }
        let db2 = Database::open(path).unwrap();
        let c = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(c[0][0], Value::Integer(10_000), "row count lost across reopen");
        let r = db2.query("SELECT val FROM t WHERE id = 5000", []).unwrap();
        assert_eq!(r[0][0], Value::Integer(5000), "row 5000 lost across reopen");
        // Index still works after reopen.
        let r2 = db2.query("SELECT id FROM t WHERE val = 7500", []).unwrap();
        assert_eq!(r2.len(), 1, "index lookup lost across reopen");
    }

    #[test]
    fn index_roots_survive_many_statements() {
        // Regression: index root moved by a split during statement N was
        // forgotten by statement N+1 (no override tracking) — entries past
        // the first split were silently unreachable.
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        for i in 1..=3_000i64 {
            db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i * 7)]).unwrap();
        }
        let mut missing = 0;
        for i in 1..=3_000i64 {
            let r = db.query("SELECT id FROM t WHERE val = ?", [Value::Integer(i * 7)]).unwrap();
            if r.len() != 1 {
                missing += 1;
            }
        }
        assert_eq!(missing, 0, "{} indexed rows unreachable", missing);
        // Range scans too.
        let r = db.query("SELECT COUNT(*) FROM t WHERE val > 14000", []).unwrap();
        assert_eq!(r[0][0], Value::Integer(1_000)); // i*7 > 14000 → i in 2001..3000
    }

    #[test]
    fn rollback_discards_root_moves() {
        // Splits inside a rolled-back transaction must not leak roots or
        // schema rewrites into the persisted state.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let mut db = Database::open(path).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", []).unwrap();
            db.execute("INSERT INTO t (val) VALUES (1)", []).unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 2..=5_000i64 {
                db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)]).unwrap();
            }
            db.execute("ROLLBACK", []).unwrap();
            let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
            assert_eq!(c[0][0], Value::Integer(1));
        }
        let db2 = Database::open(path).unwrap();
        let c = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(c[0][0], Value::Integer(1), "rollback leaked rows across reopen");
    }

    // ---- RowidRange fast path -------------------------------------------
    // These all run through `FastPath::RowidRange` when the plan shape is
    // `RowidRange { start: Some, end: Some, residual: None }` — the
    // pipeline-skipping OLTP path added alongside the binary-search
    // range scan in the B-tree.

    fn range_db(n: i64) -> Database {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)", [])
            .unwrap();
        // 10k rows → a multi-level tree so the interior-node binary
        // search and early-stop logic are exercised, not just one leaf.
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [Value::Text(format!("row-{i}")), Value::Integer(i * 7)],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn rowid_range_fast_path_bare_star() {
        let db = range_db(10_000);
        // BETWEEN on the INTEGER PRIMARY KEY alias.
        let rows = db.query("SELECT * FROM t WHERE id BETWEEN 1000 AND 1009", []).unwrap();
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0][0], Value::Integer(1000));
        assert_eq!(rows[9][0], Value::Integer(1009));
        assert_eq!(rows[0][1], Value::Text("row-1000".into()));
        // Projection form: only requested columns, in order.
        let rows = db.query("SELECT val, name FROM t WHERE id BETWEEN 1000 AND 1004", []).unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], Value::Integer(7000));
        assert_eq!(rows[0][1], Value::Text("row-1000".into()));
    }

    #[test]
    fn rowid_range_fast_path_conjunct_bounds() {
        let db = range_db(10_000);
        // >= / <= conjunct pair is the same plan shape as BETWEEN.
        let rows = db.query("SELECT id FROM t WHERE id >= 5000 AND id <= 5004", []).unwrap();
        assert_eq!(rows.len(), 5);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r[0], Value::Integer(5000 + i as i64));
        }
        // > / < (exclusive) still routes through the general range plan —
        // verify row count and edges are right there too.
        let rows = db.query("SELECT id FROM t WHERE id > 9990 AND id < 9999", []).unwrap();
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0][0], Value::Integer(9991));
        assert_eq!(rows[7][0], Value::Integer(9998));
    }

    #[test]
    fn rowid_range_fast_path_param_bounds() {
        let db = range_db(10_000);
        // Bound parameters hit `bind_expr` — make sure positional params
        // resolve against the same value vector the general path uses.
        let rows = db
            .query(
                "SELECT id FROM t WHERE id BETWEEN ? AND ?",
                [Value::Integer(4242), Value::Integer(4244)],
            )
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][0], Value::Integer(4243));
    }

    #[test]
    fn rowid_range_degenerate_and_edge_bounds() {
        let db = range_db(10_000);
        // Empty range: start > end → no rows, no panic, no infinite loop.
        let rows = db.query("SELECT id FROM t WHERE id BETWEEN 500 AND 400", []).unwrap();
        assert!(rows.is_empty());
        // Single-row range.
        let rows = db.query("SELECT id FROM t WHERE id BETWEEN 1 AND 1", []).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(1));
        // Full-table range via rowid bounds.
        let rows = db.query("SELECT COUNT(*) FROM t WHERE id BETWEEN 1 AND 20000", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(10_000));
        // Bounds beyond both ends clamp correctly.
        let rows = db.query("SELECT COUNT(*) FROM t WHERE id BETWEEN -5 AND 50000", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(10_000));
        // Range entirely past the right edge: the early-stop must kick in
        // at the first leaf without walking every remaining leaf.
        let rows = db.query("SELECT id FROM t WHERE id BETWEEN 10001 AND 20000", []).unwrap();
        assert!(rows.is_empty());
        // Range entirely before the left edge: the interior binary search
        // must skip to the first child, not panic.
        let rows = db.query("SELECT id FROM t WHERE id BETWEEN -100 AND -1", []).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn rowid_range_residual_falls_back_correctly() {
        let db = range_db(10_000);
        // A residual predicate keeps the plan off the fast path — the
        // general RowidRange executor must still filter it.
        let rows = db
            .query(
                "SELECT id FROM t WHERE id BETWEEN 1 AND 100 AND val > 350",
                [],
            )
            .unwrap();
        // val = id*7 > 350 → id >= 51, within [1, 100] → 50 rows.
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0][0], Value::Integer(51));
        assert_eq!(rows[49][0], Value::Integer(100));
    }

    #[test]
    fn rowid_range_after_deletes() {
        // Punch holes in the tree then range-scan across them: the
        // binary-search start lookup must land on the first surviving
        // rowid >= start even when earlier cells were deleted.
        let mut db = range_db(2_000);
        for i in (100..200).step_by(2) {
            db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(i)]).unwrap();
        }
        let rows = db.query("SELECT id FROM t WHERE id BETWEEN 99 AND 201", []).unwrap();
        // Survivors: 99, 101, 103, ..., 199, 200, 201
        let expect: Vec<Vec<Value>> = (99..=201)
            .filter(|i| *i == 99 || *i >= 200 || i % 2 == 1)
            .map(|i| vec![Value::Integer(i)])
            .collect();
        assert_eq!(rows, expect);
        let rows = db.query("SELECT COUNT(*) FROM t WHERE id BETWEEN 1 AND 2000", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(1_950)); // 2000 - 50 deleted
    }

    #[test]
    fn rowid_range_multi_page_spans() {
        // A range wide enough to cross many leaves AND interior nodes:
        // verifies the early-stop `Ok(false)` propagation never cuts the
        // walk short while rows remain inside the range.
        let db = range_db(10_000);
        let rows = db.query("SELECT id FROM t WHERE id BETWEEN 137 AND 9973", []).unwrap();
        assert_eq!(rows.len() as i64, 9973 - 137 + 1);
        assert_eq!(rows[0][0], Value::Integer(137));
        let last = rows.last().unwrap();
        assert_eq!(last[0], Value::Integer(9973));
        // Descending ranges are not the fast path, but must agree.
        let rows2 = db
            .query("SELECT id FROM t WHERE id BETWEEN 137 AND 9973 ORDER BY id DESC", [])
            .unwrap();
        assert_eq!(rows2.len(), rows.len());
        assert_eq!(rows2[0][0], Value::Integer(9973));
    }

    #[test]
    fn fast_paths_handle_reordered_and_duplicate_projections() {
        // Regression: `decode_row_selective` used to require ascending
        // column indices, so `SELECT val, name` on every fast path
        // (rowid point, rowid range, index point) silently returned NULL
        // for the out-of-order column, and `SELECT val, val` dropped the
        // duplicate. Fixed by decoding through a sorted-index permutation.
        let mut db = range_db(30);
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        // Rowid point lookup, reordered projection.
        let rows = db.query("SELECT val, name, id FROM t WHERE id = 5", []).unwrap();
        assert_eq!(rows, vec![vec![
            Value::Integer(35),
            Value::Text("row-5".into()),
            Value::Integer(5),
        ]]);
        // Rowid range, reordered projection.
        let rows = db.query("SELECT val, name FROM t WHERE id BETWEEN 5 AND 6", []).unwrap();
        assert_eq!(rows, vec![
            vec![Value::Integer(35), Value::Text("row-5".into())],
            vec![Value::Integer(42), Value::Text("row-6".into())],
        ]);
        // Index point lookup, reordered projection.
        let rows = db.query("SELECT val, name, id FROM t WHERE val = 35", []).unwrap();
        assert_eq!(rows, vec![vec![
            Value::Integer(35),
            Value::Text("row-5".into()),
            Value::Integer(5),
        ]]);
        // Duplicate projections on both point shapes.
        let rows = db.query("SELECT val, val FROM t WHERE id = 5", []).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(35), Value::Integer(35)]]);
        let rows = db.query("SELECT name, name FROM t WHERE val = 35", []).unwrap();
        assert_eq!(rows, vec![vec![
            Value::Text("row-5".into()),
            Value::Text("row-5".into()),
        ]]);
        // Single-column reorder: projection picks the LAST column only.
        let rows = db.query("SELECT val FROM t WHERE id = 7", []).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(49)]]);
        // Mid-table reorder mixing alias, text and integer.
        let rows = db.query("SELECT name, id, val FROM t WHERE id BETWEEN 9 AND 9", []).unwrap();
        assert_eq!(rows, vec![vec![
            Value::Text("row-9".into()),
            Value::Integer(9),
            Value::Integer(63),
        ]]);
    }
}
