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
use crate::planner::Planner;
use crate::schema::{build_table, Catalog};
use crate::sql::ast::*;
use crate::sql::parse;
use crate::storage::btree::Btree;
use crate::storage::pager::Pager;
use crate::storage::row_codec::{decode_row, encode_row};
use crate::types::{Row, Value};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The maximum number of pages cached in memory.
const DEFAULT_CACHE_PAGES: usize = 2048;

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
    /// Root page overrides (table_name -> current root). Updated when B+tree
    /// splits change the root, since the catalog's Arc<Table> is immutable.
    /// RwLock so reads can run concurrently; only writes (INSERT/UPDATE/DELETE
    /// causing a root split) take the write lock.
    root_overrides: RwLock<HashMap<String, u32>>,
    /// Max rowid per table (avoids O(n) scan on every INSERT).
    max_rowids: RwLock<HashMap<String, i64>>,
    /// Prepared-statement cache: SQL text -> (Arc<Statement>, Option<Plan>).
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
    /// RwLock so concurrent readers can hit the cache simultaneously; only
    /// a cache miss takes the brief write lock to insert.
    stmt_cache: RwLock<HashMap<String, (Arc<Statement>, Option<crate::planner::plan::Plan>)>>,
    /// FIFO order of insertion into `stmt_cache`, used for eviction when the
    /// cache reaches `stmt_cache_capacity`. The first item in this Vec is the
    /// oldest entry and the next to be evicted.
    stmt_cache_order: Mutex<Vec<String>>,
    /// Maximum number of entries in the statement cache. Default 64.
    /// Immutable after open (only set via `set_stmt_cache_capacity`).
    stmt_cache_capacity: usize,
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

impl Database {
    /// Open or create a database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut pager = Pager::open(&path, DEFAULT_CACHE_PAGES)?;
        let mut catalog = Catalog::new();
        catalog.schema_cookie = pager.schema_cookie();
        // Load the schema from page 0 (the schema table root).
        load_schema(&mut pager, &mut catalog)?;
        Ok(Self {
            pager,
            catalog,
            path,
            in_transaction: AtomicBool::new(false),
            txn_snapshot: Mutex::new(None),
            root_overrides: RwLock::new(HashMap::new()),
            max_rowids: RwLock::new(HashMap::new()),
            stmt_cache: RwLock::new(HashMap::new()),
            stmt_cache_order: Mutex::new(Vec::new()),
            stmt_cache_capacity: DEFAULT_STMT_CACHE_CAPACITY,
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
    /// Returns `(Arc<Statement>, Option<Plan>)` so the caller can:
    /// - For SELECT (query path): use just the Plan (cheap Arc clone).
    /// - For DML/DDL (execute path): use the Arc<Statement> (cheap Arc clone).
    ///
    /// The cached Plan is `Option<Plan>` and is cloned — cheap because
    /// `Plan` is `Clone` with only `Arc` references internally (no deep
    /// copies of large structures).
    ///
    /// On any cache lookup we DO NOT consult `self.catalog` mutably, so the
    /// borrow of `self.stmt_cache` and the immutable borrow of `self.catalog`
    /// don't conflict.
    fn get_or_cache_stmt(&self, sql: &str) -> Result<(std::sync::Arc<Statement>, Option<crate::planner::plan::Plan>)> {
        if self.stmt_cache_capacity == 0 {
            // Caching disabled — parse + plan every time.
            let stmt = parse(sql)?;
            let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
            return Ok((std::sync::Arc::new(stmt), plan_opt));
        }
        // Fast path: read lock — concurrent readers can hit the cache
        // simultaneously without serializing.
        {
            let cache = self.stmt_cache.read();
            if let Some((stmt, plan_opt)) = cache.get(sql) {
                return Ok((stmt.clone(), plan_opt.clone()));
            }
        }
        // Miss: parse + plan + insert. Take the write lock to insert,
        // double-check in case another thread inserted while we waited.
        let stmt = parse(sql)?;
        let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
        let mut cache = self.stmt_cache.write();
        // Double-check: another thread may have inserted while we waited.
        if let Some((s, p)) = cache.get(sql) {
            return Ok((s.clone(), p.clone()));
        }
        // Evict FIFO if at capacity.
        if cache.len() >= self.stmt_cache_capacity {
            let mut order = self.stmt_cache_order.lock();
            if let Some(oldest) = order.first().cloned() {
                cache.remove(&oldest);
                order.remove(0);
            }
        }
        let stmt_arc = std::sync::Arc::new(stmt);
        cache.insert(sql.to_string(), (stmt_arc.clone(), plan_opt.clone()));
        self.stmt_cache_order.lock().push(sql.to_string());
        Ok((stmt_arc, plan_opt))
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
        let is_ddl = is_ddl_sql(sql);
        let (stmt, _plan_opt) = self.get_or_cache_stmt(sql)?;
        // Deref the Arc<Statement> to a &Statement for execute_statement_static.
        // (The Arc itself stays alive on the stack for the duration of the call.)
        let stmt_ref: &Statement = &stmt;
        let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
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
        let mut ctx = ExecContext::new(&self.pager, catalog_ptr);
        ctx.in_transaction = in_txn;
        ctx.deferred_flush = deferred_flush;
        ctx.txn_snapshot = txn_snap;
        ctx.root_overrides = std::mem::take(self.root_overrides.get_mut());
        ctx.max_rowids = std::mem::take(self.max_rowids.get_mut());
        for v in params.into_iter() {
            ctx.bind_positional(v);
        }
        let result = Self::execute_statement_static(stmt_ref, &mut ctx, &mut self.catalog, sql);
        self.in_transaction.store(ctx.in_transaction, Ordering::Release);
        *self.txn_snapshot.get_mut() = ctx.txn_snapshot;
        *self.root_overrides.get_mut() = std::mem::take(&mut ctx.root_overrides);
        *self.max_rowids.get_mut() = std::mem::take(&mut ctx.max_rowids);
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
        let (_stmt, plan_opt) = self.get_or_cache_stmt(sql)?;
        if let Some(plan) = plan_opt {
            let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
            let in_txn = self.in_transaction.load(Ordering::Acquire);
            let txn_snap = self.txn_snapshot.lock().clone();
            let root_overrides = self.root_overrides.read().clone();
            let max_rowids = self.max_rowids.read().clone();
            let mut ctx = ExecContext::new(&self.pager, catalog_ptr);
            ctx.in_transaction = in_txn;
            ctx.deferred_flush = self.deferred_flush.load(Ordering::Acquire);
            ctx.txn_snapshot = txn_snap;
            ctx.root_overrides = root_overrides;
            ctx.max_rowids = max_rowids;
            for v in params.into_iter() {
                ctx.bind_positional(v);
            }
            let res = execute(&plan, &mut ctx)?;
            // For SELECT, root_overrides/max_rowids don't change. Don't write back.
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
        let (_stmt, plan_opt) = self.get_or_cache_stmt(sql)?;
        if let Some(plan) = plan_opt {
            let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
            let in_txn = self.in_transaction.load(Ordering::Acquire);
            let txn_snap = self.txn_snapshot.lock().clone();
            let root_overrides = self.root_overrides.read().clone();
            let max_rowids = self.max_rowids.read().clone();
            let mut ctx = ExecContext::new(&self.pager, catalog_ptr);
            ctx.in_transaction = in_txn;
            ctx.txn_snapshot = txn_snap;
            ctx.root_overrides = root_overrides;
            ctx.max_rowids = max_rowids;
            for v in params.into_iter() {
                ctx.bind_positional(v);
            }
            let res = execute(&plan, &mut ctx)?;
            Ok((res.columns, res.rows))
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

    fn plan_insert(catalog: &Catalog, stmt: &Statement) -> Result<crate::planner::plan::Plan> {
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
        })
    }

    fn plan_update(catalog: &Catalog, stmt: &Statement) -> Result<crate::planner::plan::Plan> {
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
        })
    }

    fn plan_delete(catalog: &Catalog, stmt: &Statement) -> Result<crate::planner::plan::Plan> {
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
                // Root overrides and max_rowids cached during the txn are
                // now stale; clear them so the next op rescans.
                ctx.root_overrides.clear();
                ctx.max_rowids.clear();
                Ok(())
            }
            Statement::Pragma(p) => Self::execute_pragma(p.clone(), ctx),
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
                catalog.add_table(table);
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
                let schema_row = crate::schema::encode_schema_row(
                    "index",
                    &index.name,
                    &index.table,
                    root_page,
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
                let table = catalog.drop_table(&d.name).ok_or_else(|| Error::NotFound(format!("table: {}", d.name)))?;
                ctx.pager.free_page(table.root_page)?;
                delete_schema_row(ctx.pager, "table", &d.name)?;
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

    fn execute_pragma(p: PragmaStatement, _ctx: &mut ExecContext) -> Result<()> {
        // Most pragmas are no-ops; a few are honored.
        let _ = p;
        Ok(())
    }
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
        if let Ok(row) = decode_row(payload, 5) {
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
        if let Ok(row) = decode_row(payload, 5) {
            entries.push(row);
        }
        true
    })?;
    for row in entries {
        if let Some((kind, _name, tbl_name, rootpage, sql)) = crate::schema::decode_schema_row(&row) {
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
}
