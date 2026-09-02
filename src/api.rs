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
use crate::storage::btree::Btree;
use crate::storage::pager::Pager;
use crate::storage::row_codec::{decode_row, decode_row_selective, encode_row, encode_row_aliased};
use crate::types::{Row, Value};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// The maximum number of pages cached in memory.
const DEFAULT_CACHE_PAGES: usize = 2048;

/// Allocator-wake drain state: the delayed-free sweep happens once per
/// process (verified by examples/probe_rounds.rs), so a global flag
/// suffices — the first qualifying write burst drains it, everything
/// after runs clean.
static ALLOC_SETTLED: AtomicBool = AtomicBool::new(false);

/// Estimated freed blocks that must accumulate before a drain is worth
/// its ~170 µs cost: roughly 3.7k single-row write statements (or any
/// bulk txn / index backfill). Below this, a possible wake is smaller
/// than the drain itself.
const ALLOC_WAKE_THRESHOLD: u64 = 400_000;

/// Drain mimalloc's delayed-free wake: ~512 allocations across the small
/// size classes (8..128 bytes) force the page-acquisition sweep that
/// read-path allocations would otherwise pay after a bulk-write storm
/// (200-400 µs, measured in examples/probe_drain_cold.rs). The sweep
/// frees pages back to the size-class queues; every later allocation —
/// in any class — runs at steady-state cost. No-op cost when the
/// allocator has nothing pending (~2 µs for 512 empty Vec allocations).
#[cfg(not(feature = "mimalloc"))]
fn drain_mimalloc_wake() {}

#[cfg(feature = "mimalloc")]
fn drain_mimalloc_wake() {
    let mut sink: Vec<Vec<u8>> = Vec::with_capacity(512);
    for i in 0..512usize {
        sink.push(vec![0u8; (i % 16) * 8 + 8]);
    }
    drop(sink);
}

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

// Page size: 16 KiB (larger than SQLite's 4 KiB default) to reduce splits.
// This trades some memory for fewer B+tree splits and better scan locality.
// const DEFAULT_PAGE_SIZE: u32 = 16384;

/// A (object name, root page) pair used when syncing schema roots.
type RootEntry = (String, u32);

/// A database. Owns the pager and catalog.
///
/// All mutable state is wrapped in interior-mutability primitives
/// (`RwLock`/`Mutex`/`Atomic*`), so all public read methods take `&self`.
/// This lets N reader threads share a single `&Database` via `Arc<RwLock<Database>>`
/// and run queries concurrently. Writers take the outer write lock to get
/// `&mut Database`, which serializes them — but reads proceed without
/// blocking on the outer lock.
pub struct Database {
    pub(crate) pager: Pager,
    pub(crate) catalog: Catalog,
    path: PathBuf,
    /// Inside an explicit BEGIN..COMMIT/ROLLBACK transaction. Only mutated
    /// by the writer (which holds `&mut self` via the outer write lock),
    /// but wrapped for interior mutability so `&self` query paths can read it.
    pub(crate) in_transaction: AtomicBool,
    /// Snapshot taken at BEGIN, used by ROLLBACK to restore the pager's
    /// state to the pre-transaction point.
    pub(crate) txn_snapshot: Mutex<Option<crate::storage::pager::PagerSnapshot>>,
    /// SAVEPOINT support: bookkeeping-map snapshots aligned with the
    /// pager's savepoint stack (index i corresponds to pager savepoint i).
    /// ROLLBACK TO restores the maps Arc wholesale — root overrides and
    /// max-rowid caches move with the rolled-back pages.
    savepoint_maps: Mutex<Vec<std::sync::Arc<crate::executor::StmtMaps>>>,
    /// True when the open transaction was started by SAVEPOINT (not
    /// BEGIN) — releasing the outermost savepoint then COMMITS.
    savepoint_txn: AtomicBool,
    /// Combined bookkeeping maps (table root overrides, index roots,
    /// max-rowid cache) behind ONE Arc — a query snapshot is a single
    /// read-lock + one refcount bump (previously three separate
    /// `RwLock<Arc<HashMap>>` fields: 3 locks + 3 atomic bumps per query).
    /// Only writes (DML causing root splits / rowid-cache fills) take the
    /// write lock, and the writer (`&mut self`) detaches the Arc entirely.
    pub(crate) maps: RwLock<std::sync::Arc<crate::executor::StmtMaps>>,
    /// Fast-path guard for `maps`: false while the root-override maps are
    /// empty (the common case — they only gain entries after a B+tree
    /// split moves a root). A relaxed atomic load + branch replaces the
    /// read-lock on every fast-path point lookup. Writers hold `&mut self`
    /// (exclusive with all readers at the type level), so a stale `false`
    /// is impossible: the flag is refreshed at every maps attach site.
    pub(crate) maps_populated: AtomicBool,
    /// Monotonic write epoch: bumped by every statement that executes via
    /// `Database::execute` (all DML/DDL/transaction control) and by DML
    /// executed through prepared statements (see `statement.rs`). Readers
    /// (`&self`) can never bump it, so a value observed at the START of a
    /// read is stable for the read's duration. Keyes the
    /// `table_count_cache` below (SQLite's OP_Count walks the whole tree
    /// every time; we memoize per table and invalidate on any write).
    pub(crate) write_epoch: AtomicU64,
    /// Memoized `SELECT COUNT(*) FROM t` answers, keyed by lowercased table
    /// name → (write_epoch, row count). A hit requires the epoch to match
    /// the current one; ANY write statement invalidates every entry
    /// (conservative — cross-table writes just cost a re-walk). Writers
    /// are `&mut self`, so a walk performed under one epoch value cannot
    /// interleave with a write; the stored pair is always self-consistent.
    table_count_cache: RwLock<HashMap<String, (u64, i64)>>,
    /// Estimated allocator blocks freed by write statements since the last
    /// read-side settle (see `settle_allocator`). Bulk-write transactions
    /// free hundreds of thousands of small blocks (statement ASTs, encode
    /// buffers, payload Vecs); mimalloc defers the free-queue recovery,
    /// and the FIRST allocating READ after the storm pays a 200-570 µs
    /// wake. A ~15-20 µs allocation tap on the read side absorbs it —
    /// mirroring how SQLite's costs land inside the operations that cause
    /// them, not on the next unrelated query.
    alloc_burst: AtomicU64,
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
    /// Last statement returned by `get_or_cache_stmt` (SQL text + the same
    /// `Arc<CachedStmt>` the cache holds). Consecutive executions of the
    /// same statement text — the dominant OLTP pattern — skip the FxHash
    /// of the SQL text and the HashMap probe entirely: one read-lock, one
    /// memcmp, one refcount bump. Mirrors SQLite's caller-held prepared
    /// statement, where re-executing costs ZERO lookup.
    last_stmt: RwLock<Option<(String, Arc<CachedStmt>)>>,
    /// FIFO order of insertion into `stmt_cache`, used for eviction when the
    /// cache reaches `stmt_cache_capacity`. The first item in this Vec is the
    /// oldest entry and the next to be evicted.
    stmt_cache_order: Mutex<Vec<String>>,
    /// Maximum number of entries in the statement cache. Default 64.
    /// Immutable after open (only set via `set_stmt_cache_capacity`).
    stmt_cache_capacity: usize,
    /// Plugin registry: user functions, aggregates, collations,
    /// virtual-table modules, page codecs. One Arc snapshot per statement
    /// (installed into the thread-local plugin scope alongside the
    /// correlated-subquery bridge).
    pub(crate) plugins: RwLock<std::sync::Arc<crate::plugin::PluginRegistry>>,
    /// Fast path flag: true once anything is registered. Zero plugins
    /// (the overwhelmingly common library configuration) skips the
    /// plugin-scope guard entirely — one relaxed atomic load instead of
    /// a RwLock read + Arc clone + thread-local install per statement.
    has_plugins: std::sync::atomic::AtomicBool,
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
    pub(crate) deferred_flush: AtomicBool,
    /// Threshold for forcing a flush when `deferred_flush` is enabled.
    /// Default: 1000 dirty pages (~4 MB at 4 KiB page size).
    /// Immutable after open.
    deferred_flush_threshold: usize,
    /// Last rowid inserted on this connection (`sqlite3_last_insert_rowid`).
    /// Written by every DML completion site from `ExecContext::last_insert_rowid`;
    /// read by the C ABI layer (`sqlite3_last_insert_rowid`) so ORMs like
    /// sea-orm / sqlx get real ids after INSERTs.
    last_rowid: AtomicI64,
    /// Cross-statement INSERT chain (see [`InsertChain`]): consecutive
    /// single-row literal INSERTs to the same table with the same column
    /// list execute with near-zero per-statement overhead. Interior-
    /// mutable (`Mutex`) because the read paths (`query`) must be able to
    /// break (flush) a hot chain from `&self`.
    insert_chain: Mutex<Option<InsertChain>>,
}

/// Default capacity of the statement cache.
const DEFAULT_STMT_CACHE_CAPACITY: usize = 64;

/// Shared empty-maps singleton: cloning costs one refcount bump instead
/// of an ArcBox allocation, which matters on the per-statement detach /
/// attach path in `execute` and on the ROLLBACK reset.
fn empty_maps() -> Arc<crate::executor::StmtMaps> {
    static E: std::sync::OnceLock<Arc<crate::executor::StmtMaps>> = std::sync::OnceLock::new();
    E.get_or_init(|| Arc::new(crate::executor::StmtMaps::empty()))
        .clone()
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

/// Statement-cache map type with the fast string hasher. Private: only
/// `Database`'s internal `stmt_cache` field uses it.
type StmtCacheMap = HashMap<String, Arc<CachedStmt>, FxHashBuild>;

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
pub(crate) enum FastBound {
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
pub(crate) enum FastPath {
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
        /// When every key is a literal, the encoded index key is computed
        /// ONCE at statement-cache time — execution borrows these bytes
        /// instead of re-encoding (and re-allocating) per call.
        pre_encoded: Option<Arc<[u8]>>,
        project: Option<Vec<usize>>,
        columns: Arc<[String]>,
    },
    /// `SELECT COUNT(*) FROM t WHERE indexed_col = ?` — covering index
    /// count: the table is never touched; the answer is the number of
    /// index entries with the encoded key prefix.
    IndexCount {
        table: Arc<Table>,
        index: Arc<Index>,
        keys: Vec<FastBound>,
        pre_encoded: Option<Arc<[u8]>>,
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
    /// `SELECT COUNT(*) FROM t` (bare: no WHERE / GROUP BY / DISTINCT).
    /// Direct `Btree::count_rows` — sums `n_cells` across leaf pages with
    /// zero payload decoding and no ExecContext / executor dispatch.
    /// `COUNT(col)` (NULL-skipping) and DISTINCT keep the general path.
    CountStar {
        table: Arc<Table>,
        columns: Arc<[String]>,
    },
}

/// precomputed `has_subqueries` flag (mirrors SQLite's OP_Once check —
/// computed once at plan time instead of re-walking the plan tree, which
/// allocated a Vec of expr references, on every execution).
#[derive(Clone)]
pub(crate) struct CachedStmt {
    pub(crate) stmt: Arc<Statement>,
    pub(crate) plan: Option<Arc<crate::planner::plan::Plan>>,
    pub(crate) has_subqueries: bool,
    /// Pre-compiled point-lookup fast path (see `FastPath`).
    pub(crate) fast_path: Option<Arc<FastPath>>,
}

impl FastPath {
    /// The table this fast path reads from.
    #[inline]
    fn table_name(&self) -> &str {
        match self {
            FastPath::RowidPoint { table, .. } => &table.name,
            FastPath::IndexPoint { table, .. } => &table.name,
            FastPath::IndexCount { table, .. } => &table.name,
            FastPath::RowidRange { table, .. } => &table.name,
            FastPath::CountStar { table, .. } => &table.name,
        }
    }

    /// Output column names.
    #[inline]
    pub(crate) fn output_columns_public(&self) -> Arc<[String]> {
        self.output_columns().clone()
    }

    fn output_columns(&self) -> &Arc<[String]> {
        match self {
            FastPath::RowidPoint { columns, .. } => columns,
            FastPath::IndexPoint { columns, .. } => columns,
            FastPath::IndexCount { columns, .. } => columns,
            FastPath::RowidRange { columns, .. } => columns,
            FastPath::CountStar { columns, .. } => columns,
        }
    }
}

/// Decode a row payload with an optional column projection.
/// `None` decodes all columns (identity — SELECT *); `Some(indices)` uses
/// the selective decoder, which skips over un-projected columns without
/// allocating Values for them.
#[inline]
/// Pre-encode the index key for an IndexPoint fast path when every bound
/// key is a literal (constants in the SQL text). Returns None when any
/// key is a parameter (re-encoded per call against the param values).
fn pre_encode_literal_keys(keys: &[FastBound]) -> Option<Arc<[u8]>> {
    if !keys.iter().all(|k| matches!(k, FastBound::Literal(_))) {
        return None;
    }
    let mut buf = Vec::with_capacity(keys.len() * 8);
    for k in keys {
        if let FastBound::Literal(v) = k {
            v.encode_order_key_into(&mut buf);
        }
    }
    Some(buf.into())
}

fn decode_projected(
    payload: &[u8],
    table: &Table,
    rowid: i64,
    project: Option<&[usize]>,
) -> Result<Row> {
    match project {
        Some(idxs) => {
            let mut out = Vec::with_capacity(idxs.len());
            decode_row_selective(
                payload,
                table.n_columns(),
                idxs,
                rowid,
                table.rowid_alias,
                &mut out,
            )?;
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
) -> Result<(SelectStatement, crate::sql::ast::SetOp, SelectStatement)> {
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

// ====================================================================
// ALTER TABLE RENAME COLUMN / DROP COLUMN support
// ====================================================================

/// Serialize a `ColumnDef` back to SQL text.
fn column_def_to_sql(cd: &crate::sql::ast::ColumnDef) -> String {
    use crate::sql::ast::ColumnConstraint as C;
    let mut s = cd.name.clone();
    if !cd.type_name.is_empty() {
        s.push(' ');
        s.push_str(&cd.type_name);
    }
    for c in &cd.constraints {
        s.push(' ');
        match c {
            C::PrimaryKey {
                autoincrement,
                order,
            } => {
                s.push_str("PRIMARY KEY");
                if *order == crate::sql::ast::Order::Desc {
                    s.push_str(" DESC");
                }
                if *autoincrement {
                    s.push_str(" AUTOINCREMENT");
                }
            }
            C::NotNull => s.push_str("NOT NULL"),
            C::Null => s.push_str("NULL"),
            C::Unique => s.push_str("UNIQUE"),
            C::Check(e) => s.push_str(&format!("CHECK ({})", expr_to_sql(e))),
            C::Default(e) => s.push_str(&format!("DEFAULT {}", expr_to_sql(e))),
            C::Collate(c) => s.push_str(&format!("COLLATE {}", c)),
            C::References {
                table,
                columns,
                on_delete,
                on_update,
            } => {
                s.push_str(&format!("REFERENCES {}", table));
                if !columns.is_empty() {
                    s.push_str(&format!("({})", columns.join(", ")));
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
                    s.push_str(&format!(" ON DELETE {}", act(on_delete)));
                }
                if !matches!(on_update, NoAction) {
                    s.push_str(&format!(" ON UPDATE {}", act(on_update)));
                }
            }
            C::GeneratedAs { expr, stored } => {
                s.push_str(&format!(
                    "GENERATED ALWAYS AS ({}){}",
                    expr_to_sql(expr),
                    if *stored { " STORED" } else { "" }
                ));
            }
        }
    }
    s
}

fn indexed_columns_to_sql(cols: &[crate::sql::ast::IndexedColumn]) -> String {
    cols.iter()
        .map(|c| {
            let mut s = c.name.clone();
            if let Some(coll) = &c.collation {
                s.push_str(&format!(" COLLATE {}", coll));
            }
            if c.order == crate::sql::ast::Order::Desc {
                s.push_str(" DESC");
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Serialize a full CREATE TABLE statement from its parsed parts.
fn create_table_to_sql(
    name: &str,
    columns: &[crate::sql::ast::ColumnDef],
    constraints: &[crate::sql::ast::TableConstraint],
    without_rowid: bool,
    strict: bool,
) -> String {
    use crate::sql::ast::TableConstraint as T;
    let mut parts: Vec<String> = columns.iter().map(column_def_to_sql).collect();
    for tc in constraints {
        let s = match tc {
            T::PrimaryKey { columns } => {
                format!("PRIMARY KEY ({})", indexed_columns_to_sql(columns))
            }
            T::Unique(cols) => format!("UNIQUE ({})", indexed_columns_to_sql(cols)),
            T::Check(e) => format!("CHECK ({})", expr_to_sql(e)),
            T::ForeignKey {
                columns,
                ref_table,
                ref_columns,
                on_delete,
                on_update,
            } => {
                let mut s = format!(
                    "FOREIGN KEY ({}) REFERENCES {}",
                    columns.join(", "),
                    ref_table
                );
                if !ref_columns.is_empty() {
                    s.push_str(&format!("({})", ref_columns.join(", ")));
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
                    s.push_str(&format!(" ON DELETE {}", act(on_delete)));
                }
                if !matches!(on_update, NoAction) {
                    s.push_str(&format!(" ON UPDATE {}", act(on_update)));
                }
                s
            }
        };
        parts.push(s);
    }
    let mut sql = format!("CREATE TABLE {} ({})", name, parts.join(", "));
    if without_rowid {
        sql.push_str(" WITHOUT ROWID");
    }
    if strict {
        sql.push_str(" STRICT");
    }
    sql
}

/// Rename `old` -> `new` inside an expression's column references.
/// `qualifier` is this table's name (matches qualified refs); unqualified
/// refs are table-local in CHECK/DEFAULT/GENERATED contexts.
fn rename_column_in_expr(e: &mut Expr, old: &str, new: &str, qualifier: &str) {
    match e {
        Expr::Column { table, name } => {
            let matches_qual = table
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case(qualifier))
                .unwrap_or(true);
            if matches_qual && name.eq_ignore_ascii_case(old) {
                *name = new.to_string();
                if table.is_some() {
                    // Keep the original qualifier casing; only the column name changes.
                }
            }
        }
        Expr::Binary { left, right, .. } => {
            rename_column_in_expr(left, old, new, qualifier);
            rename_column_in_expr(right, old, new, qualifier);
        }
        Expr::Unary { expr, .. } => rename_column_in_expr(expr, old, new, qualifier),
        Expr::Function { args, .. } => {
            for a in args.iter_mut() {
                rename_column_in_expr(a, old, new, qualifier);
            }
        }
        Expr::Row(items) => {
            for it in items.iter_mut() {
                rename_column_in_expr(it, old, new, qualifier);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(op) = operand {
                rename_column_in_expr(op, old, new, qualifier);
            }
            for (w, t) in whens.iter_mut() {
                rename_column_in_expr(w, old, new, qualifier);
                rename_column_in_expr(t, old, new, qualifier);
            }
            if let Some(el) = else_ {
                rename_column_in_expr(el, old, new, qualifier);
            }
        }
        Expr::In { expr, source, .. } => {
            rename_column_in_expr(expr, old, new, qualifier);
            if let crate::sql::ast::InSource::List(items) = source {
                for it in items.iter_mut() {
                    rename_column_in_expr(it, old, new, qualifier);
                }
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rename_column_in_expr(expr, old, new, qualifier);
            rename_column_in_expr(low, old, new, qualifier);
            rename_column_in_expr(high, old, new, qualifier);
        }
        Expr::Cast { expr, .. } => rename_column_in_expr(expr, old, new, qualifier),
        Expr::Collate { expr, .. } => rename_column_in_expr(expr, old, new, qualifier),
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            rename_column_in_expr(expr, old, new, qualifier);
            rename_column_in_expr(pattern, old, new, qualifier);
            if let Some(e) = escape {
                rename_column_in_expr(e, old, new, qualifier);
            }
        }
        Expr::IsNull { expr, .. } => rename_column_in_expr(expr, old, new, qualifier),
        Expr::Is { left, right, .. } => {
            rename_column_in_expr(left, old, new, qualifier);
            rename_column_in_expr(right, old, new, qualifier);
        }
        _ => {}
    }
}

/// Does an expression reference column `old` (qualified by `qualifier` or
/// unqualified)? Used by DROP COLUMN rejection checks.
fn expr_references_column(e: &Expr, old: &str, qualifier: &str) -> bool {
    match e {
        Expr::Column { table, name } => {
            let matches_qual = table
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case(qualifier))
                .unwrap_or(true);
            matches_qual && name.eq_ignore_ascii_case(old)
        }
        Expr::Binary { left, right, .. } => {
            expr_references_column(left, old, qualifier)
                || expr_references_column(right, old, qualifier)
        }
        Expr::Unary { expr, .. } => expr_references_column(expr, old, qualifier),
        Expr::Function { args, filter, .. } => {
            args.iter()
                .any(|a| expr_references_column(a, old, qualifier))
                || filter
                    .as_ref()
                    .map(|f| expr_references_column(f, old, qualifier))
                    .unwrap_or(false)
        }
        Expr::Row(items) => items
            .iter()
            .any(|it| expr_references_column(it, old, qualifier)),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand
                .as_ref()
                .map(|o| expr_references_column(o, old, qualifier))
                .unwrap_or(false)
                || whens.iter().any(|(w, t)| {
                    expr_references_column(w, old, qualifier)
                        || expr_references_column(t, old, qualifier)
                })
                || else_
                    .as_ref()
                    .map(|el| expr_references_column(el, old, qualifier))
                    .unwrap_or(false)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_references_column(expr, old, qualifier)
                || expr_references_column(pattern, old, qualifier)
                || escape
                    .as_ref()
                    .map(|e| expr_references_column(e, old, qualifier))
                    .unwrap_or(false)
        }
        Expr::IsNull { expr, .. } => expr_references_column(expr, old, qualifier),
        Expr::Is { left, right, .. } => {
            expr_references_column(left, old, qualifier)
                || expr_references_column(right, old, qualifier)
        }
        Expr::In { expr, source, .. } => {
            expr_references_column(expr, old, qualifier)
                || matches!(source, crate::sql::ast::InSource::List(items) if items.iter().any(|it| expr_references_column(it, old, qualifier)))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_references_column(expr, old, qualifier)
                || expr_references_column(low, old, qualifier)
                || expr_references_column(high, old, qualifier)
        }
        Expr::Cast { expr, .. } => expr_references_column(expr, old, qualifier),
        Expr::Collate { expr, .. } => expr_references_column(expr, old, qualifier),
        _ => false,
    }
}

/// Rewrite a CREATE TABLE statement, renaming column `old` to `new`
/// everywhere it appears as THIS table's column: the column definition
/// name, CHECK/DEFAULT/GENERATED expressions, table-constraint column
/// lists, and this table's child-FK column lists. Parent-side references
/// (REFERENCES other(...)) are left alone — other tables' FK clauses are
/// rewritten separately.
fn rename_column_in_create_table(
    sql: &str,
    table_name: &str,
    old: &str,
    new: &str,
) -> Result<String> {
    use crate::sql::ast::ColumnConstraint as C;
    use crate::sql::ast::TableConstraint as T;
    let stmt = crate::sql::parser::parse(sql)?;
    if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
        name,
        columns,
        constraints,
        without_rowid,
        strict,
        ..
    }) = stmt
    {
        let mut columns = columns;
        let mut constraints = constraints;
        for cd in columns.iter_mut() {
            if cd.name.eq_ignore_ascii_case(old) {
                cd.name = new.to_string();
            }
            for c in cd.constraints.iter_mut() {
                match c {
                    C::Check(e) | C::Default(e) => rename_column_in_expr(e, old, new, table_name),
                    C::GeneratedAs { expr, .. } => rename_column_in_expr(expr, old, new, table_name),
                    C::References { table, columns, .. }
                        // Child-side columns of THIS table's FK.
                        if table.eq_ignore_ascii_case(table_name) => {
                            for cn in columns.iter_mut() {
                                if cn.eq_ignore_ascii_case(old) {
                                    *cn = new.to_string();
                                }
                            }
                        }
                    _ => {}
                }
            }
        }
        for tc in constraints.iter_mut() {
            match tc {
                T::PrimaryKey { columns } | T::Unique(columns) => {
                    for ic in columns.iter_mut() {
                        if ic.name.eq_ignore_ascii_case(old) {
                            ic.name = new.to_string();
                        }
                    }
                }
                T::Check(e) => rename_column_in_expr(e, old, new, table_name),
                T::ForeignKey {
                    columns,
                    ref_table,
                    ref_columns,
                    ..
                } => {
                    for cn in columns.iter_mut() {
                        if cn.eq_ignore_ascii_case(old) {
                            *cn = new.to_string();
                        }
                    }
                    if ref_table.eq_ignore_ascii_case(table_name) {
                        for rc in ref_columns.iter_mut() {
                            if rc.eq_ignore_ascii_case(old) {
                                *rc = new.to_string();
                            }
                        }
                    }
                }
            }
        }
        Ok(create_table_to_sql(
            &name.name,
            &columns,
            &constraints,
            without_rowid,
            strict,
        ))
    } else {
        Err(Error::corruption(
            "schema row is not a CREATE TABLE statement",
        ))
    }
}

/// Rewrite a CREATE TABLE statement, renaming `old` -> `new` ONLY in
/// REFERENCES <target_table> (...) parent-column lists (used when another
/// table's column is renamed and this table points at it).
fn rename_fk_refs_in_create_table(
    sql: &str,
    target_table: &str,
    old: &str,
    new: &str,
) -> Result<String> {
    use crate::sql::ast::ColumnConstraint as C;
    use crate::sql::ast::TableConstraint as T;
    let stmt = crate::sql::parser::parse(sql)?;
    if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
        name,
        columns,
        constraints,
        without_rowid,
        strict,
        ..
    }) = stmt
    {
        let mut columns = columns;
        let mut constraints = constraints;
        let rename_refs = |cols: &mut Vec<String>| {
            for cn in cols.iter_mut() {
                if cn.eq_ignore_ascii_case(old) {
                    *cn = new.to_string();
                }
            }
        };
        for cd in columns.iter_mut() {
            for c in cd.constraints.iter_mut() {
                if let C::References { table, columns, .. } = c {
                    if table.eq_ignore_ascii_case(target_table) {
                        rename_refs(columns);
                    }
                }
            }
        }
        for tc in constraints.iter_mut() {
            if let T::ForeignKey {
                ref_table,
                ref_columns,
                ..
            } = tc
            {
                if ref_table.eq_ignore_ascii_case(target_table) {
                    rename_refs(ref_columns);
                }
            }
        }
        Ok(create_table_to_sql(
            &name.name,
            &columns,
            &constraints,
            without_rowid,
            strict,
        ))
    } else {
        Err(Error::corruption(
            "schema row is not a CREATE TABLE statement",
        ))
    }
}

/// Rewrite a CREATE INDEX statement, renaming the indexed column.
fn rename_column_in_create_index(sql: &str, old: &str, new: &str) -> Result<String> {
    let stmt = crate::sql::parser::parse(sql)?;
    if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Index {
        unique,
        name,
        table,
        columns,
        where_clause,
        ..
    }) = stmt
    {
        let mut columns = columns;
        for ic in columns.iter_mut() {
            if ic.name.eq_ignore_ascii_case(old) {
                ic.name = new.to_string();
            }
        }
        let mut sql = format!(
            "CREATE {}INDEX {} ON {} ({})",
            if unique { "UNIQUE " } else { "" },
            name,
            table,
            indexed_columns_to_sql(&columns)
        );
        if let Some(w) = &where_clause {
            sql.push_str(&format!(" WHERE {}", expr_to_sql(w)));
        }
        Ok(sql)
    } else {
        Err(Error::corruption(
            "schema row is not a CREATE INDEX statement",
        ))
    }
}

/// Position-aware identifier rewriter for trigger/view SQL. Tokenizes the
/// text (strings, quoted identifiers, blob literals and comments are
/// skipped verbatim) and replaces identifier tokens that match `old`
/// (case-insensitively) in column positions referring to `table`:
///
///   - qualified: `table.old`, `alias.old`, `NEW.old`, `OLD.old`
///   - `UPDATE OF old` (trigger event lists)
///   - `INSERT INTO table (..., old, ...)` column lists
///   - `UPDATE [table|alias] SET old = ...` assignment targets
///
/// Aliases of the target table are auto-detected from
/// `FROM|JOIN <table> [AS] <ident>` patterns.
///
/// `unqualified`: when true, ALSO rename bare (unqualified) identifiers
/// matching `old` — correct for single-table views over the target and for
/// triggers ON the target (their unqualified names resolve to the trigger
/// table per SQLite's lookup order). When false, only the disambiguated
/// positions above are rewritten.
fn rename_column_in_object_sql(
    sql: &str,
    table: &str,
    old: &str,
    new: &str,
    unqualified: bool,
) -> String {
    #[derive(Clone, Debug, PartialEq)]
    enum Tk {
        Ident(String),
        Str(String),
        Other(String),
    }

    fn tokenize(sql: &str) -> Vec<(Tk, usize, usize)> {
        let b = sql.as_bytes();
        let mut out: Vec<(Tk, usize, usize)> = Vec::new();
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];
            let start = i;
            if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out.push((Tk::Other(sql[start..i].to_string()), start, i));
            } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out.push((Tk::Other(sql[start..i].to_string()), start, i));
            } else if c == b'\'' {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                out.push((Tk::Str(sql[start..i].to_string()), start, i));
            } else if c == b'"' || c == b'`' || c == b'[' {
                let close = if c == b'"' {
                    b'"'
                } else if c == b'`' {
                    b'`'
                } else {
                    b']'
                };
                i += 1;
                while i < b.len() && b[i] != close {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                out.push((
                    Tk::Ident(sql[start + 1..i.saturating_sub(1)].to_string()),
                    start,
                    i,
                ));
            } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80)
                {
                    i += 1;
                }
                out.push((Tk::Ident(sql[start..i].to_string()), start, i));
            } else if c.is_ascii_digit() {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                out.push((Tk::Other(sql[start..i].to_string()), start, i));
            } else {
                i += 1;
                out.push((Tk::Other(sql[start..i].to_string()), start, i));
            }
        }
        out
    }

    let toks = tokenize(sql);
    let n = toks.len();
    let as_str = |i: usize| -> Option<&str> {
        match &toks.get(i)?.0 {
            Tk::Ident(s) => Some(s.as_str()),
            _ => None,
        }
    };
    let is_kw = |s: &str| -> bool {
        matches!(
            s.to_ascii_uppercase().as_str(),
            "SELECT"
                | "FROM"
                | "WHERE"
                | "AND"
                | "OR"
                | "NOT"
                | "VALUES"
                | "INSERT"
                | "UPDATE"
                | "DELETE"
                | "SET"
                | "INTO"
                | "OF"
                | "JOIN"
                | "ON"
                | "AS"
                | "WHEN"
                | "THEN"
                | "ELSE"
                | "CASE"
                | "END"
                | "IS"
                | "NULL"
                | "IN"
                | "BETWEEN"
                | "LIKE"
                | "GLOB"
                | "REGEXP"
                | "MATCH"
                | "ESCAPE"
                | "CAST"
                | "COLLATE"
                | "ASC"
                | "DESC"
                | "IF"
                | "EXISTS"
                | "BEGIN"
                | "BEFORE"
                | "AFTER"
                | "INSTEAD"
                | "FOR"
                | "EACH"
                | "ROW"
                | "TRIGGER"
                | "VIEW"
                | "TABLE"
                | "INDEX"
                | "CREATE"
                | "REPLACE"
                | "CONFLICT"
                | "DO"
                | "NOTHING"
                | "RETURNING"
                | "LIMIT"
                | "ORDER"
                | "BY"
                | "GROUP"
                | "HAVING"
                | "DISTINCT"
                | "ALL"
                | "UNION"
                | "EXCEPT"
                | "INTERSECT"
                | "PRIMARY"
                | "KEY"
                | "REFERENCES"
                | "DEFAULT"
                | "CHECK"
                | "UNIQUE"
                | "AUTOINCREMENT"
        )
    };

    // Pass 1: aliases of the target table (FROM|JOIN <table> [AS] <ident>).
    let mut aliases: Vec<String> = Vec::new();
    for i in 0..n {
        let up = as_str(i).map(|s| s.to_ascii_uppercase());
        if up.as_deref() == Some("FROM") || up.as_deref() == Some("JOIN") {
            if let Some(next) = as_str(i + 1) {
                if next.eq_ignore_ascii_case(table) {
                    if let Some(a) = as_str(i + 2) {
                        if a.eq_ignore_ascii_case("AS") {
                            if let Some(a2) = as_str(i + 3) {
                                aliases.push(a2.to_string());
                            }
                        } else if !is_kw(a) {
                            aliases.push(a.to_string());
                        }
                    }
                }
            }
        }
    }
    let is_target_ref = |s: &str| -> bool {
        s.eq_ignore_ascii_case(table) || aliases.iter().any(|a| s.eq_ignore_ascii_case(a))
    };

    // Pass 2: mark tokens to replace.
    let mut replace = vec![false; n];
    // 2a. Qualified references: <table|alias|NEW|OLD> . old — and bare
    // references when `unqualified` is set (but never a keyword position,
    // a table-name position, or a CREATE header).
    for i in 0..n {
        if let Some(s) = as_str(i) {
            if s.eq_ignore_ascii_case(old) {
                if unqualified && !is_kw(s) {
                    // Bare occurrence: skip if it's qualified (handled
                    // below), or it's a property of a dotted prefix
                    // written the other way (t.old already covered), or it
                    // sits right after a '.' (someone's column already).
                    let prev_is_dot = i >= 1 && matches!(&toks[i - 1].0, Tk::Other(o) if o == ".");
                    let next_is_dot =
                        i + 1 < n && matches!(&toks[i + 1].0, Tk::Other(o) if o == ".");
                    if !prev_is_dot && !next_is_dot {
                        replace[i] = true;
                    }
                }
                if i >= 2 {
                    let prev_is_dot = matches!(&toks[i - 1].0, Tk::Other(o) if o == ".");
                    if prev_is_dot {
                        if let Some(q) = as_str(i - 2) {
                            if is_target_ref(q)
                                || q.eq_ignore_ascii_case("NEW")
                                || q.eq_ignore_ascii_case("OLD")
                            {
                                replace[i] = true;
                            }
                        }
                    }
                }
            }
        }
    }
    // 2b. Statement-shape positions.
    let mut i = 0usize;
    while i < n {
        let up = as_str(i).map(|s| s.to_ascii_uppercase());
        match up.as_deref() {
            Some("INSERT") => {
                // INSERT [OR <conflict>] INTO <table> [ ( collist ) ]
                let mut j = i + 1;
                if as_str(j)
                    .map(|s| s.eq_ignore_ascii_case("OR"))
                    .unwrap_or(false)
                {
                    j += 2; // skip OR + conflict action
                }
                if as_str(j)
                    .map(|s| s.eq_ignore_ascii_case("INTO"))
                    .unwrap_or(false)
                {
                    j += 1;
                    let target = as_str(j).map(is_target_ref).unwrap_or(false);
                    j += 1;
                    // Optional column list: '(' ident[, ident]* ')'
                    if target
                        && matches!(&toks.get(j).map(|t| &t.0), Some(Tk::Other(o)) if o == "(")
                    {
                        j += 1;
                        while j < n {
                            if let Tk::Ident(s) = &toks[j].0 {
                                if s.eq_ignore_ascii_case(old) {
                                    replace[j] = true;
                                }
                                j += 1;
                            } else if matches!(&toks[j].0, Tk::Other(o) if o == ",") {
                                j += 1;
                            } else {
                                break;
                            }
                        }
                    }
                }
                i = j;
            }
            Some("UPDATE") => {
                // UPDATE [OR <conflict>] <table|OF> ...
                let mut j = i + 1;
                if as_str(j)
                    .map(|s| s.eq_ignore_ascii_case("OR"))
                    .unwrap_or(false)
                {
                    j += 2;
                }
                if as_str(j)
                    .map(|s| s.eq_ignore_ascii_case("OF"))
                    .unwrap_or(false)
                {
                    // Trigger UPDATE OF <col-list>: refers to the trigger's
                    // table, which IS the target.
                    j += 1;
                    while j < n {
                        if let Some(s) = as_str(j) {
                            if s.eq_ignore_ascii_case(old) {
                                replace[j] = true;
                            }
                            j += 1;
                        } else if matches!(&toks[j].0, Tk::Other(o) if o == ",") {
                            j += 1;
                        } else {
                            break;
                        }
                    }
                    i = j;
                } else {
                    let target = as_str(j).map(is_target_ref).unwrap_or(false);
                    j += 1;
                    // Scan forward for the SET keyword at this nesting
                    // level; then rename LHS idents followed by '='.
                    let mut depth = 0i32;
                    let mut in_set = false;
                    while j < n {
                        match &toks[j].0 {
                            Tk::Other(o) if o == "(" => {
                                depth += 1;
                                j += 1;
                            }
                            Tk::Other(o) if o == ")" => {
                                depth -= 1;
                                if depth < 0 {
                                    break;
                                }
                                j += 1;
                            }
                            Tk::Ident(s) if depth == 0 => {
                                let u = s.to_ascii_uppercase();
                                if u == "SET" {
                                    in_set = true;
                                    j += 1;
                                } else if in_set
                                    && (u == "WHERE"
                                        || u == "RETURNING"
                                        || u == "ORDER"
                                        || u == "LIMIT")
                                {
                                    break;
                                } else if in_set && s.eq_ignore_ascii_case(old) {
                                    // LHS of an assignment? next token '='
                                    if matches!(&toks.get(j + 1).map(|t| &t.0), Some(Tk::Other(o)) if o == "=")
                                        && target
                                    {
                                        replace[j] = true;
                                    }
                                    j += 1;
                                } else {
                                    j += 1;
                                }
                            }
                            _ => j += 1,
                        }
                    }
                    i = j;
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    // Pass 3: splice.
    let mut out = String::with_capacity(sql.len() + 16);
    let mut last = 0usize;
    for (idx, (_tk, start, end)) in toks.iter().enumerate() {
        if replace[idx] {
            out.push_str(&sql[last..*start]);
            out.push_str(new);
            last = *end;
        }
    }
    out.push_str(&sql[last..]);
    out
}

/// Is a view's SELECT a single-table read of `table` (no joins, no
/// subqueries)? Then every unqualified column in it resolves to `table`.
fn view_is_single_table_over(select: &crate::sql::ast::SelectStatement, table: &str) -> bool {
    use crate::sql::ast::{SelectBody, TableExpression};
    fn body_single(b: &SelectBody, table: &str) -> bool {
        match b {
            SelectBody::Simple(s) => match &s.from {
                Some(TableExpression::Table { name, .. }) => name.eq_ignore_ascii_case(table),
                _ => false,
            },
            SelectBody::Binary { left, right, .. } => {
                body_single(left, table) && body_single(right, table)
            }
        }
    }
    body_single(&select.body, table)
}

/// Rewrite trigger/view schema rows whose SQL references the renamed
/// column of `table`. The catalog entries and the persisted schema rows
/// are both updated.
fn rewrite_object_sql_column_refs(
    pager: &Pager,
    catalog: &mut Catalog,
    table: &str,
    old: &str,
    new: &str,
) -> Result<()> {
    let mut schema_updates: Vec<(i64, Vec<Value>)> = Vec::new();
    {
        let mut bt = Btree::new(pager, 0, false);
        bt.scan_table(|rowid, payload| {
            if let Ok(row) = decode_row(payload, 5, 0, None) {
                if let Some((kind, name, tbl_name, rootpage, sql)) =
                    crate::schema::decode_schema_row(&row)
                {
                    if (kind == "view" || kind == "trigger") && !sql.is_empty() {
                        // Views: rename bare references only when the view
                        // reads exactly this table (unqualified names then
                        // resolve to it). Triggers on this table: bare
                        // references resolve to it.
                        let unq = if kind == "trigger" {
                            tbl_name.eq_ignore_ascii_case(table)
                        } else {
                            catalog
                                .get_view(name)
                                .map(|v| view_is_single_table_over(&v.select, table))
                                .unwrap_or(false)
                        };
                        let new_sql = rename_column_in_object_sql(sql, table, old, new, unq);
                        if new_sql != sql {
                            schema_updates.push((
                                rowid,
                                crate::schema::encode_schema_row(
                                    kind, name, tbl_name, rootpage, &new_sql,
                                ),
                            ));
                        }
                    }
                }
            }
            true
        })?;
    }
    // Apply schema-row rewrites.
    {
        let mut bt = Btree::new(pager, 0, false);
        for (rowid, row) in &schema_updates {
            let payload = crate::storage::row_codec::encode_row(row);
            let did = bt.update_table(*rowid, &payload).unwrap_or(false);
            if !did {
                bt.delete_table(*rowid)?;
                bt.insert_table(*rowid, &payload)?;
            }
        }
    }
    // Refresh catalog entries (re-parse from the new SQL).
    for (rowid, row) in &schema_updates {
        let _ = rowid;
        if let Some((_kind, _name, _tbl, _root, sql)) = crate::schema::decode_schema_row(row) {
            if let Ok(stmt) = crate::sql::parser::parse(sql) {
                match stmt {
                    crate::sql::ast::Statement::Create(
                        crate::sql::ast::CreateStatement::View {
                            name,
                            columns,
                            select,
                            ..
                        },
                    ) => {
                        let vname = name.name.clone();
                        catalog.replace_view(
                            &vname,
                            crate::schema::View {
                                name: vname.clone(),
                                columns,
                                select: *select,
                                create_sql: sql.to_string(),
                            },
                        );
                    }
                    crate::sql::ast::Statement::Create(
                        crate::sql::ast::CreateStatement::Trigger(ct),
                    ) => {
                        let tname2 = ct.name.clone();
                        catalog.replace_trigger(
                            &tname2,
                            crate::schema::Trigger {
                                name: tname2.clone(),
                                table: ct.table.clone(),
                                when: ct.when,
                                events: ct.events,
                                for_each_row: ct.for_each_row,
                                when_clause: ct.when_clause,
                                body: ct.body,
                                create_sql: sql.to_string(),
                            },
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Does a trigger/view's stored SQL reference column `name` of `table`?
/// Conservative: any qualified reference (table/alias/NEW/OLD-qualified),
/// UPDATE OF list entry, or INSERT column-list entry counts.
fn object_sql_references_column(sql: &str, table: &str, name: &str, unqualified: bool) -> bool {
    // Reuse the rewriter machinery: rewrite to a sentinel name and see if
    // any position changed.
    let probe = rename_column_in_object_sql(sql, table, name, "\u{1}__renamed__\u{1}", unqualified);
    probe != sql
}

/// Remove a column definition from a CREATE TABLE statement, returning the
/// new SQL. Returns `None` when the column isn't found.
fn drop_column_from_create_table(sql: &str, name: &str) -> Result<Option<String>> {
    let stmt = crate::sql::parser::parse(sql)?;
    if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
        name: tname,
        columns,
        constraints,
        without_rowid,
        strict,
        ..
    }) = stmt
    {
        let before = columns.len();
        let columns: Vec<crate::sql::ast::ColumnDef> = columns
            .into_iter()
            .filter(|c| !c.name.eq_ignore_ascii_case(name))
            .collect();
        if columns.len() == before {
            return Ok(None);
        }
        Ok(Some(create_table_to_sql(
            &tname.name,
            &columns,
            &constraints,
            without_rowid,
            strict,
        )))
    } else {
        Err(Error::corruption(
            "schema row is not a CREATE TABLE statement",
        ))
    }
}

/// Pre-classification of savepoint-adjacent statements for execute()'s
/// interception (the static dispatcher has no &self for map snapshots).
enum StmtPre {
    Savepoint(String),
    Release(String),
    RollbackTo(String),
    Other,
}

fn stmt_ref_pre(stmt: &Statement) -> StmtPre {
    match stmt {
        Statement::Savepoint(name) => StmtPre::Savepoint(name.clone()),
        Statement::Release(name) => StmtPre::Release(name.clone()),
        Statement::Rollback(rb) => match &rb.savepoint {
            Some(name) => StmtPre::RollbackTo(name.clone()),
            None => StmtPre::Other,
        },
        _ => StmtPre::Other,
    }
}

impl Database {
    /// Open or create a database at the given path.
    ///
    /// A file written with an active page codec is refused here with a
    /// clear error — use [`Self::open_with_codec`] (or register the codec
    /// and run `PRAGMA codec = <name>` before any other statement).
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_inner(path, None)
    }

    /// Shared constructor: `codec` is activated before the schema load.
    fn open_inner<P: AsRef<Path>>(
        path: P,
        codec: Option<std::sync::Arc<dyn crate::plugin::PageCodec>>,
    ) -> Result<Self> {
        crate::engine_init();
        let path = path.as_ref().to_path_buf();
        let pager = Pager::open(&path, DEFAULT_CACHE_PAGES)?;
        if let Some(c) = &codec {
            pager.set_codec(Some(c.clone()))?;
        } else if let Some(required) = pager.required_codec() {
            return Err(Error::semantic(format!(
                "database is written with page codec '{}' — open it with Database::open_with_codec(path, codec)",
                required
            )));
        }
        let mut catalog = Catalog::new();
        catalog.schema_cookie = pager.schema_cookie();
        // Load the schema from page 0 (the schema table root).
        load_schema(&pager, &mut catalog)?;
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
            savepoint_maps: Mutex::new(Vec::new()),
            savepoint_txn: AtomicBool::new(false),
            maps: RwLock::new(empty_maps()),
            maps_populated: AtomicBool::new(false),
            write_epoch: AtomicU64::new(0),
            table_count_cache: RwLock::new(HashMap::new()),
            alloc_burst: AtomicU64::new(0),
            schema_root_pages: Mutex::new(schema_root_pages),
            stmt_cache: RwLock::new(StmtCacheMap::default()),
            last_stmt: RwLock::new(None),
            stmt_cache_order: Mutex::new(Vec::new()),
            stmt_cache_capacity: DEFAULT_STMT_CACHE_CAPACITY,
            seen_hashes: Mutex::new(std::collections::HashSet::default()),
            seen_hashes_cap: 4096,
            deferred_flush: AtomicBool::new(false),
            deferred_flush_threshold: 1000,
            last_rowid: AtomicI64::new(0),
            insert_chain: Mutex::new(None),
            plugins: RwLock::new(std::sync::Arc::new(crate::plugin::PluginRegistry::new())),
            has_plugins: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Open a database written with a page codec (see
    /// [`crate::plugin::PageCodec`]). Activates the codec BEFORE the
    /// schema is loaded, so every page read decodes correctly. The
    /// codec's name must match the file's marker.
    pub fn open_with_codec<P: AsRef<Path>, C: crate::plugin::PageCodec + 'static>(
        path: P,
        codec: C,
    ) -> Result<Self> {
        let codec = std::sync::Arc::new(codec);
        let codec2 = codec.clone();
        let db = Self::open_inner(path, Some(codec))?;
        // Write the marker (idempotent; also validated against any
        // existing marker inside set_codec).
        db.pager.set_codec(Some(codec2))?;
        Ok(db)
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
        let tmp = tempfile::NamedTempFile::new().map_err(Error::Io)?;
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
        // A hot INSERT chain owns the table's live root: flush it into the
        // bookkeeping maps (and rewrite the schema row if a split moved
        // the root) BEFORE the pager writes pages to disk.
        self.break_insert_chain();
        self.pager.flush()
    }

    /// Flush from a `&self` reference — used by concurrent readers when they
    /// need to see unflushed writes. Uses the pager's interior mutability.
    pub fn flush_shared(&self) -> Result<()> {
        self.break_insert_chain();
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
    // ------------------------------------------------------------------
    // INSERT CHAIN (see `InsertChain` docs for the design).
    // ------------------------------------------------------------------

    /// Try to execute `sql` as a chained single-row literal INSERT.
    ///
    /// Returns `Ok(true)` when the chain handled the statement. Returns
    /// `Ok(false)` when no chain is hot or the statement's shape doesn't
    /// match it (the caller falls back to the cold fast-insert path).
    /// On any error the chain is dropped (after flushing its state), so a
    /// failed chained statement can never leave stale bookkeeping behind.
    fn exec_chained_insert(&mut self, sql: &str) -> Result<bool> {
        let epoch = self.write_epoch.load(Ordering::Acquire);
        let mut guard = self.insert_chain.lock();
        let Some(ch) = guard.as_mut() else {
            return Ok(false);
        };
        let chain_epoch_ok = ch.epoch == epoch.wrapping_sub(1);
        if !chain_epoch_ok || matches!(parse_chain_row(sql, ch), ChainParse::Mismatch) {
            // Epoch gap (another statement ran since the chain's last use)
            // or shape mismatch: flush the chain's root / max-rowid into
            // the bookkeeping maps, then drop it. The cold path below (or
            // a later statement) rebuilds a chain for the new shape.
            let flushed = guard.take();
            drop(guard);
            if let Some(ch) = flushed {
                self.flush_insert_chain(&ch)?;
            }
            return Ok(false);
        }
        // Lean per-row execution: NOT NULL -> encode -> B+tree append.
        // `Ok(false)` = the rowid space is exhausted (bail to the general
        // path's collision-safe allocation).
        match self.exec_chain_row(ch) {
            Ok(true) => {
                ch.epoch = epoch;
                Ok(true)
            }
            Ok(false) => {
                let flushed = guard.take();
                drop(guard);
                if let Some(ch) = flushed {
                    self.flush_insert_chain(&ch)?;
                }
                Ok(false)
            }
            Err(e) => {
                // Constraint / IO error: nothing was mutated (constraint
                // checks run before the B+tree insert), but the statement
                // already bumped the epoch, so the chain can never validate
                // again — flush and drop it for a clean slate.
                let flushed = guard.take();
                drop(guard);
                if let Some(ch) = flushed {
                    let _ = self.flush_insert_chain(&ch);
                }
                Err(e)
            }
        }
    }

    /// Execute one row already scanned into `ch.full_row` against the
    /// table's B+tree. `Ok(false)` means "rowid space exhausted — caller
    /// must fall back to the general path".
    fn exec_chain_row(&self, ch: &mut InsertChain) -> Result<bool> {
        // Rowid allocation: auto-generated only (the scanner rejects
        // explicit rowid values), so `max_rowid + 1` is collision-free.
        if ch.max_rowid >= i64::MAX - 1 {
            return Ok(false);
        }
        let rowid = ch.max_rowid + 1;
        ch.max_rowid = rowid;
        if let Some(alias) = ch.rowid_alias {
            ch.full_row[alias] = Value::Integer(rowid);
        }
        // NOT NULL enforcement (the rowid alias was pre-assigned above, so
        // an alias declared NOT NULL — SQLite reports INTEGER PRIMARY KEY
        // as nullable, but a redundant NOT NULL still holds — is covered).
        for &c in &ch.not_null {
            if ch.full_row[c].is_null() {
                return Err(Error::constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    ch.table.name,
                    ch.table.columns[c].name
                )));
            }
        }
        // Payload encode into the reused buffer (elides the rowid-alias
        // column to a 1-byte marker — its value is the B+tree key).
        crate::storage::row_codec::encode_row_aliased_into(
            &ch.full_row,
            ch.rowid_alias,
            &mut ch.payload_buf,
        );
        // B+tree append with the cross-statement leaf hint.
        let mut bt = Btree::new(&self.pager, ch.root, false);
        let hint = ch.leaf_hint.take();
        let new_hint = bt.insert_table_append_hinted(rowid, &ch.payload_buf, hint)?;
        ch.leaf_hint = new_hint;
        if bt.root != ch.root {
            // Split: track the live root, rewrite the schema row NOW (an
            // auto-commit flush below must not write a file whose schema
            // row still points at the stale root), and update the tracker
            // so `sync_schema_roots` stays a no-op for this table.
            ch.root = bt.root;
            self.rewrite_schema_row_root("table", &ch.table.name, ch.root)?;
            let mut synced = self.schema_root_pages.lock();
            synced.insert(format!("table:{}", ch.name_lc), ch.root);
        }
        // Counters (mirrors the general path's epilogue).
        self.set_last_insert_rowid(rowid);
        crate::executor::change_counters::record(1);
        // Auto-commit flush, exactly like `exec_fast_insert`'s tail.
        let in_txn = self.in_transaction.load(Ordering::Acquire);
        let deferred = self.deferred_flush.load(Ordering::Acquire);
        if !in_txn && !deferred {
            self.pager.flush()?;
        } else if deferred && !in_txn {
            let dirty = self.pager.dirty_page_count();
            if dirty >= self.deferred_flush_threshold {
                let _ = self.pager.flush();
            }
        }
        Ok(true)
    }

    /// Push the chain's live root / max-rowid into the shared
    /// bookkeeping maps and rewrite the schema row if a split moved the
    /// root. Runs whenever a hot chain is broken — every general-path
    /// statement consults the maps, so they must be fresh first.
    fn flush_insert_chain(&self, ch: &InsertChain) -> Result<()> {
        let mut m = self.maps.write();
        let maps = Arc::make_mut(&mut *m);
        // Root: write under both key forms (as-declared and lowercased) —
        // `table_root`'s fast path probes the declared name first, so a
        // pre-existing declared-name entry must not shadow the fresh value.
        let name = ch.table.name.as_str();
        let lc: &str = &ch.name_lc;
        if maps.roots.get(name) != Some(&ch.root) {
            maps.roots.insert(name.to_string(), ch.root);
        }
        if lc != name && maps.roots.get(lc) != Some(&ch.root) {
            maps.roots.insert(lc.to_string(), ch.root);
        }
        // Max rowid: monotonic raise only (the cache must stay an upper
        // bound of the live rowids for collision-free allocation).
        let cur = maps.max_rowids.get(name).copied().unwrap_or(i64::MIN);
        if ch.max_rowid > cur {
            maps.max_rowids.insert(name.to_string(), ch.max_rowid);
        }
        let cur_lc = maps.max_rowids.get(lc).copied().unwrap_or(i64::MIN);
        if lc != name && ch.max_rowid > cur_lc {
            maps.max_rowids.insert(lc.to_string(), ch.max_rowid);
        }
        let nonempty = !maps.roots.is_empty() || !maps.index_roots.is_empty();
        drop(m);
        self.maps_populated.store(nonempty, Ordering::Release);
        // Schema-row sync for a root that moved while the chain was hot.
        // Mirrors `sync_schema_roots_inner`: rewrite the row, update the
        // tracker, and invalidate cached plans (they embed Arc<Table>
        // snapshots; a fresh plan re-reads the live root from the maps).
        let key = format!("table:{}", ch.name_lc);
        let synced = self.schema_root_pages.lock();
        if synced.get(&key).copied() != Some(ch.root) {
            drop(synced);
            self.rewrite_schema_row_root("table", &ch.table.name, ch.root)?;
            let mut synced = self.schema_root_pages.lock();
            synced.insert(key, ch.root);
            drop(synced);
            self.invalidate_stmt_cache();
        }
        Ok(())
    }

    /// Break (flush + drop) a hot INSERT chain. Every read path that can
    /// observe table state (`query`, prepared statements, `flush`) calls
    /// this first: while a chain is hot, the shared maps' root / max-rowid
    /// entries for the chained table are stale by design.
    pub(crate) fn break_insert_chain(&self) {
        let flushed = self.insert_chain.lock().take();
        if let Some(ch) = flushed {
            // Flush state. Read paths tolerate flush errors the same way
            // they tolerate deferred-flush failures: log nothing, keep
            // going — the next writer re-derives the maps from the catalog.
            let _ = self.flush_insert_chain(&ch);
        }
    }

    /// Build (and store) an INSERT chain for `table` when the table is
    /// plain enough for the lean chained path. Called after a successful
    /// cold-path fast insert; the chain serves SUBSEQUENT same-shape
    /// statements. `explicit_cols` is the statement's column list (empty
    /// = supplies-all shape).
    fn try_build_insert_chain(
        &self,
        explicit_cols: &[&str],
        table: &Arc<Table>,
        col_indices: &[usize],
        root: u32,
        max_rowid: i64,
        leaf_hint: Option<u32>,
    ) {
        // Shape gates that `exec_fast_insert` already verified: no vtab,
        // not WITHOUT ROWID, not STRICT, no generated columns, and either
        // supplies-all or a table with no DEFAULTs.
        // Additional gates: no CHECKs, no INSERT triggers, no outgoing
        // foreign keys, no indexes (the chain maintains only the table
        // B+tree; index maintenance needs the general path).
        if !table.check_exprs.is_empty()
            || !table.foreign_keys.is_empty()
            || self
                .catalog
                .triggers_on_table(&table.name)
                .iter()
                .any(|t| {
                    t.events
                        .iter()
                        .any(|ev| matches!(ev, TriggerEvent::Insert))
                })
            || !self.catalog.indexes_on_table(&table.name).is_empty()
            // Explicit `rowid` column (sentinel target) needs the general
            // path's conflict-checking executor.
            || col_indices.contains(&crate::executor::ROWID_COLUMN_SENTINEL)
            // Rowid space must have headroom for the fast `max+1`
            // allocation (the degenerate case falls to the general path's
            // collision-safe allocation).
            || max_rowid >= i64::MAX - 1
        {
            return;
        }
        let n_cols = table.n_columns();
        // Supplies-all: col_names stays empty (the shape marker) and
        // col_indices is the identity 0..n_cols. Explicit list: keep the
        // lowercased names for the scanner's case-insensitive comparison.
        let (col_names, col_idx): (Vec<Box<str>>, Vec<usize>) = if explicit_cols.is_empty() {
            (Vec::new(), (0..n_cols).collect())
        } else {
            (
                explicit_cols
                    .iter()
                    .map(|c| c.to_ascii_lowercase().into_boxed_str())
                    .collect(),
                col_indices.to_vec(),
            )
        };
        let affinities: Vec<crate::types::Affinity> = col_idx
            .iter()
            .map(|&i| table.columns[i].affinity)
            .collect();
        let not_null: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.nullable)
            .map(|(i, _)| i)
            .collect();
        let full_row: Vec<Value> = vec![Value::Null; n_cols];
        let chain = InsertChain {
            epoch: self.write_epoch.load(Ordering::Acquire),
            table: Arc::clone(table),
            name_lc: table.name.to_ascii_lowercase().into_boxed_str(),
            col_names,
            col_indices: col_idx,
            affinities,
            not_null,
            rowid_alias: table.rowid_alias,
            root,
            max_rowid,
            leaf_hint,
            full_row,
            payload_buf: Vec::with_capacity(n_cols * 8),
        };
        *self.insert_chain.lock() = Some(chain);
    }

    fn exec_fast_insert(&mut self, fi: FastInsert<'_>) -> Result<bool> {
        let table = match self.catalog.get_table_fast(fi.table) {
            Some(t) => t,
            None => {
                // Unknown table — same error the planner produces.
                return Err(Error::NotFound(format!("table: {}", fi.table)));
            }
        };
        // Table-shape bails: fall through to the general path.
        // Virtual tables route through xUpdate — the byte scanner's
        // btree fast path doesn't apply.
        if table.vtab.is_some()
            || table.without_rowid
            || table.strict
            || table.columns.iter().any(|c| c.generated.is_some())
        {
            return Ok(false);
        }
        // If any column has a DEFAULT and the row doesn't supply every
        // column, defaults must be evaluated — general path.
        let supplies_all = fi.columns.is_empty() || (fi.columns.len() == table.n_columns());
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
                match resolve_insert_column(&table, name) {
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
        // The original column-list shape (empty = supplies-all) decides the
        // chain's scanner gate — take it out before `fi.values` moves.
        let stmt_columns: Vec<&str> = fi.columns.clone();
        let result =
            crate::executor::fast_insert_literal_rows(&mut ctx, &table, &col_indices, fi.values)
                .map(|(inserted, root, max_rowid)| {
                    // Seed the cross-statement INSERT chain (eligibility
                    // is checked inside; plain tables only). The chain
                    // serves the NEXT same-shape statement; this one paid
                    // the cold-path setup.
                    let table_key = Arc::as_ptr(&table) as usize;
                    let leaf_hint = ctx
                        .table_append_hint
                        .filter(|(k, _)| *k == table_key)
                        .map(|(_, leaf)| leaf);
                    self.try_build_insert_chain(
                        &stmt_columns,
                        &table,
                        &col_indices,
                        root,
                        max_rowid,
                        leaf_hint,
                    );
                    inserted
                });

        // Epilogue: same write-backs as `execute` (merge into the
        // detached maps in place, then attach back).
        self.in_transaction
            .store(ctx.in_transaction, Ordering::Release);
        self.set_last_insert_rowid(ctx.last_insert_rowid);
        *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
        if let Ok(n) = result {
            crate::executor::change_counters::record(n);
            self.note_alloc_burst(n as u64, 0);
        }
        // Merge overlays back regardless of success — a failed statement
        // may still have split a B+tree (page writes are not undone), so
        // dropping the root override would lose data. ROLLBACK is the only
        // path that legitimately discards them.
        if ctx.roots_changed {
            Arc::make_mut(&mut ctx.shared)
                .roots
                .extend(ctx.root_overrides.drain());
        }
        if ctx.max_rowids_changed {
            let shared = Arc::make_mut(&mut ctx.shared);
            shared.max_rowids.extend(ctx.max_rowids.drain());
            for k in ctx.max_rowids_invalidated.drain(..) {
                shared.max_rowids.remove(&k);
            }
        }
        if ctx.index_roots_changed {
            Arc::make_mut(&mut ctx.shared)
                .index_roots
                .extend(ctx.index_roots.drain());
        }
        *self.maps.get_mut() = ctx.shared;
        self.refresh_maps_flag();
        if result.is_ok() && ctx.roots_changed {
            self.sync_schema_roots_inner()?;
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
        // Allocator-wake drain at auto-commit write-burst completion.
        self.maybe_drain_after_burst();
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

    pub(crate) fn get_or_cache_stmt(&self, sql: &str) -> Result<Arc<CachedStmt>> {
        // Planning must see the plugin registry (user aggregates change
        // how expressions are planned — see is_aggregate_call). One
        // read-lock + refcount bump + thread-local install.
        let _plugin_guard = self.plugin_scope();
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
            let fast_path = plan_arc
                .as_ref()
                .and_then(|p| Self::detect_fast_path(p))
                .map(Arc::new);
            return Ok(Arc::new(CachedStmt {
                stmt: Arc::new(stmt),
                plan: plan_arc,
                has_subqueries: has_subq,
                fast_path,
            }));
        }
        // Fast path 1: last-statement memo — consecutive calls with the
        // same SQL text (the dominant pattern) skip hashing + probing.
        {
            let memo = self.last_stmt.read();
            if let Some((last_sql, cached)) = memo.as_ref() {
                if last_sql == sql {
                    return Ok(Arc::clone(cached));
                }
            }
        }
        // Fast path 2: read lock — concurrent readers can hit the cache
        // simultaneously without serializing.
        {
            let cache = self.stmt_cache.read();
            if let Some(cached) = cache.get(sql) {
                // One Arc refcount bump — the entry holds `Arc<Statement>`,
                // `Option<Arc<Plan>>` and `Option<Arc<FastPath>>` internally,
                // so the old per-field clone (three refcount pairs) is gone.
                let out = Arc::clone(cached);
                drop(cache);
                let mut memo = self.last_stmt.write();
                *memo = Some((sql.to_string(), Arc::clone(&out)));
                return Ok(out);
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
            return Ok(Arc::new(CachedStmt {
                stmt: Arc::new(stmt),
                plan: None,
                has_subqueries: false,
                fast_path: None,
            }));
        }
        let t1 = profile::now();
        let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
        profile::span(t1, &profile::PLAN_NS);
        let plan_arc = plan_opt.map(Arc::new);
        let has_subq = plan_arc
            .as_ref()
            .map(|p| crate::executor::plan_has_subqueries(p))
            .unwrap_or(false);
        let fast_path = plan_arc
            .as_ref()
            .and_then(|p| Self::detect_fast_path(p))
            .map(Arc::new);
        let entry = Arc::new(CachedStmt {
            stmt: Arc::new(stmt),
            plan: plan_arc,
            has_subqueries: has_subq,
            fast_path,
        });
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
                cache.insert(sql.to_string(), Arc::clone(&entry));
                self.stmt_cache_order.lock().push(sql.to_string());
            }
        }
        profile::span(t2, &profile::CACHE_NS);
        let mut memo = self.last_stmt.write();
        *memo = Some((sql.to_string(), Arc::clone(&entry)));
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
    pub(crate) fn sync_schema_roots_public(&self) -> Result<()> {
        self.sync_schema_roots_inner()
    }

    /// Refresh `maps_populated` from the live maps. Called at every attach
    /// site (all on `&mut self` / write-lock paths — never on a reader).
    /// A relaxed store is enough: readers can only run before or after a
    /// writer at the type level (`query` is `&self`, writers `&mut self`).
    fn refresh_maps_flag(&self) {
        let nonempty = {
            let m = self.maps.read();
            !m.roots.is_empty() || !m.index_roots.is_empty()
        };
        self.maps_populated.store(nonempty, Ordering::Release);
    }

    /// Account allocator blocks freed by a write statement: per-statement
    /// baseline (AST, tokens, plan teardown), per-row encode buffers, and
    /// newly-dirtied pages (index backfills, page splits). Feeds the
    /// `drain_mimalloc_wake` threshold (checked at transaction end — see
    /// `maybe_drain_after_burst`).
    fn note_alloc_burst(&self, changes: u64, dirty_delta: u64) {
        // `dirty_delta` counts mutating write ops (the pager's upper-bound
        // counter) — each spills encode buffers + cell bytes, so weight it
        // as ~150 blocks.
        let blocks = 48 + changes * 6 + dirty_delta * 150;
        self.alloc_burst.fetch_add(blocks, Ordering::Relaxed);
    }

    /// Drain mimalloc's delayed-free wake when a write burst COMPLETES —
    /// i.e. a statement ends with no transaction open (auto-commit write,
    /// COMMIT, ROLLBACK, DDL). Draining mid-transaction is useless: the
    /// remaining statements re-arm the queue (measured in
    /// examples/probe_mid_drain.rs — mid-storm drain leaves a 212 µs wake
    /// on the next read; a post-storm drain leaves 19.5 µs). Once per
    /// process: later bursts never re-wake (examples/probe_rounds.rs).
    fn maybe_drain_after_burst(&self) {
        if !ALLOC_SETTLED.load(Ordering::Relaxed)
            && !self.in_transaction.load(Ordering::Acquire)
            && self.alloc_burst.load(Ordering::Relaxed) > ALLOC_WAKE_THRESHOLD
            && !ALLOC_SETTLED.swap(true, Ordering::Relaxed)
        {
            drain_mimalloc_wake();
            self.alloc_burst.store(0, Ordering::Relaxed);
        }
    }

    /// Read-side safety net for the allocator wake (see
    /// `drain_mimalloc_wake`): covers write bursts that completed without
    /// passing the transaction-end check (e.g. DML via the query/RETURNING
    /// path). Free once the process-level flag is settled.
    #[inline]
    fn maybe_settle_allocator(&self) {
        if !ALLOC_SETTLED.load(Ordering::Relaxed)
            && self.alloc_burst.load(Ordering::Relaxed) > ALLOC_WAKE_THRESHOLD
            && !ALLOC_SETTLED.swap(true, Ordering::Relaxed)
        {
            drain_mimalloc_wake();
            self.alloc_burst.store(0, Ordering::Relaxed);
        }
    }

    fn sync_schema_roots_inner(&self) -> Result<()> {
        // (object name, root page) pairs for tables and indexes.
        let (tables, indexes): (Vec<RootEntry>, Vec<RootEntry>) = {
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
        *self.last_stmt.write() = None;
    }

    /// Execute a statement that does not return rows (INSERT/UPDATE/DELETE/CREATE/...).
    ///
    /// Takes `&mut self` because the outer `RwLock<Database>` write lock
    /// must be held to ensure single-writer semantics — but the actual state
    /// mutations go through interior mutability so the body could in principle
    /// be `&self`. We keep `&mut self` for API clarity (writers serialize).
    pub fn execute<P: Params>(&mut self, sql: &str, params: P) -> Result<()> {
        profile::COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Write-epoch bump: `execute` is the gateway for every mutating
        // statement (DML, DDL, transaction control, fast-path inserts),
        // so ONE bump here invalidates the memoized COUNT(*) answers for
        // all tables. Bumping unconditionally (SELECTs through execute
        // are rare CTE shapes) trades a pointless invalidation for the
        // guarantee that no write path can forget it. Readers are `&self`
        // and can never observe a torn epoch.
        self.write_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        // ---- FAST INSERT PATH ----
        // Single-row literal VALUES inserts are the hottest statement shape
        // in OLTP. Two tiers:
        //   1. INSERT CHAIN — consecutive same-shape inserts keep every
        //      derived fact (table, columns, root, max-rowid, leaf hint)
        //      alive across statements: zero per-statement setup.
        //   2. Byte scanner — first sight of a shape: recognizes the
        //      statement without the tokenizer/parser/planner pipeline.
        // The scanners are conservative: any deviation (UPSERT, RETURNING,
        // non-literals, multi-row) falls through to the general path.
        {
            let first = sql
                .as_bytes()
                .iter()
                .find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
            if first == Some(&b'I') || first == Some(&b'i') {
                if self.exec_chained_insert(sql)? {
                    return Ok(());
                }
                if let Some(fi) = try_fast_insert_parse(sql) {
                    if self.exec_fast_insert(fi)? {
                        return Ok(());
                    }
                }
            } else {
                // Any non-INSERT statement ends a hot chain: its root /
                // max-rowid must reach the bookkeeping maps before the
                // general path (or a later reader) consults them.
                self.break_insert_chain();
            }
        }
        let is_ddl = is_ddl_sql(sql);
        let dirty_before = self.pager.dirty_page_count() as u64;
        let t_cache = profile::now();
        let cached = self.get_or_cache_stmt(sql)?;
        profile::span(t_cache, &profile::CACHE_NS);
        // WITH-clause SELECT via the execute path: same CTE machinery as
        // query(); the result rows are simply discarded.
        if let Statement::Select(sel) = cached.stmt.as_ref() {
            if sel.with.is_some() {
                // vtab xConnect bridge (same rationale as the DDL path).
                let _thread_db = crate::plugin::abi::ThreadDbGuard::install(
                    self as *const Database as *mut Database,
                );
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
                let _plugin_guard = self.plugin_scope();
                let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
                let res = self.exec_select_with_ctes(&mut ctx, sel, &HashMap::new());
                self.in_transaction
                    .store(ctx.in_transaction, Ordering::Release);
                self.set_last_insert_rowid(ctx.last_insert_rowid);
                *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
                if ctx.max_rowids_changed {
                    Arc::make_mut(&mut ctx.shared)
                        .max_rowids
                        .extend(ctx.max_rowids.drain());
                }
                *self.maps.get_mut() = ctx.shared;
                self.refresh_maps_flag();
                res?;
                return Ok(());
            }
        }
        // SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT: need &self for the
        // bookkeeping-map snapshots, so they bypass the static dispatcher.
        match stmt_ref_pre(&cached.stmt) {
            StmtPre::Savepoint(name) => {
                self.exec_savepoint(&name)?;
                return Ok(());
            }
            StmtPre::Release(name) => {
                self.exec_release_savepoint(&name)?;
                return Ok(());
            }
            StmtPre::RollbackTo(name) => {
                self.exec_rollback_to_savepoint(&name)?;
                return Ok(());
            }
            StmtPre::Other => {}
        }
        // Deref the Arc<Statement> to a &Statement for execute_statement_static.
        // (The Arc itself stays alive on the stack for the duration of the call.)
        let stmt_ref: &Statement = &cached.stmt;
        let plan_opt = cached.plan.clone();
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
        // Correlated-subquery execution bridge: installs the statement's
        // ExecContext for evaluate-time subquery re-execution (DML SET /
        // CHECK / WHERE expressions). Cleared by Drop before ctx ends.
        let _plugin_guard = self.plugin_scope();
        let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        // Use the cached plan if available — execute_statement_static
        // otherwise re-parses + re-plans, which for a 1k-row INSERT batch
        // means 1k wasted planning passes (each builds a Plan::Insert
        // containing Plan::Values { Vec<Vec<Expr>> }, several heap
        // allocations). With the cached plan, we skip all of that.
        let t_exec = profile::now();
        // C-ABI virtual-table bridge: xCreate/xConnect (reached through
        // CREATE VIRTUAL TABLE and module registration) route declare_vtab
        // and argv construction through this thread-local raw pointer.
        // Valid for the duration of the statement (we hold &mut self).
        let _thread_db =
            crate::plugin::abi::ThreadDbGuard::install(self as *const Database as *mut Database);
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
        self.in_transaction
            .store(ctx.in_transaction, Ordering::Release);
        self.set_last_insert_rowid(ctx.last_insert_rowid);
        *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
        crate::executor::change_counters::record(ctx.changes);
        // Allocator-burst accounting (see `settle_allocator`): estimate the
        // blocks this statement freed — AST/plan teardown + per-row encode
        // buffers + newly-dirtied pages (index backfill, splits). The dirty
        // count is a DELTA (pages dirtied by THIS statement), not the
        // cumulative count — lazy write-back never resets the total.
        if result.is_ok() && (ctx.changes > 0 || is_ddl) {
            let dirty_now = self.pager.dirty_page_count() as u64;
            let dirty_delta = dirty_now.saturating_sub(dirty_before);
            self.note_alloc_burst(ctx.changes.unsigned_abs(), dirty_delta);
        }
        // Merge local overlay entries into the DETACHED maps (in place —
        // the statement is the sole owner, so make_mut never clones) and
        // attach them back to the Database. Merge regardless of `result`:
        // a failed statement may still have split a B+tree (page writes
        // are not undone by error propagation); ROLLBACK is the only path
        // that legitimately discards them.
        if ctx.roots_changed {
            Arc::make_mut(&mut ctx.shared)
                .roots
                .extend(ctx.root_overrides.drain());
        }
        if ctx.max_rowids_changed {
            let shared = Arc::make_mut(&mut ctx.shared);
            shared.max_rowids.extend(ctx.max_rowids.drain());
            for k in ctx.max_rowids_invalidated.drain(..) {
                shared.max_rowids.remove(&k);
            }
        }
        if ctx.index_roots_changed {
            Arc::make_mut(&mut ctx.shared)
                .index_roots
                .extend(ctx.index_roots.drain());
        }
        *self.maps.get_mut() = ctx.shared;
        self.refresh_maps_flag();
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
            self.refresh_maps_flag();
        }
        // Persist any root-page moves (B+tree splits) to the schema rows so
        // a reopened database sees the full tree. Without this, every table
        // or index that split lost all data beyond the stale root on reopen.
        // Gated on ctx.roots_changed — roots only move on splits, which are
        // rare; previously this ran (two read locks + two Vec<(String,u32)>
        // collects with String clones) after EVERY statement.
        if result.is_ok() && ctx.roots_changed {
            self.sync_schema_roots_inner()?;
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
        // Allocator-wake drain at write-burst completion (COMMIT / auto-
        // commit / DDL): see `maybe_drain_after_burst`.
        self.maybe_drain_after_burst();
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
        // A hot INSERT chain owns the table's live root / max-rowid while
        // the shared maps hold stale values — break (flush) it before this
        // read consults the maps.
        self.break_insert_chain();
        // Absorb the allocator's post-write-storm wake (see
        // `settle_allocator`) BEFORE the parse/plan allocations would pay
        // it — a bulk-write transaction followed by reads is the classic
        // bench (and production) shape.
        self.maybe_settle_allocator();
        // In deferred_flush mode, a SELECT must see all writes that
        // happened since the last flush.
        if self.deferred_flush.load(Ordering::Acquire) && self.pager.has_dirty_pages() {
            let _ = self.pager.flush();
        }
        let cached = self.get_or_cache_stmt(sql)?;
        // Read-form PRAGMAs surface their value as result rows (one for
        // single-value pragmas, N for table-valued ones like table_info).
        if let Statement::Pragma(p) = cached.stmt.as_ref() {
            if let Some(pr) = read_pragma(p, self) {
                return Ok(pr.rows);
            }
        }
        // EXPLAIN [QUERY PLAN]: plan the inner statement and render rows;
        // never execute it.
        if let Statement::Explain(inner) = cached.stmt.as_ref() {
            let plan = Self::plan_for_statement(&self.catalog, inner)?;
            return Ok(match plan {
                Some(p) => crate::executor::explain::explain_plan_rows(&p),
                None => Vec::new(),
            });
        }
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
                let _plugin_guard = self.plugin_scope();
                let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
                let res = self.exec_select_with_ctes(&mut ctx, sel, &HashMap::new())?;
                return Ok(res.rows);
            }
        }
        // Pre-compiled point-lookup fast path: skips the ExecContext /
        // EvalContext / Plan dispatch entirely. Only fires for the
        // exact shapes detected at cache time (bare-column projections
        // over a rowid / index point lookup). Checked BEFORE the plan
        // Arc clone — the fast path never touches the plan, and the
        // refcount pair cost ~15 ns per OLTP query.
        if let Some(fp) = &cached.fast_path {
            // Bind straight from the caller's parameter storage when
            // it's already contiguous (arrays/Vec) — no per-query Vec.
            if let Some(slice) = params.as_slice() {
                let rows = self.run_fast_path(fp, slice)?;
                return Ok(rows);
            }
            let params_v: Vec<Value> = params.into_iter().collect();
            let rows = self.run_fast_path(fp, &params_v)?;
            return Ok(rows);
        }
        if let Some(plan) = cached.plan.clone() {
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
            // Correlated-subquery bridge (see execute): must span the
            // subquery rewrite AND execution — rewrite-time execution of
            // UNcorrelated subqueries may itself hit nested correlated ones.
            let _plugin_guard = self.plugin_scope();
            let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
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
                // DML via the query path bypasses `Database::execute` (and
                // its unconditional write-epoch bump) — bump it HERE so
                // memoized COUNT(*) answers can never outlive the write
                // they describe (see `FastPath::CountStar`).
                self.write_epoch
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
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
                        for k in ctx.max_rowids_invalidated.drain(..) {
                            bk.max_rowids.remove(&k);
                        }
                    }
                    if ctx.index_roots_changed {
                        bk.index_roots.extend(ctx.index_roots.drain());
                    }
                    let nonempty = !bk.roots.is_empty() || !bk.index_roots.is_empty();
                    self.maps_populated.store(nonempty, Ordering::Release);
                }
                self.sync_schema_roots_inner()?;
            } else if ctx.max_rowids_changed {
                // Pure SELECTs can still populate the max-rowid scan cache
                // (used by the index-range merge-scan heuristic) — merge it
                // back so repeated queries don't rescan.
                let mut m = self.maps.write();
                let bk = Arc::make_mut(&mut *m);
                bk.max_rowids.extend(ctx.max_rowids.drain());
                for k in ctx.max_rowids_invalidated.drain(..) {
                    bk.max_rowids.remove(&k);
                }
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

    /// Prepare a statement for repeated execution — the
    /// `sqlite3_prepare_v2` + `sqlite3_step` model.
    ///
    /// The statement is parsed and planned ONCE; parameters are bound with
    /// [`Statement::bind`] (1-based, like SQLite) and rows arrive one at a
    /// time via [`Statement::step`]. See [`Statement`] for the streaming
    /// plan shapes.
    ///
    /// Transaction-control and DDL statements are rejected here — use
    /// [`Database::execute`] for those.
    pub fn prepare(&self, sql: &str) -> Result<crate::statement::Statement<'_>> {
        crate::statement::Statement::new(self, sql)
    }

    /// Running total of rows modified by all statements on this thread
    /// (SQLite's `sqlite3_total_changes`).
    pub fn total_changes(&self) -> i64 {
        crate::executor::change_counters::total()
    }

    /// Rows modified by the most recent statement (SQLite's
    /// `sqlite3_changes`).
    pub fn changes(&self) -> i64 {
        crate::executor::change_counters::last()
    }

    /// Execute a query and return (column_names, rows).
    ///
    /// Takes `&self` — concurrent readers can call this simultaneously.
    pub fn query_with_columns<P: Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<(Vec<String>, Vec<Row>)> {
        self.break_insert_chain();
        self.maybe_settle_allocator();
        if self.deferred_flush.load(Ordering::Acquire) && self.pager.has_dirty_pages() {
            let _ = self.pager.flush();
        }
        let cached = self.get_or_cache_stmt(sql)?;
        if let Statement::Pragma(p) = cached.stmt.as_ref() {
            if let Some(pr) = read_pragma(p, self) {
                return Ok((pr.columns, pr.rows));
            }
        }
        if let Statement::Explain(inner) = cached.stmt.as_ref() {
            let plan = Self::plan_for_statement(&self.catalog, inner)?;
            let rows = match plan {
                Some(p) => crate::executor::explain::explain_plan_rows(&p),
                None => Vec::new(),
            };
            return Ok((
                vec![
                    "id".into(),
                    "parent".into(),
                    "notused".into(),
                    "detail".into(),
                ],
                rows,
            ));
        }
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
                let _plugin_guard = self.plugin_scope();
                let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
                let res = self.exec_select_with_ctes(&mut ctx, sel, &HashMap::new())?;
                return Ok((res.columns.to_vec(), res.rows));
            }
        }
        if let Some(plan) = cached.plan.clone() {
            // Pre-compiled point-lookup fast path (see query()).
            if let Some(fp) = &cached.fast_path {
                let owned;
                let slice: &[Value] = match params.as_slice() {
                    Some(s) => s,
                    None => {
                        owned = params.into_iter().collect::<Vec<Value>>();
                        &owned
                    }
                };
                let rows = self.run_fast_path(fp, slice)?;
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
            // Correlated-subquery bridge (see execute): must span the
            // subquery rewrite AND execution — rewrite-time execution of
            // UNcorrelated subqueries may itself hit nested correlated ones.
            let _plugin_guard = self.plugin_scope();
            let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
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
                        for k in ctx.max_rowids_invalidated.drain(..) {
                            bk.max_rowids.remove(&k);
                        }
                    }
                    if ctx.index_roots_changed {
                        bk.index_roots.extend(ctx.index_roots.drain());
                    }
                    let nonempty = !bk.roots.is_empty() || !bk.index_roots.is_empty();
                    self.maps_populated.store(nonempty, Ordering::Release);
                }
                self.sync_schema_roots_inner()?;
            } else if ctx.max_rowids_changed {
                // Pure SELECTs can still populate the max-rowid scan cache
                // (used by the index-range merge-scan heuristic) — merge it
                // back so repeated queries don't rescan.
                let mut m = self.maps.write();
                let bk = Arc::make_mut(&mut *m);
                bk.max_rowids.extend(ctx.max_rowids.drain());
                for k in ctx.max_rowids_invalidated.drain(..) {
                    bk.max_rowids.remove(&k);
                }
            }
            Ok((res.columns.to_vec(), res.rows))
        } else {
            Ok((Vec::new(), Vec::new()))
        }
    }

    /// Get the last inserted rowid (`sqlite3_last_insert_rowid`).
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_rowid.load(Ordering::Acquire)
    }

    /// Set the last inserted rowid (internal: called by the statement
    /// layer's ExecContext write-back).
    pub(crate) fn set_last_insert_rowid(&self, rowid: i64) {
        if rowid != 0 {
            self.last_rowid.store(rowid, Ordering::Release);
        }
    }

    /// `sqlite3_table_column_metadata`: (declared type, NOT NULL, PRIMARY
    /// KEY) for a table column, or None when table/column is unknown.
    /// Used by the C ABI layer (sqlx describe / column_nullable).
    pub fn table_column_metadata(&self, table: &str, column: &str) -> Option<(String, bool, bool)> {
        let t = self.catalog.get_table(table)?;
        let idx = t.find_column(column)?;
        let col = &t.columns[idx];
        let pk = col.primary_key;
        let not_null = !col.nullable || pk;
        Some((col.declared_type.clone(), not_null, pk))
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

    /// Read-only pager access (diagnostics: cache stats, page count).
    pub fn pager(&self) -> &Pager {
        &self.pager
    }

    // ====================================================================
    // Plugin / extension registration (static, in-process)
    // ====================================================================

    /// Register a user-defined scalar SQL function
    /// (SQLite's `sqlite3_create_function` with xFunc only).
    ///
    /// ```no_run
    /// # use rustqlite::{Database, Value, Result, plugin::{ScalarFunction, FnCtx}};
    /// struct AddSuffix;
    /// impl ScalarFunction for AddSuffix {
    ///     fn name(&self) -> &str { "add_suffix" }
    ///     fn call(&self, _ctx: &FnCtx, args: &[Value]) -> Result<Value> {
    ///         Ok(Value::Text(format!("{}!", args[0].as_text()).into()))
    ///     }
    /// }
    /// # fn main() -> Result<()> {
    /// let mut db = Database::open_in_memory()?;
    /// db.create_function(AddSuffix)?;
    /// let rows = db.query("SELECT add_suffix('hi')", [])?;
    /// assert_eq!(rows[0][0].as_text(), "hi!");
    /// # Ok(()) }
    /// ```
    pub fn create_function<F: crate::plugin::ScalarFunction + 'static>(
        &mut self,
        f: F,
    ) -> Result<()> {
        self.create_function_arc(std::sync::Arc::new(f))
    }

    /// `create_function` for an already-`Arc`ed function (used by the C ABI
    /// trampolines, which hold raw pointers inside the Arc).
    pub fn create_function_arc(
        &mut self,
        f: std::sync::Arc<dyn crate::plugin::ScalarFunction>,
    ) -> Result<()> {
        let mut reg = self.plugins.read().clone();
        if crate::plugin::lookup_scalar_is_builtin(f.name()) {
            return Err(Error::semantic(format!(
                "function {}() is a built-in and cannot be overridden",
                f.name()
            )));
        }
        Arc::make_mut(&mut reg).set_scalar(f);
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);
        Ok(())
    }

    /// Register a user-defined aggregate SQL function
    /// (SQLite's `sqlite3_create_function` with xStep + xFinal).
    pub fn create_aggregate<F: crate::plugin::AggregateFunction + 'static>(
        &mut self,
        f: F,
    ) -> Result<()> {
        let mut reg = self.plugins.read().clone();
        Arc::make_mut(&mut reg).set_aggregate(std::sync::Arc::new(f));
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);
        Ok(())
    }

    /// `create_aggregate` for an already-`Arc`ed aggregate.
    pub fn create_aggregate_arc(
        &mut self,
        f: std::sync::Arc<dyn crate::plugin::AggregateFunction>,
    ) -> Result<()> {
        let mut reg = self.plugins.read().clone();
        Arc::make_mut(&mut reg).set_aggregate(f);
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);
        Ok(())
    }

    /// Register a user-defined collation sequence
    /// (SQLite's `sqlite3_create_collation`). Built-in names NOCASE, RTRIM
    /// and BINARY can be replaced.
    pub fn create_collation<C: crate::plugin::Collation + 'static>(&mut self, c: C) -> Result<()> {
        let mut reg = self.plugins.read().clone();
        Arc::make_mut(&mut reg).set_collation(std::sync::Arc::new(c));
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);
        Ok(())
    }

    /// `create_collation` for an already-`Arc`ed collation.
    pub fn create_collation_arc(
        &mut self,
        c: std::sync::Arc<dyn crate::plugin::Collation>,
    ) -> Result<()> {
        let mut reg = self.plugins.read().clone();
        Arc::make_mut(&mut reg).set_collation(c);
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);
        Ok(())
    }

    /// Register a virtual-table module
    /// (SQLite's `sqlite3_create_module`).
    pub fn create_module<M: crate::plugin::VirtualTableModule + 'static>(
        &mut self,
        m: M,
    ) -> Result<()> {
        self.create_module_arc(std::sync::Arc::new(m))
    }

    /// `create_module` for an already-`Arc`ed module.
    ///
    /// Registering a module also CONNECTS every pending virtual table that
    /// was created by an earlier session with the same module name
    /// (SQLite's xConnect-on-open, deferred to registration because
    /// modules can't exist before the user registers them).
    pub fn create_module_arc(
        &mut self,
        m: std::sync::Arc<dyn crate::plugin::VirtualTableModule>,
    ) -> Result<()> {
        let module_name = m.name().to_ascii_lowercase();
        let mut reg = self.plugins.read().clone();
        Arc::make_mut(&mut reg).set_module(m.clone());
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);

        // Connect pending vtabs for this module: rebuild the catalog
        // Table with the module's declared columns, replace the entry,
        // and invalidate the statement cache (plans hold Arc<Table>).
        let pending: Vec<(String, std::sync::Arc<crate::plugin::vtab::VtabInstance>)> = self
            .catalog
            .all_tables()
            .into_iter()
            .filter(|(_, t)| {
                t.vtab
                    .as_ref()
                    .map(|v| v.is_pending() && v.module_name == module_name)
                    .unwrap_or(false)
            })
            .map(|(name, t)| (name, t.vtab.clone().unwrap()))
            .collect();
        let mut any_connected = false;
        for (name, inst) in pending {
            let instance = m.connect(&inst.table_name, &inst.args)?;
            let cols = instance.columns();
            let mut table = crate::plugin::vtab::vtab_columns_to_schema(&inst.table_name, &cols);
            table.create_sql = self
                .catalog
                .get_table(&name)
                .map(|t| t.create_sql.clone())
                .unwrap_or_default();
            // Reuse the SAME VtabInstance (state now connected).
            inst.set_connected(instance)?;
            table.vtab = Some(inst);
            self.catalog.add_table(table);
            any_connected = true;
        }
        if any_connected {
            self.invalidate_stmt_cache();
        }
        Ok(())
    }

    /// Register a page codec, activatable with `PRAGMA codec = <name>`.
    pub fn create_codec<C: crate::plugin::PageCodec + 'static>(&mut self, c: C) -> Result<()> {
        let mut reg = self.plugins.read().clone();
        Arc::make_mut(&mut reg).set_codec(std::sync::Arc::new(c));
        *self.plugins.write() = reg;
        self.has_plugins.store(true, Ordering::Release);
        Ok(())
    }

    /// Activate a registered page codec by name (equivalent to
    /// `PRAGMA codec = name`). Pass "plain"/"none" to disable.
    pub fn set_page_codec(&mut self, name: &str) -> Result<()> {
        let lowered = crate::plugin::codec::validate_codec_name(name)?;
        if lowered == "plain" || lowered == "none" {
            let _ = self.pager.set_codec(None);
            return Ok(());
        }
        let reg = self.plugins.read().clone();
        let codec = reg
            .codec(&lowered)
            .ok_or_else(|| Error::semantic(format!("no such codec: {name}")))?;
        // Re-set the marker in the header comment area.
        self.pager.set_codec(Some(codec))?;
        Ok(())
    }

    /// The name of the active page codec, if any.
    pub fn page_codec(&self) -> Option<String> {
        self.pager.codec_name()
    }

    /// Load a dynamic extension (`.so` / `.dylib` / `.dll`) built against
    /// `include/rustqlite_ext.h` (any language: C, C++, Zig, Rust).
    /// Mirrors SQLite's `sqlite3_load_extension`. The library must export
    ///
    /// ```c
    /// int rustqlite_extension_init(const rql_api *api, rql_db *db, char **err);
    /// ```
    ///
    /// `entry` overrides the entry-point name (default
    /// `rustqlite_extension_init`).
    #[cfg(feature = "extension")]
    pub fn load_extension<P: AsRef<Path>>(&mut self, path: P, entry: Option<&str>) -> Result<()> {
        crate::plugin::abi::load_extension(self, path.as_ref(), entry)
    }

    /// Snapshot of the plugin registry (introspection: registered function
    /// names, collations, modules, codecs).
    pub fn plugin_registry(&self) -> std::sync::Arc<crate::plugin::PluginRegistry> {
        self.plugins.read().clone()
    }

    /// The plugin-scope guard when anything is registered; None otherwise.
    /// Keeps the zero-plugin hot path at one relaxed atomic load.
    #[inline]
    pub(crate) fn plugin_scope(&self) -> Option<crate::plugin::PluginScopeGuard> {
        if !self.has_plugins.load(Ordering::Relaxed) {
            return None;
        }
        Some(crate::plugin::PluginScopeGuard::install(
            self.plugins.read().clone(),
        ))
    }

    // ====================================================================
    // CTE (WITH clause) materialization
    // ====================================================================

    /// Execute a SELECT statement with a set of pre-materialized CTEs in
    /// scope. Returns the ExecResult (columns + rows).
    /// SAVEPOINT <name> — start (or nest) a savepoint. Outside a
    /// transaction, the savepoint OPENS one (releasing the outermost
    /// savepoint then commits, per SQLite semantics).
    fn exec_savepoint(&mut self, name: &str) -> Result<()> {
        if !self.in_transaction.load(Ordering::Acquire) {
            // Implicit BEGIN (mirrors Statement::Begin handling): flush the
            // current dirty set so a plain ROLLBACK (file-based restore)
            // sees pre-savepoint state, then snapshot.
            self.pager.flush_before_snapshot()?;
            self.in_transaction.store(true, Ordering::Release);
            *self.txn_snapshot.lock() = Some(self.pager.snapshot());
            self.savepoint_txn.store(true, Ordering::Release);
        }
        self.pager.savepoint(name);
        self.savepoint_maps.lock().push(self.maps.read().clone());
        Ok(())
    }

    /// ROLLBACK TO SAVEPOINT <name> — restore pager + bookkeeping maps to
    /// the savepoint; the transaction stays open.
    fn exec_rollback_to_savepoint(&mut self, name: &str) -> Result<()> {
        match self.pager.rollback_savepoint(name)? {
            Some(depth) => {
                let mut maps_snaps = self.savepoint_maps.lock();
                maps_snaps.truncate(depth);
                if let Some(restored) = maps_snaps.last().cloned() {
                    *self.maps.get_mut() = restored;
                    self.refresh_maps_flag();
                }
                Ok(())
            }
            None => Err(Error::semantic(format!("no such savepoint: {name}"))),
        }
    }

    /// RELEASE [SAVEPOINT] <name> — discard the savepoint and everything
    /// above it without rolling back. Releasing the outermost savepoint of
    /// a savepoint-started transaction COMMITS.
    fn exec_release_savepoint(&mut self, name: &str) -> Result<()> {
        match self.pager.release_savepoint(name) {
            Some(remaining) => {
                self.savepoint_maps.lock().truncate(remaining);
                if remaining == 0 && self.savepoint_txn.load(Ordering::Acquire) {
                    // Outermost savepoint released -> COMMIT.
                    self.pager.clear_savepoints();
                    self.savepoint_maps.lock().clear();
                    self.savepoint_txn.store(false, Ordering::Release);
                    self.in_transaction.store(false, Ordering::Release);
                    *self.txn_snapshot.lock() = None;
                    self.pager.flush()?;
                }
                Ok(())
            }
            None => Err(Error::semantic(format!("no such savepoint: {name}"))),
        }
    }

    /// CTE SELECT execution bridge for the statement layer (pub(crate)).
    pub(crate) fn exec_select_with_ctes_stmt(
        &self,
        ctx: &mut ExecContext<'_>,
        select: &SelectStatement,
    ) -> Result<crate::executor::ExecResult> {
        self.exec_select_with_ctes(ctx, select, &HashMap::new())
    }

    fn exec_select_with_ctes(
        &self,
        ctx: &mut ExecContext<'_>,
        select: &SelectStatement,
        outer_ctes: &HashMap<String, crate::types::CteMaterialization>,
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
        outer_ctes: &HashMap<String, crate::types::CteMaterialization>,
        ctx: &mut ExecContext<'_>,
    ) -> Result<HashMap<String, crate::types::CteMaterialization>> {
        let mut map: HashMap<String, crate::types::CteMaterialization> = outer_ctes.clone();
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
        outer_ctes: &HashMap<String, crate::types::CteMaterialization>,
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
            (
                Arc::new(Vec::new()),
                Arc::from(vec![format!("{}.", cte.name)]),
            ),
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
        let mut seen: std::collections::HashSet<String> =
            rows.iter().map(|r| format!("{:?}", r)).collect();
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

    pub(crate) fn plan_for_statement(
        catalog: &Catalog,
        stmt: &Statement,
    ) -> Result<Option<crate::planner::plan::Plan>> {
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
        ) -> Option<crate::types::ProjectionMapping> {
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
                        names.push(
                            pe.alias
                                .clone()
                                .unwrap_or_else(|| table.columns[idx].name.clone()),
                        );
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

        // `SELECT COUNT(*) FROM t WHERE indexed_col = ?` — the
        // covering-index count: index probe only, no table fetch, no
        // pipeline (the general path measured ~1085 ns for this shape).
        // The planner wraps the Aggregate in a Project (handled inside the
        // match below); a BARE Aggregate reaching the top of the plan is
        // rare but valid — check it here, before dispatch.
        fn covering_count_fp(
            input: &crate::planner::plan::Plan,
            group_by: &[Expr],
            aggregates: &[crate::planner::plan::AggExpr],
        ) -> Option<FastPath> {
            if !group_by.is_empty() || aggregates.len() != 1 {
                return None;
            }
            let agg = &aggregates[0];
            if !agg.func.eq_ignore_ascii_case("count") || agg.arg.is_some() || agg.distinct {
                return None;
            }
            match input {
                crate::planner::plan::Plan::IndexLookup {
                    table,
                    index,
                    key_exprs,
                    ..
                } => {
                    let keys = key_exprs
                        .iter()
                        .map(bind_expr)
                        .collect::<Option<Vec<_>>>()?;
                    let pre_encoded = pre_encode_literal_keys(&keys);
                    Some(FastPath::IndexCount {
                        table: table.clone(),
                        index: index.clone(),
                        keys,
                        pre_encoded,
                        columns: std::sync::Arc::from(vec![agg.display_name.clone()]),
                    })
                }
                _ => None,
            }
        }
        /// `SELECT COUNT(*) FROM t` — the BARE table-count shape (no WHERE,
        /// no GROUP BY): direct cell counting, zero decode, no executor.
        /// Shared by the Project-wrapped and bare-Aggregate plan shapes.
        fn bare_count_star_fp(
            input: &crate::planner::plan::Plan,
            group_by: &[Expr],
            aggregates: &[crate::planner::plan::AggExpr],
        ) -> Option<FastPath> {
            if !group_by.is_empty() || aggregates.len() != 1 {
                return None;
            }
            let agg = &aggregates[0];
            if !agg.func.eq_ignore_ascii_case("count") || agg.arg.is_some() || agg.distinct {
                return None;
            }
            // Only a plain table scan (no predicate; vtabs fall back —
            // their cursor protocol must run).
            let Plan::Scan {
                table,
                predicate: None,
                ..
            } = input
            else {
                return None;
            };
            if table.vtab.is_some() {
                return None;
            }
            Some(FastPath::CountStar {
                table: table.clone(),
                columns: std::sync::Arc::from(vec![agg.display_name.clone()]),
            })
        }
        if let Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } = plan
        {
            if let Some(fp) = covering_count_fp(input, group_by, aggregates) {
                return Some(fp);
            }
            if let Some(fp) = bare_count_star_fp(input, group_by, aggregates) {
                return Some(fp);
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
            Plan::RowidRange {
                table,
                start: Some(s),
                end: Some(e),
                residual: None,
                ..
            } => {
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
                Plan::IndexLookup {
                    table,
                    index,
                    key_exprs,
                    ..
                } => {
                    let keys = key_exprs
                        .iter()
                        .map(bind_expr)
                        .collect::<Option<Vec<_>>>()?;
                    let (project, cols) = resolve_projection(columns, table)?;
                    let pre_encoded = pre_encode_literal_keys(&keys);
                    Some(FastPath::IndexPoint {
                        table: table.clone(),
                        index: index.clone(),
                        keys,
                        pre_encoded,
                        project,
                        columns: cols,
                    })
                }
                Plan::RowidRange {
                    table,
                    start: Some(s),
                    end: Some(e),
                    residual: None,
                    ..
                } => {
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
                // Project { Aggregate { IndexLookup } } — the planner's
                // wrapped form of the covering-index COUNT (see above).
                // Project { Aggregate { Scan } } — the wrapped form of the
                // bare `SELECT COUNT(*) FROM t` (see bare_count_star_fp).
                Plan::Aggregate {
                    input,
                    group_by,
                    aggregates,
                } if group_by.is_empty() && aggregates.len() == 1 => {
                    // Only when the projection is the trivial single-column
                    // pass-through of the aggregate output.
                    let trivial = columns.len() == 1
                        && matches!(&columns[0].expr, Expr::Column { name, .. }
                            if name.starts_with("__agg_"));
                    if !trivial {
                        return None;
                    }
                    if let Plan::IndexLookup {
                        table,
                        index,
                        key_exprs,
                        ..
                    } = input.as_ref()
                    {
                        let keys = key_exprs
                            .iter()
                            .map(bind_expr)
                            .collect::<Option<Vec<_>>>()?;
                        let pre_encoded = pre_encode_literal_keys(&keys);
                        Some(FastPath::IndexCount {
                            table: table.clone(),
                            index: index.clone(),
                            keys,
                            pre_encoded,
                            columns: std::sync::Arc::from(vec![aggregates[0].display_name.clone()]),
                        })
                    } else {
                        bare_count_star_fp(input, group_by, aggregates)
                    }
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
    pub(crate) fn run_fast_path_public(&self, fp: &FastPath, params: &[Value]) -> Result<Vec<Row>> {
        self.run_fast_path(fp, params)
    }

    fn run_fast_path(&self, fp: &FastPath, params: &[Value]) -> Result<Vec<Row>> {
        // Root-override resolution. `maps_populated` is a single atomic
        // load: the maps only gain root entries after a B+tree split
        // actually moves a root (rare), and until then an empty check
        // replaces the read-lock + two HashMap probes on this hottest
        // OLTP path. Writers hold `&mut self`, so the flag can never be
        // stale while a reader runs.
        let (table_root, index_root) = if self.maps_populated.load(Ordering::Acquire) {
            let m = self.maps.read();
            let name = fp.table_name();
            let t = m
                .roots
                .get(name)
                .copied()
                .or_else(|| m.roots.get(&name.to_ascii_lowercase()).copied());
            let i = match fp {
                FastPath::IndexPoint { index, .. } => m
                    .index_roots
                    .get(&index.name)
                    .copied()
                    .or_else(|| m.index_roots.get(&index.name.to_ascii_lowercase()).copied()),
                _ => None,
            };
            (t, i)
        } else {
            (None, None)
        };
        match fp {
            FastPath::RowidPoint {
                table,
                rowid,
                project,
                columns: _,
            } => {
                let rid = rowid.resolve(params).as_integer();
                let root = table_root.unwrap_or(table.root_page);
                let mut bt = Btree::new(&self.pager, root, false);
                // Decode the projected row directly under the page lock —
                // no intermediate payload Vec copy.
                match bt.lookup_table_with(rid, |payload| {
                    decode_projected(payload, table, rid, project.as_deref())
                })? {
                    Some(row) => Ok(vec![row]),
                    None => Ok(Vec::new()),
                }
            }
            FastPath::IndexCount {
                index,
                keys,
                pre_encoded,
                ..
            } => {
                let mut key_scratch: Vec<u8>;
                let key_bytes: &[u8] = match pre_encoded {
                    Some(pre) => &pre[..],
                    None => {
                        key_scratch = Vec::with_capacity(keys.len() * 8);
                        for k in keys {
                            k.resolve(params).encode_order_key_into(&mut key_scratch);
                        }
                        &key_scratch
                    }
                };
                let iroot = index_root.unwrap_or(index.root_page);
                let mut ibt = Btree::new(&self.pager, iroot, true);
                let rowids = ibt.lookup_index(key_bytes)?;
                Ok(vec![vec![Value::Integer(rowids.len() as i64)]])
            }
            FastPath::CountStar { table, .. } => {
                // Epoch-keyed memoization: the walk sums n_cells over every
                // leaf (~1-2 us for 10k rows); SQLite's OP_Count pays that
                // every call. A write bumps write_epoch BEFORE mutating, and
                // writers are `&mut self` (exclusive with this `&self`
                // read), so a cached (epoch, count) pair can never outlive
                // the state it describes. Vtabs never reach this arm.
                let epoch = self.write_epoch.load(Ordering::Acquire);
                let key = table.name.to_ascii_lowercase();
                if let Some(&(e, n)) = self.table_count_cache.read().get(&key) {
                    if e == epoch {
                        return Ok(vec![vec![Value::Integer(n)]]);
                    }
                }
                let root = table_root.unwrap_or(table.root_page);
                let mut bt = Btree::new(&self.pager, root, false);
                let n = bt.count_rows()? as i64;
                self.table_count_cache.write().insert(key, (epoch, n));
                Ok(vec![vec![Value::Integer(n)]])
            }
            FastPath::IndexPoint {
                table,
                index,
                keys,
                pre_encoded,
                project,
                columns: _,
            } => {
                // Encode the key (same order-preserving encoding as the
                // general path's exec_index_lookup). All-literal keys were
                // pre-encoded at cache time — borrow, don't re-encode.
                // Short keys encode into a STACK buffer: a heap Vec costs
                // ~30 ns per lookup (alloc + free) on the dominant
                // single-key path.
                let mut key_heap: Vec<u8>;
                let mut key_stack = [0u8; 64];
                let mut key_stack_len = 0usize;
                let key_bytes: &[u8] = match pre_encoded {
                    Some(pre) => &pre[..],
                    None => {
                        // Resolve + encode each bound into the stack slice;
                        // spill to a heap Vec if any key doesn't fit.
                        let mut spilled = false;
                        'outer: {
                            for k in keys {
                                match k
                                    .resolve(params)
                                    .encode_order_key_into_slice(&mut key_stack[key_stack_len..])
                                {
                                    Some(n) => key_stack_len += n,
                                    None => {
                                        spilled = true;
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        if !spilled {
                            &key_stack[..key_stack_len]
                        } else {
                            key_heap = Vec::with_capacity(keys.len() * 8);
                            for k in keys {
                                k.resolve(params).encode_order_key_into(&mut key_heap);
                            }
                            &key_heap
                        }
                    }
                };
                let iroot = index_root.unwrap_or(index.root_page);
                let mut ibt = Btree::new(&self.pager, iroot, true);
                // Reusable per-thread rowid scratch: the results Vec
                // (almost always 0-2 rowids on point lookups) cost a fresh
                // malloc + free per query (~25-30 ns) on this hottest OLTP
                // path. Borrowed only for the duration of the call.
                thread_local! {
                    static ROWID_SCRATCH: std::cell::RefCell<Vec<i64>> =
                        std::cell::RefCell::new(Vec::with_capacity(8));
                }
                let rows = ROWID_SCRATCH.with(|scratch| {
                    let mut rowids = std::mem::take(&mut *scratch.borrow_mut());
                    let r = (|| -> Result<Vec<Row>> {
                        ibt.lookup_index_into(key_bytes, &mut rowids)?;
                        if rowids.is_empty() {
                            return Ok(Vec::new());
                        }
                        let troot = table_root.unwrap_or(table.root_page);
                        let mut tbt = Btree::new(&self.pager, troot, false);
                        let mut rows = Vec::with_capacity(rowids.len());
                        for &rid in &rowids {
                            if let Some(row) = tbt.lookup_table_with(rid, |payload| {
                                decode_projected(payload, table, rid, project.as_deref())
                            })? {
                                rows.push(row);
                            }
                        }
                        Ok(rows)
                    })();
                    // Return the (possibly grown) buffer unless it ballooned.
                    if rowids.capacity() <= 4096 {
                        *scratch.borrow_mut() = rowids;
                    }
                    r
                })?;
                Ok(rows)
            }
            FastPath::RowidRange {
                table,
                start,
                end,
                project,
                columns: _,
            } => {
                let lo = start.resolve(params).as_integer();
                let hi = end.resolve(params).as_integer();
                let root = table_root.unwrap_or(table.root_page);
                let mut bt = Btree::new(&self.pager, root, false);
                // Pre-size the result from the range width: a growing Vec
                // pays ~13 realloc+copy rounds on a 5000-row scan, and the
                // FIRST big query after a write storm additionally pays
                // fresh large-object carving for every doubling — measured
                // 711 us vs 368 us steady for range-5000. Cap the estimate
                // so absurdly wide ranges (BETWEEN 1 AND 9e18) don't
                // over-allocate.
                let est = if hi > lo {
                    (hi - lo).min(1 << 20) as usize + 1
                } else {
                    0
                };
                let mut rows = Vec::with_capacity(est);
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

    pub(crate) fn plan_insert(
        catalog: &Catalog,
        stmt: &Statement,
    ) -> Result<crate::planner::plan::Plan> {
        let ins = match stmt {
            Statement::Insert(i) => i,
            _ => unreachable!(),
        };
        let table = catalog
            .get_table(&ins.table)
            .ok_or_else(|| Error::NotFound(format!("table: {}", ins.table)))?;
        // Plan the source.
        let source_plan = match &ins.source {
            InsertSource::Values(rows) => crate::planner::plan::Plan::Values { rows: rows.clone() },
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
                let idx = resolve_insert_column(&table, c).ok_or_else(|| {
                    Error::semantic(format!("column {} not in table {}", c, table.name))
                })?;
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

    pub(crate) fn plan_update(
        catalog: &Catalog,
        stmt: &Statement,
    ) -> Result<crate::planner::plan::Plan> {
        let upd = match stmt {
            Statement::Update(u) => u,
            _ => unreachable!(),
        };
        let table = catalog
            .get_table(&upd.table)
            .ok_or_else(|| Error::NotFound(format!("table: {}", upd.table)))?;
        let scan = crate::planner::plan::Plan::Scan {
            table: table.clone(),
            alias: upd.alias.clone(),
            index: None,
            predicate: None,
        };
        // `UPDATE ... FROM` (SQLite 3.33+): the target table is joined
        // with the FROM side; the WHERE clause references BOTH sides, so
        // it is evaluated over combined rows by the executor instead of
        // being pushed into the target scan.
        let from = upd
            .from
            .as_ref()
            .map(|te| {
                let mut planner = crate::planner::Planner::new(catalog);
                let plan = planner.plan_table_expression_pub(te)?;
                Ok::<_, Error>(Box::new(crate::planner::plan::UpdateFrom {
                    plan,
                    where_clause: upd.where_clause.clone(),
                }))
            })
            .transpose()?;
        // Use apply_where_for_scan so that `UPDATE t SET ... WHERE id = ?`
        // picks RowidLookup instead of a full table scan. Previously this
        // built a Filter{Scan, predicate} which forced an O(n) scan per
        // UPDATE — a ~743x regression on the UPDATE-by-PK benchmark.
        // (Skipped for UPDATE...FROM: the WHERE spans both sides.)
        let source = if let (Some(pred), None) = (&upd.where_clause, &from) {
            crate::planner::apply_where_for_scan(catalog, scan, pred)
        } else {
            scan
        };
        let assignments: Vec<(usize, Expr)> = upd
            .set
            .iter()
            .map(|(col, expr)| {
                let idx = table.find_column(col).unwrap_or(0);
                (idx, expr.clone())
            })
            .collect();
        Ok(crate::planner::plan::Plan::Update {
            table,
            source: Box::new(source),
            assignments,
            returning: upd.returning.clone(),
            or_conflict: upd.or.unwrap_or(ConflictResolution::Abort),
            from,
        })
    }

    pub(crate) fn plan_delete(
        catalog: &Catalog,
        stmt: &Statement,
    ) -> Result<crate::planner::plan::Plan> {
        let del = match stmt {
            Statement::Delete(d) => d,
            _ => unreachable!(),
        };
        let table = catalog
            .get_table(&del.from)
            .ok_or_else(|| Error::NotFound(format!("table: {}", del.from)))?;
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

    fn execute_statement_static(
        stmt: &Statement,
        ctx: &mut ExecContext,
        catalog: &mut Catalog,
        original_sql: &str,
    ) -> Result<()> {
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
                ctx.pager.clear_savepoints();
                ctx.pager.flush()?;
                Ok(())
            }
            Statement::Rollback(_) => {
                // Restore the pager to the snapshot taken at BEGIN. Any
                // active savepoints die with the transaction.
                ctx.pager.clear_savepoints();
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
            // Savepoint statements are intercepted in execute() (they need
            // &self for the bookkeeping-map snapshots); reaching this arm
            // means they were executed through a path without map access —
            // treat as no-ops to stay total.
            Statement::Savepoint(_) | Statement::Release(_) => Ok(()),
            Statement::Select(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_) => {
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

    fn execute_create(
        c: CreateStatement,
        ctx: &mut ExecContext,
        catalog: &mut Catalog,
        original_sql: &str,
    ) -> Result<()> {
        match c {
            CreateStatement::Table {
                name,
                columns,
                constraints,
                without_rowid,
                strict,
                if_not_exists,
            } => {
                // Tables and views share one namespace (see the View arm).
                if catalog.get_table(&name.name).is_some() || catalog.get_view(&name.name).is_some()
                {
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
                let table = build_table(
                    &name.name,
                    &columns,
                    &constraints,
                    root_page,
                    without_rowid,
                    strict,
                    original_sql,
                )?;
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
                    if col
                        .constraints
                        .iter()
                        .any(|c| matches!(c, crate::sql::ast::ColumnConstraint::Unique))
                    {
                        // Inherit the column's declared COLLATE (SQLite:
                        // the implicit auto-index uses the column's
                        // collation, so `email TEXT UNIQUE COLLATE NOCASE`
                        // enforces case-insensitive uniqueness).
                        let collate = col.constraints.iter().find_map(|c| {
                            if let crate::sql::ast::ColumnConstraint::Collate(name) = c {
                                Some(name.clone())
                            } else {
                                None
                            }
                        });
                        implicit.push(vec![crate::sql::ast::IndexedColumn {
                            name: col.name.clone(),
                            order: crate::sql::ast::Order::Asc,
                            collation: collate,
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
                    // Synthesized SQL: emit each column's COLLATE so the
                    // implicit index round-trips with its collation on
                    // reopen (the catalog IndexColumn carries it too).
                    let col_list = cols
                        .iter()
                        .map(|ic| {
                            let coll = match &ic.collation {
                                Some(c) => format!(" COLLATE \"{}\"", c),
                                None => String::new(),
                            };
                            format!("\"{}\"{}", ic.name, coll)
                        })
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
                    // SQLite stores NULL sql for auto-indexes; the catalog
                    // keeps the synthesized DDL for reopen (load_schema
                    // rebuilds implicit indexes from the TABLE's DDL and
                    // matches them to these rows by name + rootpage).
                    let schema_row = crate::schema::encode_schema_row_opt(
                        "index",
                        &index.name,
                        &index.table,
                        idx_root,
                        None,
                    );
                    insert_schema_row(ctx.pager, &schema_row)?;
                    catalog.add_index(index);
                }
                ctx.pager.flush()?;
                Ok(())
            }
            CreateStatement::Index {
                unique,
                if_not_exists,
                name,
                table: table_name,
                columns,
                where_clause,
            } => {
                if catalog.get_index(&name).is_some() {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(Error::AlreadyExists(format!("index: {}", name)));
                }
                let table = catalog
                    .get_table(&table_name)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", table_name)))?;
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
                    let mut index_bt =
                        crate::storage::btree::Btree::new(ctx.pager, root_page, true);
                    let partial = index.partial_expr.clone();
                    let is_unique = index.unique;
                    let mut backfill_err: Option<crate::error::Error> = None;
                    let mut table_bt =
                        crate::storage::btree::Btree::new(ctx.pager, table_root, false);
                    table_bt.scan_table_borrowed(|rowid, payload| {
                        if crate::storage::row_codec::decode_row_into(
                            payload,
                            n_cols,
                            rowid,
                            alias,
                            &mut row_buf,
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
                        // NULLs are distinct: rows with ANY NULL among
                        // the indexed columns are exempt from the UNIQUE
                        // duplicate check (SQLite semantics — you can
                        // CREATE UNIQUE INDEX over data holding many NULLs).
                        let has_null_key = index.columns.iter().any(|c| {
                            table
                                .find_column(&c.name)
                                .and_then(|p| row_buf.get(p))
                                .map(|v| v.is_null())
                                .unwrap_or(false)
                        });
                        let key = crate::executor::encode_index_key(&index, &table, &row_buf);
                        if is_unique && !has_null_key {
                            match index_bt.lookup_index(&key) {
                                Ok(existing) if !existing.is_empty() => {
                                    backfill_err = Some(Error::constraint(format!(
                                        "UNIQUE constraint failed: {}.{}",
                                        table_name,
                                        index
                                            .columns
                                            .iter()
                                            .map(|c| c.name.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
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
                // Post-build warm tap: validate the boundary leaves and
                // seed the leaf-hint cache through the read path, so the
                // first user query after the build doesn't pay the
                // read-path wake-up (see Btree::warm_read_path).
                if std::env::var_os("RUSTQLITE_NO_WARM_TAP").is_none() {
                    let mut warm_bt =
                        crate::storage::btree::Btree::new(ctx.pager, final_root, true);
                    let _ = warm_bt.warm_read_path();
                }
                ctx.pager.flush()?;
                Ok(())
            }
            CreateStatement::View {
                name,
                columns,
                select,
                if_not_exists,
            } => {
                // Tables and views share one namespace (SQLite: creating a
                // view with an existing TABLE's name fails with "table X
                // already exists", and vice versa). Without this check, a
                // fuzzed `CREATE VIEW t AS ... FROM t` silently shadows a
                // real table t — and a self-referencing shadow is exactly
                // how infinite view-expansion recursion arises.
                if catalog.get_view(&name.name).is_some() || catalog.get_table(&name.name).is_some()
                {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(Error::AlreadyExists(format!("table: {}", name.name)));
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
            CreateStatement::VirtualTable {
                if_not_exists,
                name,
                module,
                args,
            } => {
                if catalog.get_table(&name.name).is_some() || catalog.get_view(&name.name).is_some()
                {
                    if if_not_exists {
                        return Ok(());
                    }
                    return Err(Error::AlreadyExists(format!("table: {}", name.name)));
                }
                // Module lookup through the thread-local plugin scope —
                // the CREATE statement runs inside `execute()`'s
                // PluginScopeGuard (see the `_plugin_guard` installs).
                let module = crate::plugin::lookup_module(&module)
                    .ok_or_else(|| Error::semantic(format!("no such module: {}", module)))?;
                // xCreate: the instance supplies the column schema.
                let instance = module.create(&name.name, &args)?;
                let cols = instance.columns();
                let mut table = crate::plugin::vtab::vtab_columns_to_schema(&name.name, &cols);
                table.create_sql = original_sql.to_string();
                let vtab = crate::plugin::vtab::VtabInstance::connected(
                    name.name.clone(),
                    module.clone(),
                    args,
                    instance,
                );
                // on_create: module hook for external-state setup.
                let _ = vtab.with_table(|vt| vt.on_create());
                table.vtab = Some(std::sync::Arc::new(vtab));
                // rootpage 0: no B+tree. The schema row's SQL round-trips
                // through the parser on reopen (see load_schema).
                let schema_row = crate::schema::encode_schema_row(
                    "table",
                    &table.name,
                    &table.name,
                    0,
                    &table.create_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                catalog.add_table(table);
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
                // SQLite: the schema tables are not droppable.
                if d.name.eq_ignore_ascii_case("sqlite_master")
                    || d.name.eq_ignore_ascii_case("sqlite_schema")
                    || d.name.eq_ignore_ascii_case("sqlite_temp_master")
                {
                    return Err(Error::semantic(format!(
                        "table {} may not be dropped",
                        d.name
                    )));
                }
                // Capture indexes BEFORE the catalog drop removes them.
                let indexes_on_it = catalog.indexes_on_table(&d.name);
                let table = catalog
                    .drop_table(&d.name)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", d.name)))?;
                if let Some(vtab) = &table.vtab {
                    // Virtual table: no pages to free; run xDestroy on the
                    // module so it can drop external state.
                    if let Ok((module, args)) = vtab.module_and_args() {
                        let _ = module.destroy(&d.name, &args);
                    }
                    delete_schema_row(ctx.pager, "table", &d.name)?;
                    ctx.pager.flush()?;
                    return Ok(());
                }
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
                        if let Some((kind, _n, tbl_name, _rootpage, _sql)) =
                            crate::schema::decode_schema_row(&row)
                        {
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
                let idx = catalog
                    .drop_index(&d.name)
                    .ok_or_else(|| Error::NotFound(format!("index: {}", d.name)))?;
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
                catalog
                    .rename_table(&old_name, &new_name)
                    .ok_or_else(|| Error::AlreadyExists(format!("table: {}", new_name)))?;
                // Replace the moved entry's Arc with the rebuilt table.
                catalog.replace_table(&new_name, rebuilt);

                // Other tables' REFERENCES clauses follow the rename
                // (SQLite modern rename mode rewrites them).
                catalog.rename_fk_references(&old_name, &new_name);

                // Schema row: delete old, insert new (kind=table,
                // name/tbl_name=new, same root, new SQL).
                delete_schema_row(ctx.pager, "table", &old_name)?;
                let schema_row =
                    crate::schema::encode_schema_row("table", &new_name, &new_name, root, &new_sql);
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
                    let names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
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
                if let Ok(crate::sql::ast::Statement::Create(
                    crate::sql::ast::CreateStatement::Table {
                        columns,
                        constraints,
                        without_rowid,
                        strict,
                        ..
                    },
                )) = crate::sql::parser::parse(&new_sql)
                {
                    rebuilt = build_table(
                        &table.name,
                        &columns,
                        &constraints,
                        root,
                        without_rowid,
                        strict,
                        &new_sql,
                    )?;
                }
                let table_name = rebuilt.name.clone();
                catalog.drop_table(&a.table);
                catalog.add_table(rebuilt);

                // Schema row rewrite.
                delete_schema_row(ctx.pager, "table", &table.name)?;
                let schema_row = crate::schema::encode_schema_row(
                    "table",
                    &table_name,
                    &table_name,
                    root,
                    &new_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;

                // Physical back-fill: append the default to every existing
                // row. (SQLite stores the default in the schema and
                // materializes it at read time; a one-time rewrite is the
                // same observable behavior.)
                if !default_value.is_null() {
                    let n_cols = table.n_columns();
                    let alias = table.rowid_alias;
                    // LIVE root — the catalog Arc's root_page can lag a
                    // recent split (sync_schema_roots persists new roots to
                    // schema rows; the in-memory Arc keeps the old value
                    // until the table is re-fetched).
                    let live_root = ctx.table_root(&table);
                    let mut updates: Vec<(i64, Vec<u8>)> = Vec::new();
                    {
                        let mut bt = Btree::new(ctx.pager, live_root, false);
                        bt.scan_table(|rowid, payload| {
                            if let Ok(mut row) = decode_row(payload, n_cols, rowid, alias) {
                                row.push(default_value.clone());
                                let new_payload = encode_row_aliased(&row, alias);
                                updates.push((rowid, new_payload));
                            }
                            true
                        })?;
                    }
                    let mut bt = Btree::new(ctx.pager, live_root, false);
                    for (rowid, payload) in updates {
                        let did = bt.update_table(rowid, &payload).unwrap_or(false);
                        if !did {
                            bt.delete_table(rowid)?;
                            bt.insert_table(rowid, &payload)?;
                        }
                    }
                    if bt.root != live_root {
                        ctx.set_table_root(&table.name, bt.root);
                    }
                }
                ctx.pager.flush()?;
                Ok(())
            }
            AlterAction::RenameColumn { old, new } => {
                let table = catalog
                    .get_table(&a.table)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", a.table)))?;
                // LIVE root (catalog Arc may lag a split).
                // LIVE root (catalog Arc may lag a split): use the
                // override-aware root for both the rebuilt table entry and
                // the rewritten schema row.
                let root = ctx.table_root(&table);
                let old_name_lc = old.to_ascii_lowercase();
                // Resolve the column (case-insensitive).
                let col_idx = table
                    .columns
                    .iter()
                    .position(|c| c.name.to_ascii_lowercase() == old_name_lc)
                    .ok_or_else(|| Error::NotFound(format!("column: {}", old)))?;
                // New name must be unique in the table.
                if table
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&new))
                {
                    return Err(Error::AlreadyExists(format!("column: {}", new)));
                }
                if new.is_empty()
                    || new
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                    || new
                        .chars()
                        .any(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                {
                    return Err(Error::semantic(format!("invalid column name: {}", new)));
                }
                let old_name = table.columns[col_idx].name.clone();
                let table_name = table.name.clone();

                // 1. Rewrite this table's CREATE statement (column def,
                //    CHECK/DEFAULT/GENERATED exprs, PK/UNIQUE lists, child
                //    FK columns, self-referencing FK parent columns).
                let new_sql =
                    rename_column_in_create_table(&table.create_sql, &table_name, &old_name, &new)?;

                // 2. Rebuild the catalog entry from the new SQL.
                let mut rebuilt = (*table).clone();
                if let Ok(crate::sql::ast::Statement::Create(
                    crate::sql::ast::CreateStatement::Table {
                        columns,
                        constraints,
                        without_rowid,
                        strict,
                        ..
                    },
                )) = crate::sql::parser::parse(&new_sql)
                {
                    rebuilt = build_table(
                        &table_name,
                        &columns,
                        &constraints,
                        root,
                        without_rowid,
                        strict,
                        &new_sql,
                    )?;
                }
                catalog.replace_table(&table_name, rebuilt);

                // 3. Other tables' REFERENCES clauses pointing at the
                //    renamed parent column.
                for (other_name, other) in catalog.all_tables() {
                    if other.name.eq_ignore_ascii_case(&table_name) {
                        continue;
                    }
                    let references_us = other.foreign_keys.iter().any(|fk| {
                        fk.ref_table.eq_ignore_ascii_case(&table_name)
                            && fk
                                .ref_columns
                                .iter()
                                .any(|rc| rc.eq_ignore_ascii_case(&old_name))
                    });
                    if references_us {
                        if let Ok(new_other_sql) = rename_fk_refs_in_create_table(
                            &other.create_sql,
                            &table_name,
                            &old_name,
                            &new,
                        ) {
                            let mut rebuilt_other = (*other).clone();
                            if let Ok(crate::sql::ast::Statement::Create(
                                crate::sql::ast::CreateStatement::Table {
                                    columns,
                                    constraints,
                                    without_rowid,
                                    strict,
                                    ..
                                },
                            )) = crate::sql::parser::parse(&new_other_sql)
                            {
                                if let Ok(t) = build_table(
                                    &other.name,
                                    &columns,
                                    &constraints,
                                    other.root_page,
                                    without_rowid,
                                    strict,
                                    &new_other_sql,
                                ) {
                                    rebuilt_other = t;
                                }
                            }
                            let other_root = ctx.table_root(&other);
                            let other_display = other.name.clone();
                            catalog.replace_table(&other_name, rebuilt_other);
                            delete_schema_row(ctx.pager, "table", &other_display)?;
                            let schema_row = crate::schema::encode_schema_row(
                                "table",
                                &other_display,
                                &other_display,
                                other_root,
                                &new_other_sql,
                            );
                            insert_schema_row(ctx.pager, &schema_row)?;
                        }
                    }
                }

                // 4. Indexes on this table: rename the indexed column in
                //    their CREATE statements (and the catalog's Index columns).
                for (idx_name, idx) in catalog.all_indexes() {
                    if idx.table.eq_ignore_ascii_case(&table_name)
                        && idx
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&old_name))
                    {
                        if let Ok(new_idx_sql) =
                            rename_column_in_create_index(&idx.create_sql, &old_name, &new)
                        {
                            let mut rebuilt_idx = (*idx).clone();
                            for ic in rebuilt_idx.columns.iter_mut() {
                                if ic.name.eq_ignore_ascii_case(&old_name) {
                                    ic.name = new.clone();
                                }
                            }
                            rebuilt_idx.create_sql = new_idx_sql.clone();
                            let idx_root = ctx.index_root(&idx);
                            let display_name = idx.name.clone();
                            catalog.replace_index(&idx_name, rebuilt_idx);
                            delete_schema_row(ctx.pager, "index", &display_name)?;
                            let schema_row = crate::schema::encode_schema_row(
                                "index",
                                &display_name,
                                &table_name,
                                idx_root,
                                &new_idx_sql,
                            );
                            insert_schema_row(ctx.pager, &schema_row)?;
                        }
                    }
                }

                // 5. Triggers and views that reference the column: token-level
                //    rewrite of their stored SQL (qualified refs, UPDATE OF
                //    lists, INSERT column lists, SET targets).
                rewrite_object_sql_column_refs(ctx.pager, catalog, &table_name, &old_name, &new)?;

                // 6. Rewrite this table's schema row.
                delete_schema_row(ctx.pager, "table", &table_name)?;
                let schema_row = crate::schema::encode_schema_row(
                    "table",
                    &table_name,
                    &table_name,
                    root,
                    &new_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                ctx.pager.flush()?;
                Ok(())
            }
            AlterAction::DropColumn { name } => {
                let table = catalog
                    .get_table(&a.table)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", a.table)))?;
                let name_lc = name.to_ascii_lowercase();
                let col_idx = table
                    .columns
                    .iter()
                    .position(|c| c.name.to_ascii_lowercase() == name_lc)
                    .ok_or_else(|| Error::NotFound(format!("column: {}", name)))?;
                // Rowid alias (INTEGER PRIMARY KEY) can't be dropped.
                if table.rowid_alias == Some(col_idx) {
                    return Err(Error::semantic(
                        "cannot drop the INTEGER PRIMARY KEY column",
                    ));
                }
                if table.columns.len() <= 1 {
                    return Err(Error::semantic(format!(
                        "cannot drop column \"{}\": no other columns exist",
                        name
                    )));
                }
                let col = &table.columns[col_idx];
                // PRIMARY KEY / UNIQUE columns can't be dropped.
                if col.primary_key {
                    return Err(Error::semantic(format!(
                        "cannot drop PRIMARY KEY column: \"{}\"",
                        name
                    )));
                }
                if col.unique {
                    return Err(Error::semantic(format!(
                        "cannot drop UNIQUE column: \"{}\"",
                        name
                    )));
                }
                // Indexed columns (explicit indexes, incl. partial-index
                // WHERE clauses) can't be dropped.
                for idx in catalog.indexes_on_table(&table.name) {
                    if idx
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&name))
                    {
                        return Err(Error::semantic(format!(
                            "cannot drop indexed column: \"{}\" (index \"{}\" uses it)",
                            name, idx.name
                        )));
                    }
                    if let Some(w) = &idx.partial_expr {
                        if expr_references_column(w, &name, &table.name) {
                            return Err(Error::semantic(format!(
                                "cannot drop column \"{}\": referenced by partial index \"{}\"",
                                name, idx.name
                            )));
                        }
                    }
                }
                // CHECK constraints referencing it.
                for ck in &table.check_exprs {
                    if expr_references_column(ck, &name, &table.name) {
                        return Err(Error::semantic(format!(
                            "cannot drop column \"{}\": referenced by a CHECK constraint",
                            name
                        )));
                    }
                }
                // Generated columns referencing it.
                for c in &table.columns {
                    if let Some((ge, _)) = &c.generated {
                        if expr_references_column(ge, &name, &table.name) {
                            return Err(Error::semantic(format!(
                                "cannot drop column \"{}\": referenced by generated column \"{}\"",
                                name, c.name
                            )));
                        }
                    }
                }
                // FOREIGN KEYs (child or parent side) referencing it.
                for fk in &table.foreign_keys {
                    if fk.columns.iter().any(|&ci| {
                        table
                            .columns
                            .get(ci)
                            .map(|c| c.name.eq_ignore_ascii_case(&name))
                            .unwrap_or(false)
                    }) {
                        return Err(Error::semantic(format!(
                            "cannot drop column \"{}\": used in a FOREIGN KEY constraint",
                            name
                        )));
                    }
                }
                for (other_name, other) in catalog.all_tables() {
                    if other.name.eq_ignore_ascii_case(&table.name) {
                        continue;
                    }
                    let refs_us = other.foreign_keys.iter().any(|fk| {
                        fk.ref_table.eq_ignore_ascii_case(&table.name)
                            && fk
                                .ref_columns
                                .iter()
                                .any(|rc| rc.eq_ignore_ascii_case(&name))
                    });
                    if refs_us {
                        return Err(Error::semantic(format!(
                            "cannot drop column \"{}\": referenced by a FOREIGN KEY on table \"{}\"",
                            name, other_name
                        )));
                    }
                }
                // Views/triggers referencing it. Views: bare references
                // count only when the view reads exactly this table;
                // qualified references always count. Triggers: bare
                // references count for triggers on this table.
                for (vname, view) in catalog.all_views() {
                    let single_table_view = view_is_single_table_over(&view.select, &table.name);
                    if object_sql_references_column(
                        &view.create_sql,
                        &table.name,
                        &name,
                        single_table_view,
                    ) || (single_table_view
                        && object_sql_references_column(
                            &view.create_sql,
                            &table.name,
                            &name,
                            false,
                        ))
                    {
                        return Err(Error::semantic(format!(
                            "cannot drop column \"{}\": referenced by view \"{}\"",
                            name, vname
                        )));
                    }
                }
                for (tname, trig) in catalog.all_triggers() {
                    if trig.table.eq_ignore_ascii_case(&table.name)
                        && object_sql_references_column(&trig.create_sql, &table.name, &name, true)
                    {
                        return Err(Error::semantic(format!(
                            "cannot drop column \"{}\": referenced by trigger \"{}\"",
                            name, tname
                        )));
                    }
                }

                let root = ctx.table_root(&table);
                let table_name = table.name.clone();
                let old_n_cols = table.n_columns();
                let old_alias = table.rowid_alias;
                let dropped_is_before_alias =
                    table.rowid_alias.map(|a| col_idx < a).unwrap_or(false);

                // 1. Rewrite the CREATE statement without the column.
                let new_sql = drop_column_from_create_table(&table.create_sql, &name)?
                    .ok_or_else(|| Error::NotFound(format!("column: {}", name)))?;

                // 2. Rebuild the catalog entry.
                let mut rebuilt = (*table).clone();
                if let Ok(crate::sql::ast::Statement::Create(
                    crate::sql::ast::CreateStatement::Table {
                        columns,
                        constraints,
                        without_rowid,
                        strict,
                        ..
                    },
                )) = crate::sql::parser::parse(&new_sql)
                {
                    rebuilt = build_table(
                        &table_name,
                        &columns,
                        &constraints,
                        root,
                        without_rowid,
                        strict,
                        &new_sql,
                    )?;
                }
                let new_n_cols = rebuilt.n_columns();
                catalog.replace_table(&table_name, rebuilt);

                // 3. Rewrite every row: decode with the OLD schema, drop
                //    column `col_idx`, re-encode with the NEW schema.
                //    (SQLite rewrites the table on disk too — see
                //    sqlite3AlterDropColumn's "Edit rows of table on disk".)
                {
                    // Use the LIVE root (the catalog Arc may lag a split
                    // that sync_schema_roots has only persisted to the
                    // schema row, not the in-memory Table).
                    let live_root = ctx.table_root(&table);
                    let mut updates: Vec<(i64, Vec<u8>)> = Vec::new();
                    {
                        let mut bt = Btree::new(ctx.pager, live_root, false);
                        bt.scan_table(|rowid, payload| {
                            if let Ok(mut row) = decode_row(payload, old_n_cols, rowid, old_alias) {
                                if col_idx < row.len() {
                                    row.remove(col_idx);
                                }
                                // Rowid-alias marker may shift: the alias
                                // column's encoded position moved when a
                                // column before it was dropped.
                                let new_alias = if dropped_is_before_alias {
                                    old_alias.map(|a| a - 1)
                                } else {
                                    old_alias
                                };
                                let new_payload = encode_row_aliased(&row, new_alias);
                                updates.push((rowid, new_payload));
                            }
                            true
                        })?;
                    }
                    let mut bt = Btree::new(ctx.pager, live_root, false);
                    for (rowid, payload) in updates {
                        let did = bt.update_table(rowid, &payload).unwrap_or(false);
                        if !did {
                            bt.delete_table(rowid)?;
                            bt.insert_table(rowid, &payload)?;
                        }
                    }
                    if bt.root != live_root {
                        ctx.set_table_root(&table_name, bt.root);
                    }
                    let _ = new_n_cols;
                }

                // 4. Rewrite the schema row.
                delete_schema_row(ctx.pager, "table", &table_name)?;
                let schema_row = crate::schema::encode_schema_row(
                    "table",
                    &table_name,
                    &table_name,
                    root,
                    &new_sql,
                );
                insert_schema_row(ctx.pager, &schema_row)?;
                ctx.pager.flush()?;
                Ok(())
            }
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
            let v = crate::executor::evaluate(value_as_expr(value), &eval_ctx)?;
            match name.as_str() {
                "foreign_keys" | "recursive_triggers" => {
                    // Accept booleans, numbers, and ON/OFF bare words.
                    let on = match (&v, value_as_expr(value)) {
                        (_, crate::sql::ast::Expr::Column { name, .. }) => {
                            matches!(name.to_ascii_lowercase().as_str(), "on" | "true" | "1")
                        }
                        _ => v.is_truthy(),
                    };
                    if name == "foreign_keys" {
                        ctx.pager.set_foreign_keys_enabled(on);
                    } else {
                        ctx.pager.set_recursive_triggers_enabled(on);
                    }
                }
                "journal_mode" => {
                    // `PRAGMA journal_mode = WAL` parses WAL as a bare
                    // identifier (column ref), which evaluates to NULL —
                    // recover the spelled name from the expression.
                    let mode = match (&v, value_as_expr(value)) {
                        (Value::Text(t), _) => t.to_ascii_lowercase(),
                        (_, crate::sql::ast::Expr::Column { name, .. }) => {
                            name.to_ascii_lowercase()
                        }
                        (Value::Integer(i), _) => {
                            if *i == 0 {
                                "delete".to_string()
                            } else {
                                "wal".to_string()
                            }
                        }
                        _ => String::new(),
                    };
                    match mode.as_str() {
                        "wal" => ctx.pager.enable_wal()?,
                        "delete" | "truncate" | "persist" | "memory" | "off" => {
                            ctx.pager.disable_wal()?;
                        }
                        _ => {}
                    }
                }
                "codec" => {
                    // PRAGMA codec = <registered name> | none
                    let cname = match (&v, value_as_expr(value)) {
                        (Value::Text(t), _) => t.to_ascii_lowercase(),
                        (_, crate::sql::ast::Expr::Column { name, .. }) => {
                            name.to_ascii_lowercase()
                        }
                        _ => String::new(),
                    };
                    if cname.is_empty() || cname == "none" || cname == "plain" {
                        ctx.pager.set_codec(None)?;
                    } else {
                        let codec = crate::plugin::lookup_codec(&cname)
                            .ok_or_else(|| Error::semantic(format!("no such codec: {}", cname)))?;
                        ctx.pager.set_codec(Some(codec))?;
                    }
                }
                "synchronous" => {
                    let level = match (&v, value_as_expr(value)) {
                        (Value::Integer(i), _) => (*i).clamp(0, 3) as u8,
                        (Value::Text(t), _) => match t.to_ascii_lowercase().as_str() {
                            "off" => 0,
                            "normal" => 1,
                            "full" | "extra" => 2,
                            _ => 2,
                        },
                        (_, crate::sql::ast::Expr::Column { name, .. }) => {
                            match name.to_ascii_lowercase().as_str() {
                                "off" => 0,
                                "normal" => 1,
                                "full" | "extra" => 2,
                                _ => 2,
                            }
                        }
                        _ => 2,
                    };
                    ctx.pager.set_synchronous(level);
                }
                "locking_mode" => {
                    // Advisory round-trip (see Pager::locking_mode_exclusive).
                    let mode = match (&v, value_as_expr(value)) {
                        (Value::Text(t), _) => t.to_ascii_lowercase(),
                        (_, crate::sql::ast::Expr::Column { name, .. }) => {
                            name.to_ascii_lowercase()
                        }
                        _ => String::new(),
                    };
                    ctx.pager.set_locking_mode_exclusive(mode == "exclusive");
                }
                "wal_checkpoint" => {
                    ctx.pager.checkpoint_wal()?;
                }
                "cache_size" => {
                    // Advisory: the pager's cache capacity is set at open.
                    // Accept and ignore (no error) — SQLite treats this as
                    // a hint too.
                }
                "page_size" => {
                    // SQLite semantics: only effective before the database
                    // has content (or after VACUUM, which we don't have).
                    // Accepted silently (no error) when too late — matches
                    // SQLite ignoring the pragma mid-life.
                    if let Value::Integer(sz) = &v {
                        let _ = ctx.pager.try_set_page_size(*sz as u32);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `read_pragma` exposed for the statement layer (pub(crate)).
pub(crate) fn read_pragma_public(p: &PragmaStatement, db: &Database) -> Option<PragmaRows> {
    read_pragma(p, db)
}

/// A PRAGMA result set. SQLite pragmas return zero, one, or MANY rows with
/// a fixed column layout: `PRAGMA table_info(t)` is 6 columns × N rows,
/// `PRAGMA foreign_key_list(t)` is 7 columns × (N × ncols) rows. The
/// single-value pragmas (`PRAGMA page_size`) are one row named after the
/// pragma itself.
pub(crate) struct PragmaRows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl PragmaRows {
    fn single(name: &str, v: Value) -> Self {
        PragmaRows {
            columns: vec![name.to_string()],
            rows: vec![vec![v]],
        }
    }
}

/// Extract the argument of an argumented pragma (`PRAGMA table_info(t)`,
/// `PRAGMA index_list('t')`) — a bare identifier or string literal.
fn pragma_arg(v: Option<&crate::sql::ast::PragmaValue>) -> Option<String> {
    let e = value_as_expr(v?);
    match e {
        crate::sql::ast::Expr::Column { name, .. } => Some(name.clone()),
        crate::sql::ast::Expr::Literal(Value::Text(t)) => Some(t.to_string()),
        _ => None,
    }
}

/// `PRAGMA table_info(t)` / `PRAGMA table_xinfo(t)` result rows.
/// Column layout matches SQLite exactly:
/// (cid, name, type, notnull, dflt_value, pk[, hidden]).
/// `table_info` EXCLUDES generated (hidden) columns; `table_xinfo`
/// includes them with hidden=2 (VIRTUAL) / 3 (STORED).
fn pragma_table_info(t: &Arc<crate::schema::Table>, xinfo: bool) -> PragmaRows {
    let mut columns = vec![
        "cid".to_string(),
        "name".to_string(),
        "type".to_string(),
        "notnull".to_string(),
        "dflt_value".to_string(),
        "pk".to_string(),
    ];
    if xinfo {
        columns.push("hidden".to_string());
    }
    let mut rows = Vec::with_capacity(t.columns.len());
    for (cid, c) in t.columns.iter().enumerate() {
        // table_info skips generated columns (SQLite behavior); xinfo
        // includes them.
        if c.generated.is_some() && !xinfo {
            continue;
        }
        let mut row = vec![
            Value::Integer(cid as i64),
            Value::Text(c.name.clone().into()),
            Value::Text(c.declared_type.clone().into()),
            // SQLite quirk: plain `INTEGER PRIMARY KEY` reports notnull=0
            // (the NULL is replaced by an auto rowid at INSERT time).
            Value::Integer(i64::from(c.explicit_not_null)),
            c.default
                .as_ref()
                .map(|e| Value::Text(default_value_text(e).into()))
                .unwrap_or(Value::Null),
            Value::Integer(c.pk_seq as i64),
        ];
        if xinfo {
            // hidden (SQLite): 0 = normal, 2 = VIRTUAL generated,
            // 3 = STORED generated.
            let hidden = match &c.generated {
                Some((_, true)) => 3,
                Some((_, false)) => 2,
                None => 0,
            };
            row.push(Value::Integer(hidden));
        }
        rows.push(row);
    }
    PragmaRows { columns, rows }
}

/// Render a DEFAULT expression as SQL text (SQLite's `dflt_value` column).
/// CURRENT_TIMESTAMP / CURRENT_DATE / CURRENT_TIME render as bare
/// keywords (SQLite parses them as keywords, not function calls).
fn default_value_text(e: &Expr) -> String {
    if let Expr::Function { name, args, .. } = e {
        if args.is_empty() {
            let up = name.to_ascii_uppercase();
            if matches!(
                up.as_str(),
                "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME"
            ) {
                return up;
            }
        }
    }
    expr_to_sql(e)
}

/// `PRAGMA index_list(t)` — (seq, name, unique, origin, partial).
/// `origin`: 'c' = CREATE INDEX, 'u' = UNIQUE-constraint auto-index,
/// 'pk' = PRIMARY KEY auto-index (compound / non-integer / WITHOUT
/// ROWID). SQLite lists indexes in REVERSE creation order — root-page
/// allocation order is the creation proxy, so sort descending.
fn pragma_index_list(
    t: &Arc<crate::schema::Table>,
    catalog: &crate::schema::Catalog,
) -> PragmaRows {
    let mut idxs = catalog.indexes_on_table(&t.name);
    idxs.sort_by_key(|i| std::cmp::Reverse(i.root_page));
    let rows = idxs
        .iter()
        .enumerate()
        .map(|(seq, idx)| {
            let origin = if idx.name.starts_with("sqlite_autoindex_") {
                // The PK auto-index's column set equals the table's PK
                // (non-rowid-alias) column set; WITHOUT ROWID tables' PK
                // auto-index is always 'pk'.
                let pk_cols: Vec<&str> = t
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(i, c)| {
                        c.primary_key && (t.without_rowid || t.rowid_alias != Some(*i))
                    })
                    .map(|(_, c)| c.name.as_str())
                    .collect();
                let idx_cols: Vec<&str> = idx.columns.iter().map(|c| c.name.as_str()).collect();
                let same_set = pk_cols.len() == idx_cols.len()
                    && pk_cols
                        .iter()
                        .all(|p| idx_cols.iter().any(|c| c.eq_ignore_ascii_case(p)));
                if same_set {
                    "pk"
                } else {
                    "u"
                }
            } else {
                "c"
            };
            vec![
                Value::Integer(seq as i64),
                Value::Text(idx.name.clone().into()),
                Value::Integer(i64::from(idx.unique)),
                Value::Text(origin.into()),
                Value::Integer(i64::from(idx.partial_expr.is_some())),
            ]
        })
        .collect();
    PragmaRows {
        columns: vec![
            "seq".into(),
            "name".into(),
            "unique".into(),
            "origin".into(),
            "partial".into(),
        ],
        rows,
    }
}

/// `PRAGMA index_info(idx)` / `index_xinfo(idx)` —
/// (seqno, cid, name[, desc, coll, key]). `index_xinfo` additionally
/// appends the auxiliary rowid column (cid=-1, name NULL, key=0).
fn pragma_index_info(
    idx: &Arc<crate::schema::Index>,
    t: &Arc<crate::schema::Table>,
    xinfo: bool,
) -> PragmaRows {
    let mut columns = vec!["seqno".to_string(), "cid".to_string(), "name".to_string()];
    if xinfo {
        columns.push("desc".to_string());
        columns.push("coll".to_string());
        columns.push("key".to_string());
    }
    let mut rows = Vec::with_capacity(idx.columns.len());
    for (seqno, ic) in idx.columns.iter().enumerate() {
        let cid = t.find_column(&ic.name).map(|i| i as i64).unwrap_or(-1);
        let mut row = vec![
            Value::Integer(seqno as i64),
            Value::Integer(cid),
            Value::Text(ic.name.clone().into()),
        ];
        if xinfo {
            row.push(Value::Integer(i64::from(
                ic.order == crate::sql::ast::Order::Desc,
            )));
            row.push(Value::Text(ic.collation.clone().into()));
            row.push(Value::Integer(1)); // every indexed column is a key column
        }
        rows.push(row);
    }
    if xinfo {
        // The trailing auxiliary rowid entry (SQLite appends it to every
        // index_xinfo).
        rows.push(vec![
            Value::Integer(idx.columns.len() as i64),
            Value::Integer(-1),
            Value::Null,
            Value::Integer(0),
            Value::Text("BINARY".into()),
            Value::Integer(0),
        ]);
    }
    PragmaRows { columns, rows }
}

/// `PRAGMA foreign_key_list(t)` —
/// (id, seq, table, from, to, on_update, on_delete, match).
/// Action names match SQLite's spelling: NO ACTION, RESTRICT, SET NULL,
/// SET DEFAULT, CASCADE. `match` is always "NONE" for us (SQLite's
/// only non-default MATCH is deferrable-schema, which we don't support).
fn pragma_foreign_key_list(
    t: &Arc<crate::schema::Table>,
    catalog: &crate::schema::Catalog,
) -> PragmaRows {
    fn action_name(a: crate::sql::ast::ForeignKeyAction) -> Value {
        use crate::sql::ast::ForeignKeyAction as A;
        Value::Text(
            match a {
                A::NoAction => "NO ACTION",
                A::Restrict => "RESTRICT",
                A::SetNull => "SET NULL",
                A::SetDefault => "SET DEFAULT",
                A::Cascade => "CASCADE",
            }
            .into(),
        )
    }
    let mut rows = Vec::new();
    // SQLite lists FK clauses in reverse declaration order (id = 0 is the
    // LAST-declared constraint).
    for (id, fk) in t.foreign_keys.iter().rev().enumerate() {
        let _ = catalog; // parent lookup only needed for explicit ref_columns
        for (seq, &col_idx) in fk.columns.iter().enumerate() {
            let from = t
                .columns
                .get(col_idx)
                .map(|c| c.name.as_str())
                .unwrap_or("");
            let to = fk
                .ref_columns
                .get(seq)
                .cloned()
                .map(|c| Value::Text(c.into()))
                // Implicit form: REFERENCES parent (no columns) — SQLite
                // reports `to` as NULL.
                .unwrap_or(Value::Null);
            rows.push(vec![
                Value::Integer(id as i64),
                Value::Integer(seq as i64),
                Value::Text(fk.ref_table.clone().into()),
                Value::Text(from.to_string().into()),
                to,
                action_name(fk.on_update),
                action_name(fk.on_delete),
                Value::Text("NONE".into()),
            ]);
        }
    }
    PragmaRows {
        columns: vec![
            "id".into(),
            "seq".into(),
            "table".into(),
            "from".into(),
            "to".into(),
            "on_update".into(),
            "on_delete".into(),
            "match".into(),
        ],
        rows,
    }
}

fn read_pragma(p: &PragmaStatement, db: &Database) -> Option<PragmaRows> {
    let name = p.name.to_ascii_lowercase();
    // `PRAGMA journal_mode = WAL` / `journal_mode(WAL)`: the write form
    // RETURNS the resulting mode as a row (SQLite behavior — rusqlite and
    // ORMs read it back). The mode word parses as a bare identifier
    // (Column) or string literal.
    if p.value.is_some() && name == "journal_mode" {
        let mode = p.value.as_ref().and_then(|v| match value_as_expr(v) {
            crate::sql::ast::Expr::Column { name, .. } => Some(name.to_ascii_lowercase()),
            crate::sql::ast::Expr::Literal(Value::Text(t)) => Some(t.to_ascii_lowercase()),
            crate::sql::ast::Expr::Literal(Value::Integer(i)) => Some(if *i == 0 {
                "delete".to_string()
            } else {
                "wal".to_string()
            }),
            _ => None,
        });
        if let Some(mode) = mode {
            apply_journal_mode(db, &mode).ok()?;
            let pager = &db.pager;
            return Some(PragmaRows::single(
                "journal_mode",
                Value::Text(if pager.wal_enabled() { "wal" } else { "delete" }.into()),
            ));
        }
        return None;
    }
    // `PRAGMA locking_mode = EXCLUSIVE`: the write form RETURNS the new
    // mode as a row (SQLite behavior — connection setups read it back).
    // Semantics: advisory in this engine (single-process locking is
    // handled by the transaction slot), but the round-trip must match.
    if p.value.is_some() && name == "locking_mode" {
        let mode = p.value.as_ref().and_then(|v| match value_as_expr(v) {
            crate::sql::ast::Expr::Column { name, .. } => Some(name.to_ascii_lowercase()),
            crate::sql::ast::Expr::Literal(Value::Text(t)) => Some(t.to_ascii_lowercase()),
            _ => None,
        });
        if let Some(mode) = mode {
            db.pager.set_locking_mode_exclusive(mode == "exclusive");
            return Some(PragmaRows::single(
                "locking_mode",
                Value::Text(
                    if mode == "exclusive" {
                        "exclusive"
                    } else {
                        "normal"
                    }
                    .into(),
                ),
            ));
        }
        return None;
    }
    // Table-valued introspection pragmas: the argument arrives as the
    // call form (`PRAGMA table_info(t)`) or a string literal.
    let catalog = &db.catalog;
    match name.as_str() {
        "table_info" | "table_xinfo" => {
            let arg = pragma_arg(p.value.as_ref())?;
            let t = catalog.get_table(&arg)?;
            return Some(pragma_table_info(&t, name == "table_xinfo"));
        }
        "index_list" => {
            let arg = pragma_arg(p.value.as_ref())?;
            let t = catalog.get_table(&arg)?;
            return Some(pragma_index_list(&t, catalog));
        }
        "index_info" | "index_xinfo" => {
            let arg = pragma_arg(p.value.as_ref())?;
            let idx = catalog.get_index(&arg)?;
            let t = catalog.get_table(&idx.table)?;
            return Some(pragma_index_info(&idx, &t, name == "index_xinfo"));
        }
        "foreign_key_list" => {
            let arg = pragma_arg(p.value.as_ref())?;
            let t = catalog.get_table(&arg)?;
            return Some(pragma_foreign_key_list(&t, catalog));
        }
        _ => {}
    }
    // Every other pragma WITH a value is a plain write form: no result
    // rows (the write itself runs through the execute path).
    if p.value.is_some() {
        return None;
    }
    // integrity_check / quick_check: full structural walk of every b-tree
    // (see src/storage/integrity.rs). Runs against the flushed on-disk
    // state so the reported file shape is real.
    if name == "integrity_check" || name == "quick_check" {
        // Flush pending dirty pages first: the check validates the file,
        // and the session's live roots must match what's persisted.
        if db.pager.has_dirty_pages() {
            let _ = db.pager.flush();
        }
        let (roots, index_roots) = {
            let maps = db.maps.read();
            let roots = maps.roots.clone();
            let index_roots = maps.index_roots.clone();
            (roots, index_roots)
        };
        // integrity_check returns one value per problem line ("ok" when
        // clean); SQLite surfaces each as its own single-column row.
        let problems = crate::storage::integrity::integrity_check(
            &db.catalog,
            &db.pager,
            &roots,
            &index_roots,
            name == "quick_check",
        );
        let rows: Vec<Vec<Value>> = problems.iter().cloned().map(|v| vec![v]).collect();
        return Some(PragmaRows {
            columns: vec!["integrity_check".into()],
            rows,
        });
    }
    let pager = &db.pager;
    let v = match name.as_str() {
        "foreign_keys" => Value::Integer(if pager.foreign_keys_enabled() { 1 } else { 0 }),
        "page_size" => Value::Integer(pager.page_size() as i64),
        "page_count" => Value::Integer(pager.n_pages() as i64),
        "cache_size" => Value::Integer(pager.cache_capacity() as i64),
        "schema_version" => Value::Integer(pager.schema_cookie() as i64),
        "journal_mode" => Value::Text(if pager.wal_enabled() {
            "wal".into()
        } else {
            "delete".into()
        }),
        "codec" => Value::Text(
            pager
                .codec_name()
                .or_else(|| pager.required_codec())
                .map(|s| s.into())
                .unwrap_or_else(|| "none".into()),
        ),
        "synchronous" => Value::Integer(pager.synchronous() as i64),
        "temp_store" => Value::Integer(0),
        "locking_mode" => Value::Text(if pager.locking_mode_exclusive() {
            "exclusive".into()
        } else {
            "normal".into()
        }),
        "user_version" => Value::Integer(0),
        "application_id" => Value::Integer(0),
        "auto_vacuum" => Value::Integer(0),
        "encoding" => Value::Text("UTF-8".into()),
        _ => return None,
    };
    Some(PragmaRows::single(&name, v))
}

/// Apply a `PRAGMA journal_mode = X` mode switch from the read/query path
/// (the pager methods take `&self` — interior mutability).
fn apply_journal_mode(db: &Database, mode: &str) -> Result<()> {
    match mode {
        "wal" => db.pager.enable_wal(),
        "delete" | "truncate" | "persist" | "memory" | "off" => db.pager.disable_wal(),
        _ => Ok(()),
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
        E::Column {
            table: Some(t),
            name,
        } => format!("{}.{}", t, name),
        E::Binary { op, left, right } => {
            use crate::sql::ast::BinaryOp::*;
            let sym = match op {
                Add => "+",
                Sub => "-",
                Mul => "*",
                Div => "/",
                Mod => "%",
                Concat => "||",
                BitAnd => "&",
                BitOr => "|",
                BitXor => "^",
                ShiftLeft => "<<",
                ShiftRight => ">>",
                Eq => "=",
                NotEq => "!=",
                Lt => "<",
                LtEq => "<=",
                Gt => ">",
                GtEq => ">=",
                And => "AND",
                Or => "OR",
            };
            format!("{} {} {}", expr_to_sql(left), sym, expr_to_sql(right))
        }
        E::Unary { op, expr } => {
            use crate::sql::ast::UnaryOp::*;
            let sym = match op {
                Neg => "-",
                Pos => "+",
                Not => "NOT ",
                BitNot => "~",
            };
            format!("{}{}", sym, expr_to_sql(expr))
        }
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
        return format!(
            "{}{}{}",
            &sql[..name_start],
            new_name,
            &sql[name_start + name_len..]
        );
    }
    let name_len = trimmed
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(trimmed.len());
    format!(
        "{}{}{}",
        &sql[..after_ws],
        new_name,
        &sql[after_ws + name_len..]
    )
}

/// Append `, <column-def>` to a CREATE TABLE statement's column list (just
/// before the closing paren).
fn rewrite_create_table_add_column(sql: &str, column: &crate::sql::ast::ColumnDef) -> String {
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
            References {
                table,
                columns,
                on_delete,
                on_update,
            } => {
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
            if let Some((kind, name, tbl_name, rootpage, sql)) =
                crate::schema::decode_schema_row(&row)
            {
                if kind == "trigger" && tbl_name.eq_ignore_ascii_case(old_table) {
                    let new_sql = rewrite_on_table(sql, old_table, new_table);
                    updates.push((
                        rowid,
                        crate::schema::encode_schema_row(
                            "trigger", name, new_table, rootpage, &new_sql,
                        ),
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
            if let Some((kind, name, tbl_name, rootpage, sql)) =
                crate::schema::decode_schema_row(&row)
            {
                if kind == "index" && tbl_name.eq_ignore_ascii_case(old_table) {
                    let new_sql = rewrite_on_table(sql, old_table, new_table);
                    updates.push((
                        rowid,
                        crate::schema::encode_schema_row(
                            "index", name, new_table, rootpage, &new_sql,
                        ),
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
    let rowid = crate::executor::next_auto_rowid(pager, 0, max_rowid)?;
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
    // sqlite_master / sqlite_schema: a queryable view over this very
    // B+tree (page 0). Registered FIRST so nothing shadows it.
    catalog.add_table(crate::schema::sqlite_master_table());
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
    // Index rows keyed by name — used by the table branch to recover the
    // rootpages of implicit auto-index rows (which carry NULL sql, like
    // SQLite — they are rebuilt from the TABLE's DDL, not parsed).
    let mut index_row_by_name: std::collections::HashMap<String, (u32, String)> =
        std::collections::HashMap::new();
    for row in entries {
        if let Some((kind, _name, tbl_name, rootpage, sql)) = crate::schema::decode_schema_row(&row)
        {
            if kind == "index" {
                index_row_by_name.insert(_name.to_string(), (rootpage, sql.to_string()));
            }
            let owned = (
                kind.to_string(),
                _name.to_string(),
                tbl_name.to_string(),
                rootpage,
                sql.to_string(),
            );
            if kind == "table" {
                tables_first.push(owned);
            } else {
                others.push(owned);
            }
        }
    }
    let ordered = tables_first.into_iter().chain(others);
    for (kind, _name, tbl_name, rootpage, sql) in ordered {
        let kind = kind.as_str();
        let sql = sql.as_str();
        let _ = &_name;
        let _ = &tbl_name;
        {
            match kind {
                "table" => {
                    if let Ok(Statement::Create(CreateStatement::Table {
                        name: tn,
                        columns,
                        constraints,
                        without_rowid,
                        strict,
                        ..
                    })) = parse(sql)
                    {
                        let table = build_table(
                            &tn.name,
                            &columns,
                            &constraints,
                            rootpage,
                            without_rowid,
                            strict,
                            sql,
                        )?;
                        catalog.add_table(table.clone());
                        // Implicit auto-indexes (sqlite_autoindex_*): their
                        // schema rows have NULL sql (SQLite-faithful), so
                        // reconstruct them from THIS DDL — column-level
                        // UNIQUE, then table-level UNIQUE, then non-alias
                        // PRIMARY KEY — matching CREATE-time order, and
                        // pair with the rows by name to recover rootpages.
                        // (Rows WITH sql text are handled by the "index"
                        // branch below — legacy files.)
                        rebuild_implicit_indexes(
                            &table,
                            &columns,
                            &constraints,
                            &index_row_by_name,
                            catalog,
                        );
                    } else if let Ok(Statement::Create(CreateStatement::VirtualTable {
                        name: tn,
                        module,
                        args,
                        ..
                    })) = parse(sql)
                    {
                        // Virtual table: the module isn't registered at
                        // open time — build a PENDING instance. The column
                        // list stays empty until `create_module` connects
                        // it (the planner rejects queries over pending
                        // vtabs with "no such module").
                        let mut table = crate::plugin::vtab::vtab_columns_to_schema(&tn.name, &[]);
                        table.create_sql = sql.to_string();
                        table.vtab = Some(std::sync::Arc::new(
                            crate::plugin::vtab::VtabInstance::pending(
                                tn.name.clone(),
                                module.clone(),
                                args.clone(),
                            ),
                        ));
                        catalog.add_table(table);
                    }
                }
                "index" => {
                    if let Ok(Statement::Create(CreateStatement::Index {
                        unique,
                        name: idx_name,
                        table,
                        columns,
                        where_clause,
                        ..
                    })) = parse(sql)
                    {
                        let table_obj = catalog.get_table(&table).ok_or_else(|| {
                            Error::corruption(format!(
                                "index {} references missing table {}",
                                idx_name, table
                            ))
                        })?;
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
                "view" => {
                    if let Ok(Statement::Create(CreateStatement::View {
                        name: vn,
                        columns,
                        select,
                        ..
                    })) = parse(sql)
                    {
                        catalog.add_view(crate::schema::View {
                            name: vn.name,
                            columns,
                            select: *select,
                            create_sql: sql.to_string(),
                        });
                    }
                }
                "trigger" => {
                    if let Ok(Statement::Create(CreateStatement::Trigger(t))) = parse(sql) {
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
                _ => {}
            }
            let _ = tbl_name;
        }
    }
    Ok(())
}

/// Rebuild the implicit `sqlite_autoindex_<table>_<n>` indexes for one
/// table during `load_schema`. Their schema rows carry NULL sql (SQLite
/// stores NULL for auto-indexes), so the TABLE's DDL is the source of
/// truth: column-level UNIQUE (in column order), then table-level
/// UNIQUE, then non-rowid-alias PRIMARY KEY — the exact collection order
/// `execute_create` uses, so the numbering matches. Rows carrying DDL
/// text (legacy files) are skipped here — the "index" branch parses them.
fn rebuild_implicit_indexes(
    table: &crate::schema::Table,
    columns: &[crate::sql::ast::ColumnDef],
    constraints: &[crate::sql::ast::TableConstraint],
    index_rows: &std::collections::HashMap<String, (u32, String)>,
    catalog: &mut Catalog,
) {
    let mut implicit: Vec<Vec<crate::sql::ast::IndexedColumn>> = Vec::new();
    for col in columns {
        if col
            .constraints
            .iter()
            .any(|c| matches!(c, crate::sql::ast::ColumnConstraint::Unique))
        {
            let collate = col.constraints.iter().find_map(|c| {
                if let crate::sql::ast::ColumnConstraint::Collate(name) = c {
                    Some(name.clone())
                } else {
                    None
                }
            });
            implicit.push(vec![crate::sql::ast::IndexedColumn {
                name: col.name.clone(),
                order: crate::sql::ast::Order::Asc,
                collation: collate,
            }]);
        }
    }
    for c in constraints {
        match c {
            crate::sql::ast::TableConstraint::Unique(cols) => {
                implicit.push(cols.clone());
            }
            crate::sql::ast::TableConstraint::PrimaryKey { columns: cols } => {
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
        let idx_name = format!("sqlite_autoindex_{}_{}", table.name, n + 1);
        let Some((root, sql_text)) = index_rows.get(&idx_name) else {
            continue;
        };
        if !sql_text.is_empty() {
            continue; // legacy row with DDL text — parsed by the index branch
        }
        let idx_columns = match crate::schema::build_index_columns(cols, table) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let col_list = cols
            .iter()
            .map(|ic| {
                let coll = match &ic.collation {
                    Some(c) => format!(" COLLATE \"{}\"", c),
                    None => String::new(),
                };
                format!("\"{}\"{}", ic.name, coll)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let idx_sql = format!(
            "CREATE UNIQUE INDEX \"{}\" ON \"{}\"({})",
            idx_name, table.name, col_list
        );
        catalog.add_index(crate::schema::Index {
            name: idx_name,
            table: table.name.clone(),
            columns: idx_columns,
            root_page: *root,
            unique: true,
            partial_expr: None,
            create_sql: idx_sql,
        });
    }
}

// Heuristic: does this SQL string start a DDL statement (CREATE/DROP/ALTER)?
// Used to invalidate the statement cache after schema changes. We only need
// a cheap prefix check — the parser is the source of truth, and the cache
// will be re-populated on the next call.
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
    if j < b.len()
        && (b[j].is_ascii_digit() || (b[j] == b'.' && j + 1 < b.len() && b[j + 1].is_ascii_digit()))
    {
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
        // String literal with '' escapes; UTF-8-safe. The common case (no
        // escaped quote inside) builds the Text value directly from the
        // SQL slice — one copy, zero heap allocation for short strings
        // (Text's small-string optimization stores up to 23 bytes inline).
        let mut k = i + 1;
        loop {
            if k >= b.len() {
                return None; // unterminated
            }
            if b[k] == b'\'' {
                if k + 1 < b.len() && b[k + 1] == b'\'' {
                    // Escaped quote inside — take the byte-collecting slow
                    // path below (it restarts from the literal start).
                    break;
                }
                let s = std::str::from_utf8(&b[i + 1..k]).ok()?;
                return Some((Value::Text(s.into()), k + 1));
            }
            k += 1;
        }
        // Slow path: at least one '' escape — byte-collect.
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
        Some((Value::Text(s.as_str().into()), k))
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
    Some(FastInsert {
        table,
        columns,
        values,
    })
}

// ============================================================================
// INSERT CHAIN — cross-statement single-row literal INSERT fast path
// ============================================================================

/// Cross-statement memo for consecutive single-row literal INSERTs into
/// the same table with the same column list.
///
/// The per-statement INSERT pipeline — scanner allocations, catalog lookup,
/// column resolution, `ExecContext` construction, maps detach/attach,
/// per-statement hash-map bookkeeping, page-cache hints that reset on every
/// statement boundary — costs ~1 µs of pure overhead, which dominates
/// single-row INSERT throughput. A chain keeps every derived fact (the
/// `Arc<Table>`, resolved column indices, affinities, the live root page,
/// the max-rowid cache, and the B+tree right-most-leaf append hint) alive
/// ACROSS statements, so the steady state per statement is:
///
/// ```text
/// scan SQL into the reused row buffer (zero allocations)
///   -> NOT NULL check -> payload encode into the reused buffer
///   -> B+tree append into the pinned leaf -> counters
/// ```
///
/// Correctness rests on two invariants:
///
/// 1. **Epoch gate** — `execute` bumps `write_epoch` unconditionally before
///    dispatch. The chain is valid on entry only when the current epoch is
///    exactly `chain.epoch + 1`, i.e. the ONLY statement between the chain's
///    last use and now is the current one. Any other statement (DML, DDL,
///    transaction control, `query`-path DML) breaks the chain first.
/// 2. **Flush on break** — while the chain is hot it is the sole owner of
///    the table's true root page and max-rowid; the shared bookkeeping maps
///    are stale. Before ANY general-path statement consults those maps, the
///    chain is flushed: root and max-rowid are written back (and the schema
///    row is rewritten if a split moved the root, so a reopened database
///    descends from the live root). The flush sites are `execute`'s general
///    path, `query`/`query_with_columns`, the prepared-statement path, and
///    `Database::flush` — every entry point that reads or mutates table
///    state. Because writers hold `&mut self` (the outer `RwLock` write
///    half) and readers hold the read half, no thread can observe the
///    maps mid-chain.
///
/// Only "plain" tables are eligible: rowid tables without virtual tables,
/// WITHOUT ROWID, STRICT, generated columns, CHECK constraints, INSERT
/// triggers, outgoing foreign keys, indexes, or column-subset inserts on
/// tables with DEFAULTs. Everything else falls back to the existing
/// (still fast) general path.
struct InsertChain {
    /// `write_epoch` after the statement that last used (or built) this
    /// chain. See the struct docs for the validity rule.
    epoch: u64,
    table: Arc<Table>,
    /// Lowercased table name — key form used by the maps flush.
    name_lc: Box<str>,
    /// Lowercased column names as resolved (`[]` = supplies-all shape).
    col_names: Vec<Box<str>>,
    /// Target column index per VALUES position, parallel to
    /// `col_names.len()` (or the column count for supplies-all).
    col_indices: Vec<usize>,
    /// Column affinity per VALUES position (parallel to `col_indices`).
    affinities: Vec<crate::types::Affinity>,
    /// Column indices that enforce NOT NULL.
    not_null: Vec<usize>,
    /// Rowid-alias column (INTEGER PRIMARY KEY), if any.
    rowid_alias: Option<usize>,
    /// Live root page of the table's B+tree (tracks splits).
    root: u32,
    /// Monotonic upper bound of rowids present in the table.
    max_rowid: i64,
    /// Cross-statement right-most-leaf append hint (page id). Validated on
    /// every use by the B+tree insert path; falls back automatically.
    leaf_hint: Option<u32>,
    /// Reusable full-width row buffer, NULL-reset per row.
    full_row: Vec<Value>,
    /// Reusable payload encode buffer.
    payload_buf: Vec<u8>,
}

/// Outcome of parsing a statement against an [`InsertChain`].
enum ChainParse {
    /// The statement's shape matched and its values were written into the
    /// chain's row buffer.
    Matched,
    /// Any deviation — different table, different column list, multi-row
    /// VALUES, explicit rowid, non-literal expression, trailing clauses.
    Mismatch,
}

/// Parse one single-row literal INSERT whose shape matches `ch`, writing
/// the affinity-coerced values directly into `ch.full_row` at their
/// resolved column positions. Zero heap allocations on the matched path
/// for short strings (Text small-string optimization).
fn parse_chain_row(sql: &str, ch: &mut InsertChain) -> ChainParse {
    let b = sql.as_bytes();
    let mut i = skip_ws(b, 0);
    // INSERT INTO
    match match_word_ci(b, i, "INSERT") {
        Some(j) => i = skip_ws(b, j),
        None => return ChainParse::Mismatch,
    }
    match match_word_ci(b, i, "INTO") {
        Some(j) => i = skip_ws(b, j),
        None => return ChainParse::Mismatch,
    }
    // Table name must match the chained table (case-insensitive).
    let (ts, te) = match read_ident(b, i) {
        Some(r) => r,
        None => return ChainParse::Mismatch,
    };
    if !ch.table.name.as_bytes().eq_ignore_ascii_case(&b[ts..te]) {
        return ChainParse::Mismatch;
    }
    i = skip_ws(b, te);
    let n_fields = ch.col_indices.len();
    // Column list: must match the chain's resolved list verbatim
    // (case-insensitive, same count, same order).
    if i < b.len() && b[i] == b'(' {
        if ch.col_names.is_empty() {
            // Chain is a supplies-all shape; a column list is a different
            // shape — let the cold path rebuild.
            return ChainParse::Mismatch;
        }
        i += 1;
        let mut k = 0usize;
        loop {
            i = skip_ws(b, i);
            let (cs, ce) = match read_ident(b, i) {
                Some(r) => r,
                None => return ChainParse::Mismatch,
            };
            if k >= ch.col_names.len()
                || !ch.col_names[k].as_bytes().eq_ignore_ascii_case(&b[cs..ce])
            {
                return ChainParse::Mismatch;
            }
            k += 1;
            i = skip_ws(b, ce);
            if i < b.len() && b[i] == b',' {
                i += 1;
                continue;
            }
            break;
        }
        if i >= b.len() || b[i] != b')' {
            return ChainParse::Mismatch;
        }
        i = skip_ws(b, i + 1);
    } else if !ch.col_names.is_empty() {
        // Statement supplies all columns; the chain was built for an
        // explicit column list.
        return ChainParse::Mismatch;
    }
    // VALUES ( <single row> )
    match match_word_ci(b, i, "VALUES") {
        Some(j) => i = skip_ws(b, j),
        None => return ChainParse::Mismatch,
    }
    if i >= b.len() || b[i] != b'(' {
        return ChainParse::Mismatch;
    }
    i += 1;
    // NULL-reset the reusable row buffer (releases nothing: Text values
    // up to 23 bytes are stored inline inside the Value).
    for v in ch.full_row.iter_mut() {
        *v = Value::Null;
    }
    let mut k = 0usize;
    loop {
        i = skip_ws(b, i);
        if k >= n_fields {
            // More values than the chained shape.
            return ChainParse::Mismatch;
        }
        let (v, ni) = match parse_fast_literal(b, i) {
            Some(r) => r,
            None => return ChainParse::Mismatch,
        };
        let target = ch.col_indices[k];
        if target == crate::executor::ROWID_COLUMN_SENTINEL {
            // Explicit `rowid` column on an alias-less table: needs the
            // executor's conflict-checking path.
            return ChainParse::Mismatch;
        }
        ch.full_row[target] = ch.affinities[k].coerce(v);
        k += 1;
        i = skip_ws(b, ni);
        if i < b.len() && b[i] == b',' {
            i += 1;
            continue;
        }
        break;
    }
    if k != n_fields {
        return ChainParse::Mismatch;
    }
    if i >= b.len() || b[i] != b')' {
        return ChainParse::Mismatch;
    }
    i = skip_ws(b, i + 1);
    // Multi-row VALUES (a comma follows the row) or any trailing clause
    // other than a single optional semicolon → cold path.
    if i < b.len() && b[i] != b';' {
        return ChainParse::Mismatch;
    }
    if i < b.len() && b[i] == b';' {
        i = skip_ws(b, i + 1);
    }
    if i != b.len() {
        return ChainParse::Mismatch;
    }
    // An explicit value for the rowid-alias column (e.g. `id` in
    // `INSERT INTO t (id, name) VALUES (5, 'x')`) needs the general path's
    // collision check; the chain only handles auto-generated rowids.
    if let Some(alias) = ch.rowid_alias {
        if !ch.full_row[alias].is_null() {
            return ChainParse::Mismatch;
        }
    }
    ChainParse::Matched
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
            if c.is_ascii_lowercase() {
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
    /// Borrow the parameters as a contiguous slice when possible (arrays,
    /// Vecs, unit). The pre-compiled fast paths use this to bind directly
    /// from the caller's storage instead of collecting a fresh `Vec` per
    /// query — one heap allocation + N moves saved on every call.
    fn as_slice(&self) -> Option<&[Value]> {
        None
    }
}

impl Params for () {
    type Iter = std::iter::Empty<Value>;
    fn into_iter(self) -> Self::Iter {
        std::iter::empty()
    }
    fn as_slice(&self) -> Option<&[Value]> {
        Some(&[])
    }
}

impl Params for Vec<Value> {
    type Iter = std::vec::IntoIter<Value>;
    fn into_iter(self) -> Self::Iter {
        <Vec<Value> as IntoIterator>::into_iter(self)
    }
    fn as_slice(&self) -> Option<&[Value]> {
        Some(self)
    }
}

/// Borrowed parameter slices (used by the sqlx driver and other callers
/// that already hold a `Vec<Value>`). Binding goes through the zero-copy
/// `as_slice` fast path; consuming paths clone.
impl<'a> Params for &'a [Value] {
    type Iter = std::iter::Cloned<std::slice::Iter<'a, Value>>;
    fn into_iter(self) -> Self::Iter {
        self.iter().cloned()
    }
    fn as_slice(&self) -> Option<&[Value]> {
        Some(self)
    }
}

/// Resolve one name from an INSERT column list to a column index.
/// `rowid` / `_rowid_` / `oid` are accepted as synonyms for the table's
/// INTEGER PRIMARY KEY alias column (SQLite semantics); a real column of
/// the same name always takes precedence.
fn resolve_insert_column(table: &crate::schema::Table, name: &str) -> Option<usize> {
    if let Some(idx) = table.find_column(name) {
        return Some(idx);
    }
    let is_rowid_name = name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid");
    if is_rowid_name {
        if let Some(alias_idx) = table.rowid_alias {
            return Some(alias_idx);
        }
        // No INTEGER PRIMARY KEY: `rowid` targets the rowid itself — the
        // executor routes the value to the B+tree key (sentinel index).
        return Some(crate::executor::ROWID_COLUMN_SENTINEL);
    }
    None
}

impl<const N: usize> Params for [Value; N] {
    type Iter = std::array::IntoIter<Value, N>;
    fn into_iter(self) -> Self::Iter {
        // Explicit IntoIterator call: a bare `self.into_iter()` here would
        // resolve to this very method (infinite recursion).
        IntoIterator::into_iter(self)
    }
    fn as_slice(&self) -> Option<&[Value]> {
        Some(self.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_wake_drained_after_bulk_txn() {
        // A bulk-write transaction followed by COMMIT must drain the
        // mimalloc deferred-free wake INSIDE the COMMIT (see
        // `maybe_drain_after_burst`) — the next read then pays no
        // post-storm recovery. Capture the pre-state so PARALLEL tests
        // that already settled the process flag don't mask the check.
        let was_settled_before = ALLOC_SETTLED.load(Ordering::Relaxed);
        let mut db = Database::open_in_memory().unwrap();
        db.set_deferred_flush(true);
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        let sql = "INSERT INTO t (name, val, score) VALUES (?, ?, ?)";
        db.execute("BEGIN", []).unwrap();
        // 5000 single-row statements ≈ 1.02M accounted blocks — past the
        // 400k ALLOC_WAKE_THRESHOLD, so the COMMIT-path drain must fire.
        for i in 1..=5000i64 {
            db.execute(
                sql,
                [
                    Value::Text(format!("name{}", i).into()),
                    Value::Integer(i * 2),
                    Value::Real(i as f64 * 1.5),
                ],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        if !was_settled_before {
            assert!(
                ALLOC_SETTLED.load(Ordering::Relaxed),
                "wake should be drained after a 5000-row txn COMMIT"
            );
        }
        // Data intact either way.
        let n = db.query("SELECT COUNT(*) FROM t", []).unwrap()[0][0].as_integer();
        assert_eq!(n, 5000);
    }

    #[test]
    fn test_rename_column_in_object_sql() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO t (v, log) VALUES (NEW.v * -1, 'negated'); END";
        let out = rename_column_in_object_sql(sql, "t", "v", "amount", true);
        assert!(out.contains("(amount, log)"), "out: {out}");
        assert!(out.contains("NEW.amount"), "out: {out}");
        let sql2 = "CREATE VIEW big AS SELECT id FROM t WHERE v > 1";
        let out2 = rename_column_in_object_sql(sql2, "t", "v", "val", true);
        assert!(out2.contains("val > 1"), "out2: {out2}");
    }

    fn memdb() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn create_insert_select() {
        let mut db = memdb();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO users (name) VALUES ('Alice')", [])
            .unwrap();
        db.execute("INSERT INTO users (name) VALUES ('Bob')", [])
            .unwrap();
        let rows = db.query("SELECT id, name FROM users", []).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Text("Alice".into()));
        assert_eq!(rows[1][1], Value::Text("Bob".into()));
    }

    #[test]
    fn update_and_delete() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", [])
            .unwrap();
        db.execute("UPDATE t SET x = x + 1", []).unwrap();
        let rows = db.query("SELECT x FROM t ORDER BY id", []).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(11)],
                vec![Value::Integer(21)],
                vec![Value::Integer(31)],
            ]
        );
        db.execute("DELETE FROM t WHERE x = 21", []).unwrap();
        let rows = db.query("SELECT x FROM t ORDER BY id", []).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn aggregate() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
        db.execute("INSERT INTO t (x) VALUES (1), (2), (3), (4), (5)", [])
            .unwrap();
        let rows = db
            .query("SELECT SUM(x), COUNT(*), MIN(x), MAX(x), AVG(x) FROM t", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(15));
        assert_eq!(rows[0][1], Value::Integer(5));
        assert_eq!(rows[0][2], Value::Integer(1));
        assert_eq!(rows[0][3], Value::Integer(5));
    }

    #[test]
    fn join() {
        let mut db = memdb();
        db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", [])
            .unwrap();
        db.execute(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO users (name) VALUES ('Alice'), ('Bob')", [])
            .unwrap();
        db.execute(
            "INSERT INTO orders (user_id, total) VALUES (1, 100), (1, 200), (2, 50)",
            [],
        )
        .unwrap();
        let rows = db
            .query(
                "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id",
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn group_by() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT, v INTEGER)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO t (k, v) VALUES ('a', 1), ('a', 2), ('b', 3), ('b', 4)",
            [],
        )
        .unwrap();
        let rows = db.query("SELECT k, SUM(v) FROM t GROUP BY k", []).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn reopen_persists() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut db = Database::open(tmp.path()).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", [])
                .unwrap();
            db.execute("INSERT INTO t (name) VALUES ('Alice')", [])
                .unwrap();
        }
        let db = Database::open(tmp.path()).unwrap();
        let rows = db.query("SELECT name FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Text("Alice".into()));
    }

    // ========================================================================
    // UPSERT / RETURNING / CHECK / NOT NULL / date-time integration tests
    // ========================================================================

    #[test]
    fn upsert_do_nothing() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT UNIQUE)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a')", []).unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 'b') ON CONFLICT (id) DO NOTHING",
            [],
        )
        .unwrap();
        let rows = db.query("SELECT id, val FROM t", []).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Text("a".into()));
    }

    #[test]
    fn upsert_do_update() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT, n INTEGER)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a', 10)", []).unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 'b', 5) ON CONFLICT (id) DO UPDATE SET n = n + excluded.n",
            [],
        )
        .unwrap();
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
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a@x.com', 'Alice')", [])
            .unwrap();
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
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)", [])
            .unwrap();
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
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
            [],
        )
        .unwrap();
        let err = db.execute(
            "INSERT INTO t VALUES (1, 1, 1) ON CONFLICT (a, b) DO NOTHING",
            [],
        );
        assert!(err.is_err());
    }

    #[test]
    fn insert_returning() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        let rows = db
            .query("INSERT INTO t (x) VALUES (10), (20) RETURNING id, x", [])
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[0][1], Value::Integer(10));
        assert_eq!(rows[1][0], Value::Integer(2));
        assert_eq!(rows[1][1], Value::Integer(20));
    }

    #[test]
    fn insert_returning_star() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        let rows = db
            .query("INSERT INTO t (x) VALUES (7) RETURNING *", [])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Integer(1), Value::Integer(7)]);
    }

    #[test]
    fn update_returning() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", [])
            .unwrap();
        let rows = db
            .query("UPDATE t SET x = x * 2 WHERE x > 15 RETURNING id, x", [])
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(2));
        assert_eq!(rows[0][1], Value::Integer(40));
        assert_eq!(rows[1][1], Value::Integer(60));
    }

    #[test]
    fn delete_returning() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", [])
            .unwrap();
        let rows = db
            .query("DELETE FROM t WHERE x <= 20 RETURNING x", [])
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(10));
        assert_eq!(rows[1][0], Value::Integer(20));
        let left = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(left[0][0], Value::Integer(1));
    }

    #[test]
    fn check_constraint_enforced() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0))",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO t (age) VALUES (25)", []).unwrap();
        let err = db.execute("INSERT INTO t (age) VALUES (-1)", []);
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("CHECK constraint failed"));
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
        db.execute("CREATE TABLE t (a INTEGER, b INTEGER, CHECK (a < b))", [])
            .unwrap();
        let ok = db.execute("INSERT INTO t VALUES (1, 2)", []);
        assert!(ok.is_ok());
        let err = db.execute("INSERT INTO t VALUES (2, 1)", []);
        assert!(err.is_err());
    }

    #[test]
    fn not_null_constraint_enforced() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            [],
        )
        .unwrap();
        let err = db.execute("INSERT INTO t (name) VALUES (NULL)", []);
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("NOT NULL constraint failed"));
        // UPDATE to NULL also fails.
        db.execute("INSERT INTO t (name) VALUES ('a')", []).unwrap();
        let err = db.execute("UPDATE t SET name = NULL", []);
        assert!(err.is_err());
    }

    #[test]
    fn datetime_functions_end_to_end() {
        let db = memdb();
        let rows = db
            .query(
                "SELECT date('2023-07-14'), datetime('2023-07-14 13:45:28'), time('23:59:59')",
                [],
            )
            .unwrap();
        assert_eq!(rows[0][0], Value::Text("2023-07-14".into()));
        assert_eq!(rows[0][1], Value::Text("2023-07-14 13:45:28".into()));
        assert_eq!(rows[0][2], Value::Text("23:59:59".into()));

        let rows = db
            .query(
                "SELECT julianday('1970-01-01'), unixepoch('2023-01-01 00:00:00')",
                [],
            )
            .unwrap();
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
        db.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, day TEXT)", [])
            .unwrap();
        db.execute(
            "INSERT INTO events (day) VALUES ('2023-01-01'), ('2023-06-15'), ('2024-03-20')",
            [],
        )
        .unwrap();
        let rows = db
            .query(
                "SELECT day FROM events WHERE day > date('2023-01-01', '+90 days') ORDER BY day",
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Text("2023-06-15".into()));
    }

    // ========================================================================
    // Subquery tests (scalar / IN / EXISTS)
    // ========================================================================

    #[test]
    fn scalar_subquery_in_select() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", [])
            .unwrap();
        let rows = db.query("SELECT (SELECT MAX(x) FROM t) AS m", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(30));
        // Scalar subquery in WHERE.
        let rows = db
            .query("SELECT x FROM t WHERE x > (SELECT AVG(x) FROM t)", [])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(30));
    }

    #[test]
    fn scalar_subquery_empty_returns_null() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        let rows = db
            .query("SELECT (SELECT x FROM t WHERE id = 99)", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Null);
    }

    #[test]
    fn in_subquery() {
        let mut db = memdb();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO a (v) VALUES (1), (2), (3), (4)", [])
            .unwrap();
        db.execute("INSERT INTO b (v) VALUES (2), (4), (6)", [])
            .unwrap();
        let rows = db
            .query(
                "SELECT v FROM a WHERE v IN (SELECT v FROM b) ORDER BY v",
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(2));
        assert_eq!(rows[1][0], Value::Integer(4));
        // NOT IN
        let rows = db
            .query(
                "SELECT v FROM a WHERE v NOT IN (SELECT v FROM b) ORDER BY v",
                [],
            )
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(1));
        assert_eq!(rows[1][0], Value::Integer(3));
    }

    #[test]
    fn exists_subquery() {
        let mut db = memdb();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
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
        let rows = db
            .query("SELECT v FROM a WHERE EXISTS (SELECT 1 FROM b)", [])
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn nested_subqueries() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (5), (15), (25)", [])
            .unwrap();
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
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO t (x) VALUES (10), (20), (30)", [])
            .unwrap();
        let rows = db
            .query(
                "SELECT x FROM t WHERE x > (SELECT AVG(x) FROM t WHERE x < ?)",
                vec![Value::Integer(30)],
            )
            .unwrap();
        // Subquery: AVG over rows where x < 30 → 15. Rows where x > 15: 20, 30.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Integer(20));
        assert_eq!(rows[1][0], Value::Integer(30));
    }

    #[test]
    fn correlated_subquery_executes_correctly() {
        let mut db = memdb();
        db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, v INTEGER)", [])
            .unwrap();
        db.execute("INSERT INTO a (v) VALUES (1), (2)", []).unwrap();
        db.execute("INSERT INTO b (v) VALUES (1), (1), (7)", [])
            .unwrap();
        // Correlated (a.v referenced inside subquery): for a.v=1, MAX(b.v)
        // over b.v=1 rows is 1 → matches. For a.v=2, no b rows → NULL → no
        // match. (Previously this shape errored 'unsupported'.)
        let result = db
            .query(
                "SELECT v FROM a WHERE v = (SELECT MAX(v) FROM b WHERE b.v = a.v)",
                [],
            )
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0][0], Value::Integer(1));
        // Empty outer match: b has no v=2 rows, so the scalar subquery is
        // NULL and the equality filters the row out (NULL = NULL is not true).
        let result2 = db
            .query(
                "SELECT v FROM a WHERE v = (SELECT MAX(v) FROM b WHERE b.v = 99)",
                [],
            )
            .unwrap();
        assert_eq!(result2.len(), 0);
    }

    // ========================================================================
    // IndexRange tests (range predicates on indexed columns)
    // ========================================================================

    #[test]
    fn index_range_scan_select() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
            .unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        db.execute(
            "INSERT INTO t (val) VALUES (3), (1), (4), (1), (5), (9), (2), (6)",
            [],
        )
        .unwrap();
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
        let rows = db
            .query("SELECT val FROM t WHERE val BETWEEN 2 AND 5", [])
            .unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![2, 3, 4, 5]);
        // Both bounds.
        let rows = db
            .query("SELECT val FROM t WHERE val > 1 AND val < 5", [])
            .unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![2, 3, 4]);
    }

    #[test]
    fn index_range_with_residual() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, cat TEXT)",
            [],
        )
        .unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        db.execute(
            "INSERT INTO t (val, cat) VALUES (1, 'a'), (2, 'b'), (3, 'a'), (4, 'b'), (5, 'a')",
            [],
        )
        .unwrap();
        // Range on val + residual on cat.
        let rows = db
            .query("SELECT val FROM t WHERE val > 1 AND cat = 'a'", [])
            .unwrap();
        let vals: Vec<i64> = rows.iter().map(|r| r[0].as_integer()).collect();
        assert_eq!(vals, vec![3, 5]);
    }

    #[test]
    fn index_range_update() {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
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
        db.execute("UPDATE t SET score = score + 1.0 WHERE val > 90", [])
            .unwrap();
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE score > 0.5", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(10));
        // Also verify all rows with val <= 90 still have score 0.
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE score = 0.0", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(90));
        // DELETE with a range predicate.
        db.execute("DELETE FROM t WHERE val >= 95", []).unwrap();
        let rows = db.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(rows[0][0], Value::Integer(94));
    }

    #[test]
    fn index_range_negative_and_real_values() {
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x REAL)", [])
            .unwrap();
        db.execute("CREATE INDEX idx_x ON t(x)", []).unwrap();
        db.execute(
            "INSERT INTO t (x) VALUES (-5.5), (-2.0), (0.0), (1.5), (3.25), (7.0)",
            [],
        )
        .unwrap();
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
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
                .unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 1..=10_000i64 {
                db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
                    .unwrap();
            }
            db.execute("COMMIT", []).unwrap();
            db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
            let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
            assert_eq!(c[0][0], Value::Integer(10_000));
        }
        let db2 = Database::open(path).unwrap();
        let c = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(
            c[0][0],
            Value::Integer(10_000),
            "row count lost across reopen"
        );
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
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
            .unwrap();
        db.execute("CREATE INDEX idx_val ON t(val)", []).unwrap();
        for i in 1..=3_000i64 {
            db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i * 7)])
                .unwrap();
        }
        let mut missing = 0;
        for i in 1..=3_000i64 {
            let r = db
                .query("SELECT id FROM t WHERE val = ?", [Value::Integer(i * 7)])
                .unwrap();
            if r.len() != 1 {
                missing += 1;
            }
        }
        assert_eq!(missing, 0, "{} indexed rows unreachable", missing);
        // Range scans too.
        let r = db
            .query("SELECT COUNT(*) FROM t WHERE val > 14000", [])
            .unwrap();
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
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val INTEGER)", [])
                .unwrap();
            db.execute("INSERT INTO t (val) VALUES (1)", []).unwrap();
            db.execute("BEGIN", []).unwrap();
            for i in 2..=5_000i64 {
                db.execute("INSERT INTO t (val) VALUES (?)", [Value::Integer(i)])
                    .unwrap();
            }
            db.execute("ROLLBACK", []).unwrap();
            let c = db.query("SELECT COUNT(*) FROM t", []).unwrap();
            assert_eq!(c[0][0], Value::Integer(1));
        }
        let db2 = Database::open(path).unwrap();
        let c = db2.query("SELECT COUNT(*) FROM t", []).unwrap();
        assert_eq!(
            c[0][0],
            Value::Integer(1),
            "rollback leaked rows across reopen"
        );
    }

    // ---- RowidRange fast path -------------------------------------------
    // These all run through `FastPath::RowidRange` when the plan shape is
    // `RowidRange { start: Some, end: Some, residual: None }` — the
    // pipeline-skipping OLTP path added alongside the binary-search
    // range scan in the B-tree.

    fn range_db(n: i64) -> Database {
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        // 10k rows → a multi-level tree so the interior-node binary
        // search and early-stop logic are exercised, not just one leaf.
        for i in 1..=n {
            db.execute(
                "INSERT INTO t (name, val) VALUES (?, ?)",
                [
                    Value::Text(format!("row-{i}").into()),
                    Value::Integer(i * 7),
                ],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn rowid_range_fast_path_bare_star() {
        let db = range_db(10_000);
        // BETWEEN on the INTEGER PRIMARY KEY alias.
        let rows = db
            .query("SELECT * FROM t WHERE id BETWEEN 1000 AND 1009", [])
            .unwrap();
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0][0], Value::Integer(1000));
        assert_eq!(rows[9][0], Value::Integer(1009));
        assert_eq!(rows[0][1], Value::Text("row-1000".into()));
        // Projection form: only requested columns, in order.
        let rows = db
            .query("SELECT val, name FROM t WHERE id BETWEEN 1000 AND 1004", [])
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], Value::Integer(7000));
        assert_eq!(rows[0][1], Value::Text("row-1000".into()));
    }

    #[test]
    fn rowid_range_fast_path_conjunct_bounds() {
        let db = range_db(10_000);
        // >= / <= conjunct pair is the same plan shape as BETWEEN.
        let rows = db
            .query("SELECT id FROM t WHERE id >= 5000 AND id <= 5004", [])
            .unwrap();
        assert_eq!(rows.len(), 5);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r[0], Value::Integer(5000 + i as i64));
        }
        // > / < (exclusive) still routes through the general range plan —
        // verify row count and edges are right there too.
        let rows = db
            .query("SELECT id FROM t WHERE id > 9990 AND id < 9999", [])
            .unwrap();
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
        let rows = db
            .query("SELECT id FROM t WHERE id BETWEEN 500 AND 400", [])
            .unwrap();
        assert!(rows.is_empty());
        // Single-row range.
        let rows = db
            .query("SELECT id FROM t WHERE id BETWEEN 1 AND 1", [])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Integer(1));
        // Full-table range via rowid bounds.
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE id BETWEEN 1 AND 20000", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(10_000));
        // Bounds beyond both ends clamp correctly.
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE id BETWEEN -5 AND 50000", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(10_000));
        // Range entirely past the right edge: the early-stop must kick in
        // at the first leaf without walking every remaining leaf.
        let rows = db
            .query("SELECT id FROM t WHERE id BETWEEN 10001 AND 20000", [])
            .unwrap();
        assert!(rows.is_empty());
        // Range entirely before the left edge: the interior binary search
        // must skip to the first child, not panic.
        let rows = db
            .query("SELECT id FROM t WHERE id BETWEEN -100 AND -1", [])
            .unwrap();
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
            db.execute("DELETE FROM t WHERE id = ?", [Value::Integer(i)])
                .unwrap();
        }
        let rows = db
            .query("SELECT id FROM t WHERE id BETWEEN 99 AND 201", [])
            .unwrap();
        // Survivors: 99, 101, 103, ..., 199, 200, 201
        let expect: Vec<Vec<Value>> = (99..=201)
            .filter(|i| *i == 99 || *i >= 200 || i % 2 == 1)
            .map(|i| vec![Value::Integer(i)])
            .collect();
        assert_eq!(rows, expect);
        let rows = db
            .query("SELECT COUNT(*) FROM t WHERE id BETWEEN 1 AND 2000", [])
            .unwrap();
        assert_eq!(rows[0][0], Value::Integer(1_950)); // 2000 - 50 deleted
    }

    #[test]
    fn rowid_range_multi_page_spans() {
        // A range wide enough to cross many leaves AND interior nodes:
        // verifies the early-stop `Ok(false)` propagation never cuts the
        // walk short while rows remain inside the range.
        let db = range_db(10_000);
        let rows = db
            .query("SELECT id FROM t WHERE id BETWEEN 137 AND 9973", [])
            .unwrap();
        assert_eq!(rows.len() as i64, 9973 - 137 + 1);
        assert_eq!(rows[0][0], Value::Integer(137));
        let last = rows.last().unwrap();
        assert_eq!(last[0], Value::Integer(9973));
        // Descending ranges are not the fast path, but must agree.
        let rows2 = db
            .query(
                "SELECT id FROM t WHERE id BETWEEN 137 AND 9973 ORDER BY id DESC",
                [],
            )
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
        let rows = db
            .query("SELECT val, name, id FROM t WHERE id = 5", [])
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(35),
                Value::Text("row-5".into()),
                Value::Integer(5),
            ]]
        );
        // Rowid range, reordered projection.
        let rows = db
            .query("SELECT val, name FROM t WHERE id BETWEEN 5 AND 6", [])
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(35), Value::Text("row-5".into())],
                vec![Value::Integer(42), Value::Text("row-6".into())],
            ]
        );
        // Index point lookup, reordered projection.
        let rows = db
            .query("SELECT val, name, id FROM t WHERE val = 35", [])
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer(35),
                Value::Text("row-5".into()),
                Value::Integer(5),
            ]]
        );
        // Duplicate projections on both point shapes.
        let rows = db.query("SELECT val, val FROM t WHERE id = 5", []).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(35), Value::Integer(35)]]);
        let rows = db
            .query("SELECT name, name FROM t WHERE val = 35", [])
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Text("row-5".into()),
                Value::Text("row-5".into()),
            ]]
        );
        // Single-column reorder: projection picks the LAST column only.
        let rows = db.query("SELECT val FROM t WHERE id = 7", []).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(49)]]);
        // Mid-table reorder mixing alias, text and integer.
        let rows = db
            .query("SELECT name, id, val FROM t WHERE id BETWEEN 9 AND 9", [])
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Text("row-9".into()),
                Value::Integer(9),
                Value::Integer(63),
            ]]
        );
    }

    #[test]
    fn rowid_reuse_after_delete_of_max() {
        // Regression: the shared max-rowid cache survived DELETE (the merge
        // is extend-only, so removals never propagated), making the next
        // INSERT skip rowids — `DELETE row 2` then INSERT returned 3, not
        // SQLite's max(existing)+1 = 2.
        let mut db = memdb();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        db.execute("INSERT INTO t (v) VALUES ('a')", []).unwrap();
        db.execute("INSERT INTO t (v) VALUES ('b')", []).unwrap();
        // Filtered delete of the max rowid (streaming path).
        db.execute("DELETE FROM t WHERE v = 'b'", []).unwrap();
        db.execute("INSERT INTO t (v) VALUES ('c')", []).unwrap();
        let rows = db.query("SELECT id, v FROM t ORDER BY id", []).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Text("a".into())],
                vec![Value::Integer(2), Value::Text("c".into())],
            ],
            "INSERT must reuse max(existing)+1 = 2"
        );
        // Delete-all (Scan path) then insert: rowids restart at 1.
        db.execute("DELETE FROM t", []).unwrap();
        db.execute("INSERT INTO t (v) VALUES ('d')", []).unwrap();
        let rows = db.query("SELECT id, v FROM t", []).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1), Value::Text("d".into())]]);
        // Point delete of the max (fast path) then insert.
        db.execute("DELETE FROM t WHERE id = 1", []).unwrap();
        db.execute("INSERT INTO t (v) VALUES ('e')", []).unwrap();
        let rows = db.query("SELECT id FROM t", []).unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn index_count_fast_path_shapes() {
        // `SELECT COUNT(*) FROM t WHERE indexed_col = ?` runs a dedicated
        // covering-index fast path (index probe only, no table fetch).
        // Exercise literal keys, param keys, misses, aliases, and the
        // shapes that must NOT take it (COUNT(col), GROUP BY).
        let mut db = memdb();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, cat TEXT, val INTEGER)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        for i in 1..=1000i64 {
            let cat = if i % 3 == 0 {
                "a"
            } else if i % 3 == 1 {
                "b"
            } else {
                "c"
            };
            db.execute(
                "INSERT INTO t (cat, val) VALUES (?, ?)",
                [Value::Text(cat.into()), Value::Integer(i)],
            )
            .unwrap();
        }
        db.execute("COMMIT", []).unwrap();
        db.execute("CREATE INDEX idx_cat ON t(cat)", []).unwrap();
        // Literal key.
        let r = db
            .query("SELECT COUNT(*) FROM t WHERE cat = 'a'", [])
            .unwrap();
        assert_eq!(r, vec![vec![Value::Integer(333)]]);
        // Parameter key (re-encoded per call).
        let r = db
            .query(
                "SELECT COUNT(*) FROM t WHERE cat = ?",
                [Value::Text("b".into())],
            )
            .unwrap();
        assert_eq!(r, vec![vec![Value::Integer(334)]]);
        // Miss.
        let r = db
            .query("SELECT COUNT(*) FROM t WHERE cat = 'zzz'", [])
            .unwrap();
        assert_eq!(r, vec![vec![Value::Integer(0)]]);
        // Aliased.
        let r = db
            .query("SELECT COUNT(*) AS n FROM t WHERE cat = 'a'", [])
            .unwrap();
        assert_eq!(r, vec![vec![Value::Integer(333)]]);
        // Column names through query_with_columns.
        let (cols, rows) = db
            .query_with_columns("SELECT COUNT(*) FROM t WHERE cat = 'c'", [])
            .unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(333)]]);
        assert_eq!(cols.len(), 1);
        // COUNT(col) is NOT the fast path but must stay correct.
        let r = db
            .query("SELECT COUNT(val) FROM t WHERE cat = 'c'", [])
            .unwrap();
        assert_eq!(r, vec![vec![Value::Integer(333)]]);
        // GROUP BY multi-bucket stays correct.
        let r = db
            .query("SELECT cat, COUNT(*) FROM t GROUP BY cat ORDER BY cat", [])
            .unwrap();
        assert_eq!(
            r,
            vec![
                vec![Value::Text("a".into()), Value::Integer(333)],
                vec![Value::Text("b".into()), Value::Integer(334)],
                vec![Value::Text("c".into()), Value::Integer(333)],
            ]
        );
        // Indexed point lookups still correct alongside (pre-encoded
        // literal keys path).
        let r = db
            .query("SELECT id FROM t WHERE cat = 'a' AND val = 999", [])
            .unwrap();
        assert_eq!(r, vec![vec![Value::Integer(999)]]);
    }
}
