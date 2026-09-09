//! SQLite-style prepared statements: `prepare` / `bind` / `step` /
//! `reset` / `finalize`.
//!
//! This is the `sqlite3_prepare_v2` + `sqlite3_step` model on top of the
//! engine: a statement is parsed and planned ONCE, parameters are bound
//! any number of times, and rows arrive ONE AT A TIME (batches of 64
//! internally) without materializing the whole result set.
//!
//! ```no_run
//! use rustqlite::{Database, Value, StepResult};
//!
//! let mut db = Database::open_in_memory().unwrap();
//! db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT)", []).unwrap();
//! db.execute("INSERT INTO t (x) VALUES ('a'), ('b'), ('c')", []).unwrap();
//!
//! let mut stmt = db.prepare("SELECT id, x FROM t WHERE id >= ? ORDER BY id").unwrap();
//! stmt.bind(1, Value::Integer(2)); // 1-based, like sqlite3_bind_*
//! while stmt.step().unwrap() == StepResult::Row {
//!     println!("{} {}", stmt.column_int(0), stmt.column_text(1).unwrap());
//! }
//! ```
//!
//! # Streaming shapes
//!
//! The executor is collect-all; this layer adds **resumable drivers** for
//! the OLTP core plans, so those stream in batches with early termination
//! and never build a `Vec<Row>` of the full result:
//!
//! - bare `Scan` (+ pushed predicate, incl. virtual tables)
//! - `RowidRange`
//! - `Filter` / `Project` / `Limit` over any of the above
//!
//! Everything else (aggregates, joins, sorts, set ops, CTEs, index
//! lookups) executes once and then serves rows from the materialized
//! result — still no re-parse, no re-plan, and `reset()` re-executes with
//! the same bound parameters.
//!
//! # Transaction / DDL statements
//!
//! `prepare` accepts row-producing and DML statements. Transaction control
//! (`BEGIN` / `COMMIT` / ...), DDL and `ATTACH`/`VACUUM` are rejected with
//! a pointer to [`Database::execute`] — they need the mutable path.

use crate::api::{Database, FastPath};
use crate::error::{Error, Result};
use crate::executor::{
    bare_column_projection, execute, scan_groupby_grouper, trivial_group_projection, ExecContext,
    GroupProjection,
};
use crate::planner::plan::Plan;
use crate::sql::ast::{Expr, Statement as AstStatement};
use crate::types::{Row, Value};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

/// Batch size for streaming drivers.
const BATCH: usize = 64;
/// Sequential scan drivers' ROW-SERVING pull width: each `next_batch`
/// pays a root descent + resume binary search + ExecContext rebuild, so
/// the top-level `step()` loop pulls this many rows per batch and serves
/// them one at a time from its pending queue — 64-row batches paid the
/// per-batch fixed cost ~16x more often. Widening happens ONLY at the
/// top serving loop: the driver contract "next_batch returns AT MOST
/// `budget` rows" stays intact for the LIMIT/OFFSET and Filter wrappers,
/// whose row accounting depends on it. The MAX_BATCH_BYTES live-
/// footprint cap still bounds wide rows inside each driver.
const SCAN_BATCH_ROWS: usize = 1024;

/// Cap on the heap bytes materialized into ONE streaming batch (see
/// `ProjectedScanDriver::next_batch`): keeps wide-row statements at a
/// bounded live footprint instead of BATCH x row-size.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Approximate heap bytes owned by a materialized row (blob/text bodies).
#[inline]
fn row_heap_bytes(row: &[Value]) -> usize {
    let mut n = 0usize;
    for v in row {
        n += match v {
            Value::Blob(b) => b.len(),
            Value::Text(t) => t.len(),
            _ => 0,
        };
    }
    n
}

/// The result of [`Statement::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepResult {
    /// A row is available via the `column_*` accessors.
    Row,
    /// The statement is finished (matches SQLITE_DONE).
    Done,
}

/// A prepared statement bound to a database handle.
///
/// Statements are NOT `Send`: they borrow the [`Database`] (SQLite has the
/// same rule per connection; share the `Database` via `Arc` and prepare
/// per thread).
pub struct Statement<'a> {
    db: &'a Database,
    sql: String,
    stmt: Arc<AstStatement>,
    /// The cached plan, refcounted — prepare and every start() clone an
    /// Arc (one atomic) instead of deep-cloning the plan tree (5-10
    /// allocations for a filter/limit stack; the sqlx driver's
    /// fetch_optional path prepared per query).
    plan: Option<Arc<Plan>>,
    has_subqueries: bool,
    fast_path: Option<Arc<FastPath>>,
    params: Vec<Value>,
    named: HashMap<String, Value>,
    /// Positional parameter count discovered at prepare time.
    param_count: usize,
    /// Named parameter names discovered at prepare time (bare, no sigil).
    named_param_names: Vec<String>,
    /// Streaming state.
    stream: StreamState,
    /// Buffered rows from the last driver batch.
    pending: VecDeque<Row>,
    /// Recycled row buffers for the streaming drivers: rows served to
    /// the consumer come back here (capacity retained) and are popped
    /// again by the drivers' next_batch — one heap allocation per row
    /// SLOT for the statement's lifetime instead of one per ROW. The
    /// borrow discipline of `row()`/`column_value()` (references die
    /// before the next `step()`) makes recycling sound; `take_row()`
    /// simply removes a row from circulation.
    row_pool: Vec<Row>,
    columns: Option<Arc<[String]>>,
    current_row: Option<Row>,
    done: bool,
    /// Post-execution map deltas (DML merge-back, mirrors query()).
    deltas: CtxDeltas,
    changes_at_start: i64,
    /// Committed-view scope armed by `start()` for SELECT statements
    /// that run while a foreign write transaction is open (WAL-grade
    /// reads: the pager serves BEGIN-time pages, `read_maps` serves the
    /// BEGIN-time bookkeeping). Held for the statement's lifetime so the
    /// streaming drivers' `next_batch` calls stay inside the scope.
    view_guard: Option<crate::storage::pager::CommittedViewGuard<'a>>,
}

#[derive(Default)]
struct CtxDeltas {
    /// Table name → new root page after a B+tree split (statement DML
    /// merge-back). Previously only max-rowids were merged back, so the
    /// first split's new root was dropped and every subsequent insert/read
    /// went through the STALE root — a 5000-row insert silently retained
    /// ~391 rows (one leaf's worth).
    root_overrides: HashMap<String, u32>,
    index_roots: HashMap<String, u32>,
    roots_changed: bool,
    index_roots_changed: bool,
    max_rowids: HashMap<String, i64>,
    max_rowids_invalidated: Vec<String>,
    max_rowids_changed: bool,
}

enum StreamState {
    Fresh,
    Driver(Box<dyn Driver>),
    Materialized(std::vec::IntoIter<Row>),
    Exhausted,
}

impl<'a> Statement<'a> {
    pub(crate) fn new(db: &'a Database, sql: &str) -> Result<Self> {
        let cached = db.get_or_cache_stmt(sql)?;
        let stmt = Arc::clone(&cached.stmt);
        // Statements that need the mutable / static path.
        match stmt.as_ref() {
            AstStatement::Begin(_)
            | AstStatement::Commit
            | AstStatement::Rollback(_)
            | AstStatement::Savepoint(_)
            | AstStatement::Release(_)
            | AstStatement::Attach(_)
            | AstStatement::Detach(_)
            | AstStatement::Vacuum(_)
            | AstStatement::Alter(_)
            | AstStatement::Create(_)
            | AstStatement::Drop(_) => {
                return Err(Error::Unsupported(
                    "transaction / DDL statements must use Database::execute",
                ));
            }
            _ => {}
        }
        // Parameter discovery.
        let mut param_count = 0usize;
        let mut named_param_names: Vec<String> = Vec::new();
        collect_parameters(stmt.as_ref(), &mut param_count, &mut named_param_names);
        let plan = cached.plan.clone();
        // Output columns at PREPARE time when a streaming driver covers
        // the plan (SQLite's column_count/column_name work before the
        // first step). Materialized shapes set columns on first step.
        // Subquery-bearing plans never take the driver path (see start()),
        // so they don't advertise driver columns here either.
        let prep_columns = if cached.has_subqueries {
            None
        } else {
            plan.as_deref()
                .and_then(try_build_driver)
                .map(|d| d.columns())
        };
        Ok(Self {
            db,
            sql: sql.to_string(),
            fast_path: cached.fast_path.clone(),
            has_subqueries: cached.has_subqueries,
            stmt,
            plan,
            params: vec![Value::Null; param_count],
            named: HashMap::new(),
            param_count,
            named_param_names,
            stream: StreamState::Fresh,
            pending: VecDeque::new(),
            row_pool: Vec::new(),
            columns: prep_columns,
            current_row: None,
            done: false,
            deltas: CtxDeltas::default(),
            changes_at_start: db.total_changes(),
            view_guard: None,
        })
    }

    /// The statement's SQL text.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Number of positional parameters (`?`). Named parameters are extra.
    pub fn parameter_count(&self) -> usize {
        self.param_count
    }

    /// Names of the named parameters in order of first appearance, in
    /// their ORIGINAL spelling (with sigil: `:name`, `@name`, `$name`).
    pub fn parameter_names(&self) -> &[String] {
        &self.named_param_names
    }

    /// Bind a positional parameter. `idx` is **1-based** (the SQLite C API
    /// convention; `?1` is the first parameter).
    pub fn bind(&mut self, idx: usize, value: Value) -> Result<()> {
        if idx == 0 || idx > self.params.len() {
            return Err(Error::semantic(format!(
                "parameter index {} out of range (1..={})",
                idx,
                self.params.len()
            )));
        }
        self.params[idx - 1] = value;
        Ok(())
    }

    /// Bind a named parameter. The name matches with or without the
    /// leading sigil (`:name`, `@name`, `$name`); the value is stored
    /// under the parameter's ORIGINAL spelling (the engine's lookup key).
    pub fn bind_named(&mut self, name: &str, value: Value) -> Result<()> {
        let bare = name
            .trim_start_matches([':', '@', '$'])
            .to_ascii_lowercase();
        let original = self
            .named_param_names
            .iter()
            .find(|n| {
                n.trim_start_matches([':', '@', '$'])
                    .eq_ignore_ascii_case(&bare)
            })
            .cloned()
            .ok_or_else(|| Error::semantic(format!("no such parameter: {}", name)))?;
        self.named.insert(original, value);
        Ok(())
    }

    /// Bind all positional parameters from a slice (1-based order).
    pub fn bind_all(&mut self, values: &[Value]) -> Result<()> {
        for (i, v) in values.iter().enumerate() {
            self.bind(i + 1, v.clone())?;
        }
        Ok(())
    }

    /// Reset for re-execution with the CURRENT bindings (SQLite's
    /// `sqlite3_reset`; see also [`Self::clear_bindings`]).
    pub fn reset(&mut self) {
        self.stream = StreamState::Fresh;
        for mut row in self.pending.drain(..) {
            row.clear();
            self.row_pool.push(row);
        }
        if let Some(mut r) = self.current_row.take() {
            r.clear();
            self.row_pool.push(r);
        }
        self.columns = None;
        self.done = false;
        self.deltas = CtxDeltas::default();
        self.changes_at_start = self.db.total_changes();
        // Drop the committed-view scope: a re-executed statement re-arms
        // (or not) against the CURRENT transaction state at its next
        // start().
        self.view_guard = None;
    }

    /// Clear all bound parameters (back to NULL).
    pub fn clear_bindings(&mut self) {
        for v in &mut self.params {
            *v = Value::Null;
        }
        self.named.clear();
    }

    /// Number of output columns (valid after the first step).
    pub fn column_count(&self) -> usize {
        self.columns.as_ref().map(|c| c.len()).unwrap_or(0)
    }

    /// Output column name (valid after the first step).
    pub fn column_name(&self, idx: usize) -> Option<&str> {
        self.columns.as_ref()?.get(idx).map(|s| s.as_str())
    }

    /// The current row (valid between a `Row` step and the next step).
    pub fn row(&self) -> Option<&Row> {
        self.current_row.as_ref()
    }

    /// TAKE the current row's ownership (the next `step()` sees None and
    /// serves fresh rows normally). For single-row consumers — the sqlx
    /// driver's `fetch_optional` — this replaces a full row clone with a
    /// move.
    pub fn take_row(&mut self) -> Option<Row> {
        self.current_row.take()
    }

    /// The current row's value at `idx`.
    pub fn column_value(&self, idx: usize) -> Option<&Value> {
        self.current_row.as_ref()?.get(idx)
    }

    pub fn column_int(&self, idx: usize) -> i64 {
        self.column_value(idx).map(|v| v.as_integer()).unwrap_or(0)
    }

    pub fn column_real(&self, idx: usize) -> f64 {
        self.column_value(idx).map(|v| v.as_real()).unwrap_or(0.0)
    }

    pub fn column_text(&self, idx: usize) -> Option<String> {
        self.column_value(idx).map(|v| v.as_text())
    }

    pub fn column_blob(&self, idx: usize) -> Option<Vec<u8>> {
        match self.column_value(idx) {
            Some(Value::Blob(b)) => Some(b.clone()),
            _ => None,
        }
    }

    /// Number of rows this statement changed (DML; valid after Done).
    pub fn changes(&self) -> i64 {
        self.db.total_changes() - self.changes_at_start
    }

    /// Produce the next row. Returns [`StepResult::Row`] while rows remain.
    pub fn step(&mut self) -> Result<StepResult> {
        if self.done {
            return Ok(StepResult::Done);
        }
        if matches!(self.stream, StreamState::Fresh) {
            // A hot INSERT chain owns the table's live root / max-rowid
            // while the shared maps hold stale values — break (flush) it
            // before `start()` snapshots the maps for this statement.
            self.db.break_insert_chain();
            // Per-connection snapshot for `last_insert_rowid()` (see
            // Database::execute).
            crate::executor::change_counters::note_conn_rowid(self.db.last_insert_rowid());
            self.start()?;
        }
        // Serve one buffered row. The PREVIOUS current_row's consumer
        // reference is dead by now (row()/column_value() borrow self),
        // so its buffer goes back to the pool for the next batch fill.
        if let Some(row) = self.pending.pop_front() {
            if let Some(mut prev) = self.current_row.take() {
                // Clear on return: the pool keeps the row's SLOT capacity
                // (the alloc the drivers would otherwise repeat per row)
                // but never its payload bytes — pooled blob/text bodies
                // would otherwise pin up to a full batch's worth of wide
                // rows in memory between batches.
                prev.clear();
                self.row_pool.push(prev);
            }
            self.current_row = Some(row);
            return Ok(StepResult::Row);
        }
        match &mut self.stream {
            StreamState::Driver(drv) => {
                let db = self.db;
                let params = self.params.clone();
                let named = self.named.clone();
                // Pull WIDE batches for sequential scans: the driver stack
                // clamps correctly (LimitDriver caps at limit_left,
                // FilterDriver banks leftovers), and each batch amortizes
                // the root descent + resume + ctx rebuild across 1024 rows
                // instead of 64. Wrapper drivers still receive exact
                // budgets from THEIR callers, so LIMIT/OFFSET accounting
                // is unaffected.
                let batch =
                    drv.next_batch(db, &params, &named, SCAN_BATCH_ROWS, &mut self.row_pool)?;
                if batch.is_empty() {
                    self.done = true;
                    self.stream = StreamState::Exhausted;
                    // The driver's scan pulled pages through the pager
                    // (each an Arc + 4 KB allocation freed on LRU
                    // eviction) — return them to the OS now that the
                    // statement is finished instead of retaining the
                    // wake for the rest of the process.
                    db.maybe_drain_read_burst();
                    return Ok(StepResult::Done);
                }
                self.columns.get_or_insert_with(|| drv.columns());
                self.pending.extend(batch);
                self.current_row = self.pending.pop_front();
                match self.current_row {
                    Some(_) => Ok(StepResult::Row),
                    None => Ok(StepResult::Done),
                }
            }
            StreamState::Materialized(iter) => match iter.next() {
                Some(row) => {
                    if let Some(mut prev) = self.current_row.take() {
                        prev.clear();
                        self.row_pool.push(prev);
                    }
                    self.current_row = Some(row);
                    Ok(StepResult::Row)
                }
                None => {
                    self.done = true;
                    self.stream = StreamState::Exhausted;
                    self.db.maybe_drain_read_burst();
                    Ok(StepResult::Done)
                }
            },
            StreamState::Fresh | StreamState::Exhausted => {
                self.done = true;
                Ok(StepResult::Done)
            }
        }
    }

    /// Execute to completion (DML convenience).
    pub fn raw_execute(&mut self) -> Result<()> {
        while self.step()? == StepResult::Row {}
        Ok(())
    }

    /// Collect every remaining row.
    pub fn query_all(&mut self) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        while self.step()? == StepResult::Row {
            if let Some(r) = &self.current_row {
                rows.push(r.clone());
            }
        }
        Ok(rows)
    }

    /// Finalize: drop all resources (statements also finalize on drop).
    pub fn finalize(self) -> Result<()> {
        Ok(())
    }

    // -----------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------

    /// Begin execution: choose the streaming driver or materialize.
    fn start(&mut self) -> Result<()> {
        // Deferred-flush consistency (same contract as Database::query).
        if self
            .db
            .deferred_flush
            .load(std::sync::atomic::Ordering::Acquire)
            && self.db.pager.has_dirty_pages()
        {
            let _ = self.db.pager.flush();
        }

        // Committed-view arming (SELECT statements only): while a
        // foreign write transaction is open on this database, the pager
        // serves BEGIN-time pages and `read_maps` the BEGIN-time
        // bookkeeping — SQLite-WAL reader semantics. DML statements must
        // keep reading the live view (read-your-own-writes / write
        // targeting), so they never arm.
        self.view_guard = self
            .db
            .committed_view_guard(matches!(self.stmt.as_ref(), AstStatement::Select(_)));

        // PRAGMA reads: single row for value pragmas, N rows for
        // table-valued pragmas (table_info etc.).
        if let AstStatement::Pragma(p) = self.stmt.as_ref() {
            if let Some(pr) = crate::api::read_pragma_public(p, self.db) {
                self.columns = Some(Arc::from(pr.columns));
                self.stream = StreamState::Materialized(pr.rows.into_iter());
                return Ok(());
            }
            self.stream = StreamState::Exhausted;
            return Ok(());
        }

        // EXPLAIN: plan rows, never execute.
        if let AstStatement::Explain(inner) = self.stmt.as_ref() {
            let plan = Database::plan_for_statement(&self.db.catalog, inner)?;
            let rows = match plan {
                Some(p) => crate::executor::explain::explain_plan_rows(&p),
                None => Vec::new(),
            };
            self.columns = Some(Arc::from(vec![
                "opcode".to_string(),
                "detail".to_string(),
                "extra".to_string(),
            ]));
            self.stream = StreamState::Materialized(rows.into_iter());
            return Ok(());
        }

        // WITH-clause SELECTs: re-materialize per execution.
        let cte_select: Option<crate::sql::ast::SelectStatement> = match self.stmt.as_ref() {
            AstStatement::Select(sel) if sel.with.is_some() => Some(sel.clone()),
            _ => None,
        };
        if let Some(sel) = cte_select {
            let db = self.db;
            let res = self.exec_with_ctx(|ctx| db.exec_select_with_ctes_stmt(ctx, &sel))?;
            self.columns = Some(res.columns.clone());
            self.stream = StreamState::Materialized(res.rows.into_iter());
            return Ok(());
        }

        // Streaming driver FIRST when one covers the plan: drivers
        // produce rows in BUDGET-SIZED batches (bounded live memory),
        // while the precompiled fast paths materialize the ENTIRE result
        // before the first step — for a 100k-row range consumed one row
        // at a time (S06), the fused ProjectedRangeDriver measured ~10%
        // faster and far less peak memory. Point/COUNT lookup plans have
        // no driver shape, so they still take their fast paths. Fast
        // paths are skipped anyway under committed reads.
        //
        // SUBQUERY-BEARING plans are EXCLUDED from both shortcuts: the
        // driver / fast-path paths evaluate plan expressions WITHOUT the
        // subquery rewrite (rewrite_plan_subqueries runs on the
        // materialized path only) and WITHOUT an installed CorrGuard —
        // an IN / scalar / EXISTS subquery inside a driver predicate
        // then fails to evaluate, and FilterDriver SWALLOWS the error
        // (`.unwrap_or(false)`), silently dropping every row. The
        // materialized path below rewrites uncorrelated subqueries into
        // literals and evaluates correlated ones with the guard
        // installed, which is the only correct route for them.
        if let Some(plan) = self.plan.clone() {
            if !self.has_subqueries {
                // Streaming driver?
                if let Some(drv) = try_build_driver(&plan) {
                    self.columns = Some(drv.columns());
                    self.stream = StreamState::Driver(drv);
                    return Ok(());
                }
                // Precompiled point/COUNT fast paths (plans with no driver
                // shape): a handful of rows. Committed-view reads take them
                // too — run_fast_path resolves BEGIN-time roots under an
                // armed scope (the guard above armed it for SELECTs).
                if self.fast_path.is_some() {
                    if let Some(fp) = self.fast_path.clone() {
                        let rows = self.db.run_fast_path_public(&fp, &self.params)?;
                        self.columns = Some(fp_output_columns(&fp));
                        self.stream = StreamState::Materialized(rows.into_iter());
                        return Ok(());
                    }
                }
            }
            // Materialized: DML (with merge-back) or general SELECT.
            // DML WITHOUT RETURNING emits the engine's internal change
            // count row (["inserted", N]) — step() must surface DONE, not
            // a phantom row (SQLite's sqlite3_step on INSERT returns
            // SQLITE_DONE). Rows are only served for RETURNING.
            let dml_has_returning = match self.stmt.as_ref() {
                AstStatement::Insert(i) => i.returning.is_some(),
                AstStatement::Update(u) => u.returning.is_some(),
                AstStatement::Delete(d) => d.returning.is_some(),
                _ => false,
            };
            let is_dml = matches!(
                plan.as_ref(),
                Plan::Insert { .. } | Plan::Update { .. } | Plan::Delete { .. }
            );
            let has_subq = self.has_subqueries;
            let plan2 = plan.clone();
            let res = self.exec_with_ctx(move |ctx| {
                let plan_local;
                let plan_ref: &Plan = if has_subq {
                    plan_local = crate::executor::rewrite_plan_subqueries(&plan2, ctx)?;
                    &plan_local
                } else {
                    &plan2
                };
                execute(plan_ref, ctx)
            })?;
            if is_dml {
                // DML through a prepared statement bypasses
                // Database::execute (and its write-epoch bump), so the
                // statement path must invalidate the memoized COUNT(*)
                // answers itself — see Database::write_epoch.
                self.db
                    .write_epoch
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
                self.merge_dml_maps();
                self.db.sync_schema_roots_public()?;
                // Auto-commit ledger: a DML statement stepped with no
                // transaction open was its own implicit transaction.
                if !self
                    .db
                    .in_transaction
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    self.db.pager.note_tx_autocommit();
                }
            } else if self.deltas.max_rowids_changed && !self.db.pager.committed_reads_armed() {
                // Committed-view reads never merge (see query()).
                self.merge_max_rowids();
            }
            if is_dml && !dml_has_returning {
                self.stream = StreamState::Exhausted;
                return Ok(());
            }
            self.columns = Some(res.columns.clone());
            self.stream = StreamState::Materialized(res.rows.into_iter());
            return Ok(());
        }
        self.stream = StreamState::Exhausted;
        Ok(())
    }

    /// Run a closure with a fresh reader ExecContext (guards installed),
    /// capturing map deltas for merge-back.
    fn exec_with_ctx<R>(&mut self, f: impl FnOnce(&mut ExecContext<'_>) -> Result<R>) -> Result<R> {
        let db = self.db;
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.read_maps();
        let in_txn = db.in_transaction.load(std::sync::atomic::Ordering::Acquire);
        let txn_snap = if in_txn {
            db.txn_snapshot.lock().clone()
        } else {
            None
        };
        let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        ctx.in_transaction = in_txn;
        ctx.deferred_flush = db.deferred_flush.load(std::sync::atomic::Ordering::Acquire);
        ctx.txn_snapshot = txn_snap;
        for v in self.params.iter() {
            ctx.bind_positional(v.clone());
        }
        for (k, v) in &self.named {
            ctx.bind(k, v.clone());
        }
        let _plugin_guard = db.plugin_scope();
        let _corr_guard = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        let out = f(&mut ctx);
        // sqlite3_last_insert_rowid / sqlite3_changes bookkeeping: DML
        // through the streaming-statement path must update the Database
        // and the change counters the same way Database::execute does.
        db.set_last_insert_rowid(ctx.last_insert_rowid);
        crate::executor::change_counters::record(ctx.changes);
        // Engine-wide aggregate for the stats endpoint (any-thread read).
        db.pager.note_rows_modified(ctx.changes);
        let out = out?;
        // Capture deltas (query()'s DML merge-back semantics).
        self.deltas = CtxDeltas {
            root_overrides: ctx.root_overrides.clone(),
            index_roots: ctx.index_roots.clone(),
            roots_changed: ctx.roots_changed,
            index_roots_changed: ctx.index_roots_changed,
            max_rowids: ctx.max_rowids.clone(),
            max_rowids_invalidated: ctx.max_rowids_invalidated.clone(),
            max_rowids_changed: ctx.max_rowids_changed || !ctx.max_rowids_invalidated.is_empty(),
        };
        Ok(out)
    }

    fn merge_dml_maps(&mut self) {
        // DML via the shared path: merge root/index/max-rowid deltas into
        // the Database's bookkeeping maps — ALL THREE map kinds, plus the
        // maps_populated fast-path flag (mirrors Database::query).
        if !(self.deltas.roots_changed
            || self.deltas.index_roots_changed
            || self.deltas.max_rowids_changed)
        {
            return;
        }
        let mut m = self.db.maps.write();
        let bk = Arc::make_mut(&mut *m);
        if self.deltas.roots_changed {
            bk.roots.extend(self.deltas.root_overrides.drain());
        }
        if self.deltas.index_roots_changed {
            bk.index_roots.extend(self.deltas.index_roots.drain());
        }
        if self.deltas.max_rowids_changed {
            bk.max_rowids.extend(self.deltas.max_rowids.drain());
            for k in self.deltas.max_rowids_invalidated.drain(..) {
                bk.max_rowids.remove(&k);
            }
        }
        let nonempty = !bk.roots.is_empty() || !bk.index_roots.is_empty();
        self.db
            .maps_populated
            .store(nonempty, std::sync::atomic::Ordering::Release);
    }

    fn merge_max_rowids(&mut self) {
        self.merge_dml_maps();
    }
}

// ---------------------------------------------------------------------------
// Parameter discovery
// ---------------------------------------------------------------------------

/// Walk a statement AST collecting the max `?` count and named parameters.
fn collect_parameters(stmt: &AstStatement, positional: &mut usize, named: &mut Vec<String>) {
    // Highest numeric parameter name seen. The COUNT is max index + 1
    // (numeric names are Vec indices — the lexer numbers anonymous `?`
    // as "0","1",...); 0 when no numeric name appears.
    let mut max_pos = 0usize;
    let mut saw_numeric = false;
    let mut counter = 0usize;

    fn walk_expr(
        e: &Expr,
        max_pos: &mut usize,
        saw_numeric: &mut bool,
        named: &mut Vec<String>,
        counter: &mut usize,
    ) {
        match e {
            Expr::Parameter(p) => {
                // Numeric names are Vec indices (the lexer numbers
                // anonymous `?` as "0","1",...). The parameter COUNT is
                // max index + 1 so every referenced slot exists.
                if let Ok(n) = p.parse::<usize>() {
                    *max_pos = (*max_pos).max(n);
                    *saw_numeric = true;
                    let _ = counter;
                } else {
                    let bare = p.trim_start_matches([':', '@', '$']).to_ascii_lowercase();
                    if !bare.is_empty()
                        && !named.iter().any(|n| {
                            n.trim_start_matches([':', '@', '$'])
                                .eq_ignore_ascii_case(&bare)
                        })
                    {
                        named.push(p.clone()); // keep the sigil form — that's the HashMap key
                    }
                }
            }
            Expr::Binary { left, right, .. } => {
                walk_expr(left, max_pos, saw_numeric, named, counter);
                walk_expr(right, max_pos, saw_numeric, named, counter);
            }
            Expr::Unary { expr, .. } => walk_expr(expr, max_pos, saw_numeric, named, counter),
            Expr::Between {
                expr, low, high, ..
            } => {
                walk_expr(expr, max_pos, saw_numeric, named, counter);
                walk_expr(low, max_pos, saw_numeric, named, counter);
                walk_expr(high, max_pos, saw_numeric, named, counter);
            }
            Expr::In { expr, source, .. } => {
                walk_expr(expr, max_pos, saw_numeric, named, counter);
                match source {
                    crate::sql::ast::InSource::List(items) => {
                        for i in items.iter() {
                            walk_expr(i, max_pos, saw_numeric, named, counter);
                        }
                    }
                    // Parameters inside `IN (SELECT … ? …)` belong to THIS
                    // statement (SQLite binds them here too).
                    crate::sql::ast::InSource::Subquery(s) => {
                        walk_select(s, max_pos, saw_numeric, named, counter)
                    }
                    crate::sql::ast::InSource::Table(_) => {}
                }
            }
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => {
                walk_expr(expr, max_pos, saw_numeric, named, counter);
                walk_expr(pattern, max_pos, saw_numeric, named, counter);
                if let Some(e) = escape {
                    walk_expr(e, max_pos, saw_numeric, named, counter);
                }
            }
            Expr::IsNull { expr, .. } => walk_expr(expr, max_pos, saw_numeric, named, counter),
            Expr::Is { left, right, .. } => {
                walk_expr(left, max_pos, saw_numeric, named, counter);
                walk_expr(right, max_pos, saw_numeric, named, counter);
            }
            Expr::Function { args, .. } => {
                for a in args {
                    walk_expr(a, max_pos, saw_numeric, named, counter);
                }
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(o) = operand {
                    walk_expr(o, max_pos, saw_numeric, named, counter);
                }
                for (w, t) in whens {
                    walk_expr(w, max_pos, saw_numeric, named, counter);
                    walk_expr(t, max_pos, saw_numeric, named, counter);
                }
                if let Some(e) = else_ {
                    walk_expr(e, max_pos, saw_numeric, named, counter);
                }
            }
            Expr::Row(items) => {
                for i in items {
                    walk_expr(i, max_pos, saw_numeric, named, counter);
                }
            }
            Expr::Cast { expr, .. } => walk_expr(expr, max_pos, saw_numeric, named, counter),
            Expr::Collate { expr, .. } => walk_expr(expr, max_pos, saw_numeric, named, counter),
            // Scalar subqueries / EXISTS: parameters inside them bind on
            // THIS statement (SQLite semantics), so they must count.
            Expr::Subquery(s) => walk_select(s, max_pos, saw_numeric, named, counter),
            Expr::Exists(s) => walk_select(s, max_pos, saw_numeric, named, counter),
            _ => {}
        }
    }
    fn walk_select(
        s: &crate::sql::ast::SelectStatement,
        max_pos: &mut usize,
        saw_numeric: &mut bool,
        named: &mut Vec<String>,
        counter: &mut usize,
    ) {
        // WITH clause: parameters inside CTE bodies bind on this statement.
        if let Some(w) = &s.with {
            for cte in &w.ctes {
                walk_select(&cte.select, max_pos, saw_numeric, named, counter);
            }
        }
        walk_body(&s.body, max_pos, saw_numeric, named, counter);
        for t in &s.order_by {
            walk_expr(&t.expr, max_pos, saw_numeric, named, counter);
        }
        if let Some(l) = &s.limit {
            walk_expr(l, max_pos, saw_numeric, named, counter);
        }
        if let Some(o) = &s.offset {
            walk_expr(o, max_pos, saw_numeric, named, counter);
        }
    }

    fn walk_body(
        b: &crate::sql::ast::SelectBody,
        max_pos: &mut usize,
        saw_numeric: &mut bool,
        named: &mut Vec<String>,
        counter: &mut usize,
    ) {
        use crate::sql::ast::SelectBody;
        match b {
            SelectBody::Simple(body) => {
                for rc in &body.columns {
                    if let crate::sql::ast::ResultColumn::Expr { expr, .. } = rc {
                        walk_expr(expr, max_pos, saw_numeric, named, counter);
                    }
                }
                if let Some(w) = &body.where_clause {
                    walk_expr(w, max_pos, saw_numeric, named, counter);
                }
                for e in &body.group_by {
                    walk_expr(e, max_pos, saw_numeric, named, counter);
                }
                if let Some(h) = &body.having {
                    walk_expr(h, max_pos, saw_numeric, named, counter);
                }
                if let Some(f) = &body.from {
                    walk_table(f, max_pos, saw_numeric, named, counter);
                }
            }
            SelectBody::Binary { left, right, .. } => {
                walk_body(left, max_pos, saw_numeric, named, counter);
                walk_body(right, max_pos, saw_numeric, named, counter);
            }
        }
    }

    fn walk_table(
        te: &crate::sql::ast::TableExpression,
        max_pos: &mut usize,
        saw_numeric: &mut bool,
        named: &mut Vec<String>,
        counter: &mut usize,
    ) {
        match te {
            crate::sql::ast::TableExpression::Subquery { select, .. } => {
                walk_select(select, max_pos, saw_numeric, named, counter)
            }
            crate::sql::ast::TableExpression::Function { args, .. } => {
                // Table-valued function arguments may be bound params
                // (`pragma_table_info(?)`, `json_each(?)`).
                for a in args {
                    walk_expr(a, max_pos, saw_numeric, named, counter);
                }
            }
            crate::sql::ast::TableExpression::Join {
                left,
                right,
                constraint,
                ..
            } => {
                walk_table(left, max_pos, saw_numeric, named, counter);
                walk_table(right, max_pos, saw_numeric, named, counter);
                if let crate::sql::ast::JoinConstraint::On(e) = constraint {
                    walk_expr(e, max_pos, saw_numeric, named, counter);
                }
            }
            _ => {}
        }
    }
    match stmt {
        AstStatement::Select(s) => {
            walk_select(s, &mut max_pos, &mut saw_numeric, named, &mut counter)
        }
        AstStatement::Insert(i) => {
            if let crate::sql::ast::InsertSource::Values(rows) = &i.source {
                for r in rows {
                    for e in r {
                        walk_expr(e, &mut max_pos, &mut saw_numeric, named, &mut counter);
                    }
                }
            } else if let crate::sql::ast::InsertSource::Select(s) = &i.source {
                walk_select(s, &mut max_pos, &mut saw_numeric, named, &mut counter);
            }
        }
        AstStatement::Update(u) => {
            for (_, e) in &u.set {
                walk_expr(e, &mut max_pos, &mut saw_numeric, named, &mut counter);
            }
            if let Some(w) = &u.where_clause {
                walk_expr(w, &mut max_pos, &mut saw_numeric, named, &mut counter);
            }
        }
        AstStatement::Delete(d) => {
            if let Some(w) = &d.where_clause {
                walk_expr(w, &mut max_pos, &mut saw_numeric, named, &mut counter);
            }
        }
        _ => {}
    }
    *positional = if saw_numeric { max_pos + 1 } else { 0 };
}

// ---------------------------------------------------------------------------
// Streaming drivers
// ---------------------------------------------------------------------------

/// A resumable row source. `next_batch` pulls up to `budget` MORE rows
/// (given the statement's parameters); an empty result means EOF.
trait Driver {
    fn columns(&self) -> Arc<[String]>;
    /// `pool` is the statement's recycled row buffers: drivers that
    /// materialize output rows POP cleared buffers from it (capacity
    /// retained, one allocation per slot for the statement's lifetime)
    /// and pass them through to their inner sources where applicable.
    /// The statement returns consumed rows to the pool between batches.
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>>;
}

/// Output column names for a scan driver (mirrors exec_scan).
fn scan_columns(table: &Arc<crate::schema::Table>, alias: Option<&str>) -> Arc<[String]> {
    let prefix = alias.unwrap_or(&table.name);
    if prefix == table.name {
        table.qualified_col_names.clone()
    } else {
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect::<Vec<String>>()
            .into()
    }
}

/// Scan columns PLUS the hidden rowid slot for no-alias tables (see
/// `crate::planner::HIDDEN_ROWID`): expression evaluation resolves
/// `rowid` through the trailing slot, star expansions filter it out.
fn scan_columns_with_rowid(
    table: &Arc<crate::schema::Table>,
    alias: Option<&str>,
) -> Arc<[String]> {
    if !crate::planner::wants_rowid_slot(table) {
        return scan_columns(table, alias);
    }
    let mut cols: Vec<String> = scan_columns(table, alias).iter().cloned().collect();
    cols.push(crate::planner::HIDDEN_ROWID.to_string());
    cols.into()
}

/// GROUP BY streaming driver: `Project(Aggregate(Scan | Filter(Scan)))`
/// with a bare-selection projection (the planner's rewritten shape of
/// `SELECT cat, COUNT(*) FROM t GROUP BY cat` — by far the most common
/// GROUP BY form). The aggregation runs ONCE (first batch) into the
/// owned HashGrouper — the necessary state — and groups are finalized in
/// budget-sized batches on every later `next_batch`. The 100k-group query
/// therefore never materializes a 100k-row result set: the previous
/// materialized path built the full `Vec<Row>` (plus the projected copy,
/// before the executor's fused Project-over-Aggregate) — on torture S03
/// that was ~10 MB of output rows stacked on the grouper's state, where
/// SQLite streams its sorted aggregation at flat memory.
struct GroupByDriver {
    /// The Aggregate's input, verbatim (Scan or Filter(Scan)).
    input: Box<Plan>,
    group_by: Vec<Expr>,
    aggregates: Vec<crate::planner::plan::AggExpr>,
    proj: GroupProjection,
    columns: Arc<[String]>,
    /// Built once (first batch) from the scan; consumed batch-at-a-time
    /// after that. The iterator itself carries the bounded-memory
    /// contract: RAM-only groupers walk first-seen order; spilled
    /// groupers stream the temp-store chunk merge — the driver never
    /// materializes more than one batch of output rows regardless of
    /// group cardinality.
    iter: Option<crate::executor::GroupIter>,
    finished: bool,
}

impl GroupByDriver {
    fn new(
        input: Box<Plan>,
        group_by: Vec<Expr>,
        aggregates: Vec<crate::planner::plan::AggExpr>,
        proj: GroupProjection,
    ) -> Self {
        let columns: Arc<[String]> = proj.names.clone().into();
        Self {
            input,
            group_by,
            aggregates,
            proj,
            columns,
            iter: None,
            finished: false,
        }
    }

    /// Extract (table, alias, filter) from the Aggregate's input when it
    /// is the scan-groupby shape `scan_groupby_grouper` handles.
    fn scan_input(
        plan: &Plan,
    ) -> Option<(
        std::sync::Arc<crate::schema::Table>,
        Option<String>,
        Option<Expr>,
    )> {
        match plan {
            Plan::Scan {
                table,
                alias,
                index: None,
                predicate: None,
            } if table.vtab.is_none() => Some((table.clone(), alias.clone(), None)),
            Plan::Filter { input, predicate } => {
                if let Plan::Scan {
                    table,
                    alias,
                    index: None,
                    predicate: None,
                } = input.as_ref()
                {
                    if table.vtab.is_none() {
                        return Some((table.clone(), alias.clone(), Some(predicate.clone())));
                    }
                }
                None
            }
            _ => None,
        }
    }
}

impl Driver for GroupByDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }

    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        _pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.finished {
            return Ok(Vec::new());
        }
        if self.iter.is_none() {
            let (table, alias, filter) = match Self::scan_input(&self.input) {
                Some(t) => t,
                None => {
                    return Err(Error::runtime(
                        "GroupByDriver: unsupported aggregate input shape",
                    ))
                }
            };
            let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
            let shared = db.read_maps();
            let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
            for p in params {
                ctx.bind_positional(p.clone());
            }
            ctx.named_params = named.clone();
            let _g = db.plugin_scope();
            let _c = crate::executor::CorrGuard::install(&mut ctx as *mut _);
            let grouper = scan_groupby_grouper(
                &mut ctx,
                table,
                alias,
                filter.as_ref(),
                &self.group_by,
                &self.aggregates,
            )?;
            self.iter = Some(grouper.into_group_iter());
        }
        let iter = match self.iter.as_mut() {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        let n_group = self.group_by.len();
        let mut out: Vec<Row> = Vec::with_capacity(budget.clamp(1, 4096));
        while out.len() < budget.max(1) {
            let (keys, states) = match iter.next_group() {
                Some(g) => g,
                None => {
                    self.finished = true;
                    self.iter = None; // fully drained — free the state
                    break;
                }
            };
            let mut row: Vec<Value> = match _pool.pop() {
                Some(mut r) => {
                    r.clear();
                    r
                }
                None => Vec::with_capacity(self.proj.src.len().max(1)),
            };
            for &s in self.proj.src.iter() {
                if s < n_group {
                    row.push(keys.get(s).cloned().unwrap_or(Value::Null));
                } else {
                    let i = s - n_group;
                    row.push(crate::executor::finalize_agg(
                        &states[i],
                        &self.aggregates[i].func,
                    ));
                }
            }
            out.push(row);
        }
        Ok(out)
    }
}

/// Try to build a streaming driver for a plan shape. `None` → materialize.
fn try_build_driver(plan: &Plan) -> Option<Box<dyn Driver>> {
    // GROUP BY streaming: Project(Aggregate(scan-ish)) with a bare
    // selection projection — groups finalize in batches from the owned
    // grouper (see GroupByDriver). Must be checked before the generic
    // Project/Filter arms below would decompose the shape.
    if let Plan::Project { input, columns } = plan {
        if let Plan::Aggregate {
            input: agg_input,
            group_by,
            aggregates,
        } = input.as_ref()
        {
            if !group_by.is_empty()
                && aggregates
                    .iter()
                    .all(|a| crate::plugin::lookup_aggregate(&a.func).is_none())
                && GroupByDriver::scan_input(agg_input).is_some()
            {
                if let Some(proj) = trivial_group_projection(columns, group_by, aggregates) {
                    return Some(Box::new(GroupByDriver::new(
                        agg_input.clone(),
                        group_by.clone(),
                        aggregates.clone(),
                        proj,
                    )));
                }
            }
        }
    }
    match plan {
        Plan::Scan {
            table,
            alias,
            index: None,
            predicate,
        } => {
            if table.vtab.is_some() {
                Some(Box::new(VtabDriver::new(table.clone(), alias.clone())))
            } else {
                Some(Box::new(ScanDriver::new(
                    table.clone(),
                    alias.clone(),
                    predicate.clone(),
                )))
            }
        }
        Plan::RowidRange {
            table,
            alias,
            start,
            end,
            residual,
        } => Some(Box::new(RangeDriver::new(
            table.clone(),
            alias.clone(),
            start.clone(),
            end.clone(),
            residual.clone(),
        ))),
        Plan::Filter { input, predicate } => {
            // FUSED: Filter over a bare table Scan becomes ONE driver that
            // scans with a compiled predicate (positional comparison tree)
            // and selective decode — non-matching rows cost a decode of
            // just the predicate's columns, never a full-row Vec (the
            // generic FilterDriver materializes every row first, then
            // walks the predicate AST with per-row name lookups). Only
            // taken when the predicate actually compiles (correct alias
            // prefix); otherwise the generic chain applies.
            if let Plan::Scan {
                table,
                alias,
                index: None,
                predicate: scan_pred,
            } = input.as_ref()
            {
                if scan_pred.is_none()
                    && table.vtab.is_none()
                    && FilteredScanDriver::compiles(table, alias.as_deref(), predicate)
                {
                    return Some(Box::new(FilteredScanDriver::new(
                        table.clone(),
                        alias.clone(),
                        predicate.clone(),
                    )));
                }
            }
            let base = try_build_driver(input)?;
            Some(Box::new(FilterDriver::new(base, predicate.clone())))
        }
        Plan::Project { input, columns } => {
            // FUSED: Project over a bare Scan with a bare-column
            // projection — the scan decodes ONLY the projected columns
            // (overflow-aware: wide TEXT/BLOB columns are gathered
            // directly into the value's buffer, unprojected columns
            // never touch their chain pages). Without this, the generic
            // chain materializes FULL rows in the ScanDriver (including
            // every wide column) and re-projects per batch.
            if let Plan::Scan {
                table,
                alias: _,
                index: None,
                predicate: None,
            } = input.as_ref()
            {
                if table.vtab.is_none() {
                    if let Some((project, out_cols)) = bare_column_projection(columns, table) {
                        // `project == None` means SELECT * (all columns,
                        // table order) — `unwrap_or_default()` would turn
                        // it into an EMPTY projection and the scan would
                        // decode ZERO columns (empty rows, values lost).
                        let project = project.unwrap_or_else(|| (0..table.n_columns()).collect());
                        return Some(Box::new(ProjectedScanDriver::new(
                            table.clone(),
                            project,
                            out_cols,
                        )));
                    }
                }
            }
            // FUSED: Project over a RowidRange with a bare-column
            // projection and no residual — selective decode inside the
            // rowid bounds (the streaming twin of
            // exec_rowid_range_projected_impl). `SELECT id, val FROM t
            // WHERE id BETWEEN ? AND ?` full-decoded every row in
            // RangeDriver (wide columns included) and re-projected per
            // batch; this skips the un-projected decode entirely.
            if let Plan::RowidRange {
                table,
                alias: _,
                start,
                end,
                residual: None,
            } = input.as_ref()
            {
                if table.vtab.is_none() {
                    if let Some((Some(project), out_cols)) = bare_column_projection(columns, table)
                    {
                        return Some(Box::new(ProjectedRangeDriver::new(
                            table.clone(),
                            project,
                            out_cols,
                            start.clone(),
                            end.clone(),
                        )));
                    }
                }
            }
            let base = try_build_driver(input)?;
            Some(Box::new(ProjectDriver::new(base, columns.clone())))
        }
        Plan::Limit {
            input,
            count,
            offset,
        } => {
            let base = try_build_driver(input)?;
            Some(Box::new(LimitDriver::new(
                base,
                count.clone(),
                offset.clone(),
            )))
        }
        _ => None,
    }
}

/// FUSED Project-over-Scan driver: the scan decodes ONLY the projected
/// columns per row (the streaming twin of the executor's
/// `exec_scan_projected` fusion), resumable across batches by rowid.
/// Wide TEXT/BLOB columns spill to overflow chains: the walk gathers the
/// projected column's byte range DIRECTLY into the value's own buffer
/// (single copy), and unprojected columns never touch their chain pages.
struct ProjectedScanDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    /// Wanted column indices, in the projection's output order.
    project: Vec<usize>,
    last_rowid: i64,
    eof: bool,
}

impl ProjectedScanDriver {
    fn new(table: Arc<crate::schema::Table>, project: Vec<usize>, out_cols: Arc<[String]>) -> Self {
        Self {
            table,
            columns: out_cols,
            project,
            last_rowid: i64::MIN,
            eof: false,
        }
    }
}

impl Driver for ProjectedScanDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        _params: &[Value],
        _named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.read_maps();
        let ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        let root = ctx.table_root(&self.table);
        let mut bt = crate::storage::btree::Btree::new(ctx.pager, root, false);
        let n_cols = self.table.n_columns();
        let rowid_alias = self.table.rowid_alias;
        let start = if self.last_rowid == i64::MIN {
            i64::MIN
        } else {
            self.last_rowid + 1
        };
        // Sequential-scan widening lives at the TOP serving loop (`step`),
        // not here: wrapper drivers (LIMIT/OFFSET, Filter) rely on this
        // call returning AT MOST `budget` rows to keep their row
        // accounting exact. Widening inside would make a base batch
        // overshoot LimitDriver's offset/limit bookkeeping.
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        let mut batch_bytes = 0usize;
        bt.scan_table_range_selective_pooled(
            start,
            i64::MAX,
            n_cols,
            &self.project,
            rowid_alias,
            pool,
            |rowid, row| {
                if out.len() >= budget {
                    hit_end = false;
                    return false; // resume NEXT batch at this row
                }
                batch_bytes += row_heap_bytes(&row);
                out.push(row);
                // `last` tracks only PUSHED rows: a row stopped-on
                // (budget/bytes cap) was NOT delivered, so the resume
                // start (`last + 1`) must land ON it, not past it.
                last = rowid;
                if batch_bytes >= MAX_BATCH_BYTES {
                    // Wide rows: 64 x 64 KB blobs = 4 MB of live allocations
                    // per batch (fresh mimalloc pages each time — the freed
                    // batch's pages are deferred). Cap the batch's LIVE
                    // footprint; small rows never hit this.
                    hit_end = false;
                    return false;
                }
                true
            },
        )?;
        self.eof = hit_end && out.len() < budget;
        self.last_rowid = last;
        Ok(out)
    }
}

/// FUSED ProjectedRangeDriver: `Project(RowidRange)` with a bare-column
/// projection and no residual decodes ONLY the projected columns inside
/// the rowid range (selective decode) — the streaming twin of the
/// executor's `exec_rowid_range_projected_impl`. `SELECT id, val FROM t
/// WHERE id BETWEEN ? AND ?` previously full-decoded every row (wide
/// TEXT columns included) in RangeDriver and re-projected per batch;
/// this skips the un-projected decode entirely. Bounds may be
/// parameters (re-evaluated per batch, resume-aware).
struct ProjectedRangeDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    /// Wanted column indices, in the projection's output order.
    project: Vec<usize>,
    start: Option<Expr>,
    end: Option<Expr>,
    last_rowid: i64,
    eof: bool,
}

impl ProjectedRangeDriver {
    fn new(
        table: Arc<crate::schema::Table>,
        project: Vec<usize>,
        out_cols: Arc<[String]>,
        start: Option<Expr>,
        end: Option<Expr>,
    ) -> Self {
        Self {
            table,
            columns: out_cols,
            project,
            start,
            end,
            last_rowid: i64::MIN,
            eof: false,
        }
    }
}

impl Driver for ProjectedRangeDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.read_maps();
        let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        for p in params {
            ctx.bind_positional(p.clone());
        }
        let _g = db.plugin_scope();
        let _c = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = crate::executor::EvalContext::new(&empty_row, &empty_cols, params, named);
        // Bounds: re-evaluated per batch (parameters may change); the
        // resume clamp keeps mid-range batches from re-reading rows.
        let lo = match &self.start {
            Some(e) => {
                let v = crate::executor::expr::evaluate(e, &eval_ctx)?.as_integer();
                if self.last_rowid != i64::MIN && self.last_rowid + 1 > v {
                    self.last_rowid + 1
                } else {
                    v
                }
            }
            None => {
                if self.last_rowid == i64::MIN {
                    i64::MIN
                } else {
                    self.last_rowid + 1
                }
            }
        };
        let hi = match &self.end {
            Some(e) => crate::executor::expr::evaluate(e, &eval_ctx)?.as_integer(),
            None => i64::MAX,
        };
        if lo > hi {
            self.eof = true;
            return Ok(Vec::new());
        }
        let root = ctx.table_root(&self.table);
        let mut bt = crate::storage::btree::Btree::new(ctx.pager, root, false);
        let n_cols = self.table.n_columns();
        let rowid_alias = self.table.rowid_alias;
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        let mut batch_bytes = 0usize;
        bt.scan_table_range_selective_pooled(
            lo,
            hi,
            n_cols,
            &self.project,
            rowid_alias,
            pool,
            |rowid, row| {
                if out.len() >= budget {
                    hit_end = false;
                    return false; // resume NEXT batch at this row
                }
                batch_bytes += row_heap_bytes(&row);
                out.push(row);
                // `last` tracks only PUSHED rows (see ProjectedScanDriver).
                last = rowid;
                if batch_bytes >= MAX_BATCH_BYTES {
                    hit_end = false;
                    return false;
                }
                true
            },
        )?;
        self.eof = hit_end && out.len() < budget;
        self.last_rowid = last;
        Ok(out)
    }
}

/// Full-table scan with rowid resume: each batch re-seeks to
/// (last_rowid + 1) and scans up to `budget` rows — one B+tree descent
/// plus the batch's rows, never the whole table at once.
struct ScanDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    predicate: Option<Expr>,
    /// Trailing hidden-rowid slot (no-alias tables): every decoded row
    /// carries the rowid so expressions above can reference it.
    append_rowid: bool,
    last_rowid: i64,
    eof: bool,
}

impl ScanDriver {
    fn new(
        table: Arc<crate::schema::Table>,
        alias: Option<String>,
        predicate: Option<Expr>,
    ) -> Self {
        let append_rowid = crate::planner::wants_rowid_slot(&table);
        let columns = if append_rowid {
            scan_columns_with_rowid(&table, alias.as_deref())
        } else {
            scan_columns(&table, alias.as_deref())
        };
        Self {
            table,
            columns,
            predicate,
            append_rowid,
            last_rowid: i64::MIN,
            eof: false,
        }
    }
}

impl Driver for ScanDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.read_maps();
        let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        for p in params {
            ctx.bind_positional(p.clone());
        }
        let _g = db.plugin_scope();
        let _c = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        let root = ctx.table_root(&self.table);
        let mut bt = crate::storage::btree::Btree::new(ctx.pager, root, false);
        let n_cols = self.table.n_columns();
        let rowid_alias = self.table.rowid_alias;
        let predicate = self.predicate.as_ref();
        // Predicate-eval scratch: only built when a predicate exists (a
        // bare `SELECT *` scan pays no per-batch string Vec + params copy).
        let col_names: Vec<String> = if predicate.is_some() {
            self.columns.iter().cloned().collect()
        } else {
            Vec::new()
        };
        let params_owned: Vec<Value> = if predicate.is_some() {
            params.to_vec()
        } else {
            Vec::new()
        };
        let start = if self.last_rowid == i64::MIN {
            i64::MIN
        } else {
            self.last_rowid + 1
        };
        // Budget contract: wrapper drivers (LIMIT/OFFSET, Filter) count on
        // AT MOST `budget` rows back; widening happens in `step()` only.
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        let mut batch_bytes = 0usize;
        bt.scan_table_range_borrowed(start, i64::MAX, |rowid, payload| {
            if out.len() >= budget {
                hit_end = false;
                return false; // stop the walk; resume next batch
            }
            let mut row: Row = match pool.pop() {
                Some(mut r) => {
                    r.clear();
                    r
                }
                None => Vec::with_capacity(n_cols + 1),
            };
            if crate::storage::row_codec::decode_row_into(
                payload,
                n_cols,
                rowid,
                rowid_alias,
                &mut row,
            )
            .is_ok()
            {
                // Hidden rowid slot BEFORE predicate eval: the predicate
                // may itself reference rowid (name resolution happens
                // against self.columns, which carries the slot).
                if self.append_rowid {
                    row.push(Value::Integer(rowid));
                }
                if let Some(pred) = predicate {
                    match crate::executor::eval_row(pred, &row, &col_names, &params_owned, named) {
                        Ok(v) if v.is_truthy() => {}
                        _ => {
                            last = last.max(rowid);
                            row.clear();
                            pool.push(row);
                            return true;
                        }
                    }
                }
                batch_bytes += row_heap_bytes(&row);
                out.push(row);
                // `last` tracks PUSHED rows BEFORE any stop: a byte-cap
                // stop AFTER a push must resume past the pushed row.
                last = last.max(rowid);
                if batch_bytes >= MAX_BATCH_BYTES {
                    // Wide rows: bound ONE batch's live footprint (see
                    // ProjectedScanDriver) — 64 KB blobs x a wide top-level
                    // pull would otherwise hold tens of MB in the pending
                    // queue.
                    hit_end = false;
                    return false;
                }
                return true;
            }
            last = last.max(rowid);
            true
        })?;
        self.eof = hit_end && out.len() < budget;
        self.last_rowid = last;
        Ok(out)
    }
}

/// FUSED Filter-over-Scan driver: scans with a COMPILED predicate and
/// SELECTIVE decode, resumable across batches by rowid.
///
/// This is the streaming twin of the executor's `scan_filter_limit` fast
/// path: non-matching rows cost only a decode of the predicate's columns
/// (no full-row Vec, no AST walk, no per-row name lookups); matching rows
/// are fully materialized for the driver chain above. Bounded by `budget`
/// PASSING rows per call, so a `LimitDriver` with a small limit stops the
/// walk almost immediately.
struct FilteredScanDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    predicate: Expr,
    /// Alias (or table name) the predicate compiles against.
    prefix: String,
    /// Trailing hidden-rowid slot (no-alias tables; see ScanDriver). The
    /// COMPILED predicate never references it (a hidden-name column ref
    /// fails compile_predicate, declining the fusion), but MATCHED rows
    /// carry the slot for the projection drivers above.
    append_rowid: bool,
    last_rowid: i64,
    eof: bool,
}

impl FilteredScanDriver {
    /// Does the predicate compile against this (table, prefix)? Gates the
    /// fusion at driver-build time so a non-compiling predicate keeps the
    /// generic (correct, slower) chain.
    fn compiles(table: &Arc<crate::schema::Table>, alias: Option<&str>, predicate: &Expr) -> bool {
        let prefix = alias.unwrap_or(&table.name);
        crate::executor::predicate::compile_predicate(predicate, table, prefix).is_some()
    }

    fn new(table: Arc<crate::schema::Table>, alias: Option<String>, predicate: Expr) -> Self {
        let append_rowid = crate::planner::wants_rowid_slot(&table);
        let columns = if append_rowid {
            scan_columns_with_rowid(&table, alias.as_deref())
        } else {
            scan_columns(&table, alias.as_deref())
        };
        let prefix = alias.unwrap_or_else(|| table.name.clone());
        Self {
            table,
            columns,
            predicate,
            prefix,
            append_rowid,
            last_rowid: i64::MIN,
            eof: false,
        }
    }
}

impl Driver for FilteredScanDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        _named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.read_maps();
        let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        for p in params {
            ctx.bind_positional(p.clone());
        }
        let _g = db.plugin_scope();
        let _c = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        let Some(pred) = crate::executor::predicate::compile_predicate(
            &self.predicate,
            &self.table,
            &self.prefix,
        ) else {
            // Compilability was checked at build time; if it somehow fails
            // here (schema drift), degrade to the generic materialized path
            // rather than silently dropping rows.
            let alias = if self.prefix == self.table.name {
                None
            } else {
                Some(self.prefix.clone())
            };
            let mut generic = FilterDriver::new(
                Box::new(ScanDriver::new(self.table.clone(), alias, None)),
                self.predicate.clone(),
            );
            return generic.next_batch(db, params, _named, budget, pool);
        };
        let root = ctx.table_root(&self.table);
        let mut bt = crate::storage::btree::Btree::new(ctx.pager, root, false);
        let n_cols = self.table.n_columns();
        let rowid_alias = self.table.rowid_alias;
        // Wanted = predicate columns (selective decode while scanning).
        let mut wanted: Vec<usize> = Vec::new();
        crate::executor::predicate::compiled_columns(&pred, &mut wanted);
        let start = if self.last_rowid == i64::MIN {
            i64::MIN
        } else {
            self.last_rowid + 1
        };
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        // FUSED RANGE-PROBE SCAN: a two-sided INTEGER range on one column
        // (`a BETWEEN ? AND ?`) skips the selective decode + predicate
        // dispatch entirely — the payload probe reads ONE column's bytes
        // (~3-8 ns/row vs ~40-60). Rowid-alias columns are even cheaper:
        // the range becomes the B+tree's own rowid bounds (a seek, not a
        // scan). Non-numeric payloads cannot match a two-sided INTEGER
        // range (SQLite's type order: numbers < TEXT/BLOB; NULL never
        // matches), so skipping them is exact.
        let range = if let Some(r) = crate::executor::predicate::try_int_column_range(
            &self.predicate,
            &self.table,
            &self.prefix,
        ) {
            // Bounds must be EXACT i64s: a REAL bound (BETWEEN 8.5 AND
            // 15.5) or a TEXT-typed param ('5') must NOT truncate into
            // the fused range — those shapes decline to the general
            // predicate path, which compares them with full mixed-type
            // semantics (SQLite: INTEGER < REAL < TEXT by value class).
            let resolve = |v: &crate::executor::predicate::PredValue| -> Option<i64> {
                match v {
                    crate::executor::predicate::PredValue::Literal(
                        crate::types::Value::Integer(i),
                    ) => Some(*i),
                    crate::executor::predicate::PredValue::Param(i) => match params.get(*i) {
                        Some(crate::types::Value::Integer(n)) => Some(*n),
                        _ => None,
                    },
                    _ => None,
                }
            };
            match (resolve(&r.lo), resolve(&r.hi)) {
                (Some(lo), Some(hi)) if lo <= hi => Some((r.col, lo, hi)),
                _ => None,
            }
        } else {
            None
        };
        if let Some((col, lo, hi)) = range {
            let append_rowid = self.append_rowid;
            if rowid_alias == Some(col) {
                // The range is over the rowid: bound the walk itself.
                let walk_lo = start.max(lo);
                bt.scan_table_range_borrowed(walk_lo, hi, |rowid, payload| {
                    if out.len() >= budget {
                        hit_end = false;
                        return false;
                    }
                    last = last.max(rowid);
                    let mut row: Row = match pool.pop() {
                        Some(mut r) => {
                            r.clear();
                            r
                        }
                        None => Vec::with_capacity(n_cols + 1),
                    };
                    if crate::storage::row_codec::decode_row_into(
                        payload,
                        n_cols,
                        rowid,
                        rowid_alias,
                        &mut row,
                    )
                    .is_ok()
                    {
                        if append_rowid {
                            row.push(Value::Integer(rowid));
                        }
                        out.push(row);
                    }
                    true
                })?;
            } else {
                bt.scan_table_range_borrowed(start, i64::MAX, |rowid, payload| {
                    if out.len() >= budget {
                        hit_end = false;
                        return false;
                    }
                    last = last.max(rowid);
                    match crate::storage::row_codec::probe_int_column(
                        payload,
                        col,
                        rowid,
                        rowid_alias,
                    ) {
                        Some(crate::storage::row_codec::ProbeNum::Int(v)) => {
                            if v >= lo && v <= hi {
                                if let Ok(mut row) = crate::storage::row_codec::decode_row(
                                    payload,
                                    n_cols,
                                    rowid,
                                    rowid_alias,
                                ) {
                                    if append_rowid {
                                        row.push(Value::Integer(rowid));
                                    }
                                    out.push(row);
                                }
                            }
                        }
                        // Exact mixed INTEGER/REAL comparison via match
                        // guard: no lossy `lo as f64` casts (SQLite
                        // compares across the numeric class exactly, and
                        // so does the fused probe).
                        Some(crate::storage::row_codec::ProbeNum::Real(v))
                            if crate::storage::row_codec::real_ge_int(v, lo)
                                && crate::storage::row_codec::real_le_int(v, hi) =>
                        {
                            let mut row: Row = match pool.pop() {
                                Some(mut r) => {
                                    r.clear();
                                    r
                                }
                                None => Vec::with_capacity(n_cols + 1),
                            };
                            if crate::storage::row_codec::decode_row_into(
                                payload,
                                n_cols,
                                rowid,
                                rowid_alias,
                                &mut row,
                            )
                            .is_ok()
                            {
                                if append_rowid {
                                    row.push(Value::Integer(rowid));
                                }
                                out.push(row);
                            }
                        }
                        // REAL outside the range: no match.
                        Some(crate::storage::row_codec::ProbeNum::Real(_)) => {}
                        None => {} // NULL/TEXT/BLOB/absent: cannot match
                    }
                    true
                })?;
            }
            self.eof = hit_end && out.len() < budget;
            self.last_rowid = last;
            return Ok(out);
        }
        if wanted.is_empty() {
            // Degenerate (constant) predicate: full decode + positional eval.
            let positions: Vec<usize> = (0..n_cols).collect();
            let sel_pred = pred;
            bt.scan_table_range_borrowed(start, i64::MAX, |rowid, payload| {
                if out.len() >= budget {
                    hit_end = false;
                    return false;
                }
                last = last.max(rowid);
                let mut row: Row = match pool.pop() {
                    Some(mut r) => {
                        r.clear();
                        r
                    }
                    None => Vec::with_capacity(n_cols + 1),
                };
                let keep = crate::storage::row_codec::decode_row_into(
                    payload,
                    n_cols,
                    rowid,
                    rowid_alias,
                    &mut row,
                )
                .is_ok()
                    && sel_pred.eval(&row, &positions, params);
                if keep {
                    if self.append_rowid {
                        row.push(Value::Integer(rowid));
                    }
                    out.push(row);
                } else {
                    row.clear();
                    pool.push(row);
                }
                true
            })?;
        } else {
            wanted.sort_unstable();
            wanted.dedup();
            let mut positions = vec![usize::MAX; n_cols];
            for (pos, &c) in wanted.iter().enumerate() {
                positions[c] = pos;
            }
            let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
            let sel_pred = pred;
            bt.scan_table_range_borrowed(start, i64::MAX, |rowid, payload| {
                if out.len() >= budget {
                    hit_end = false;
                    return false; // stop the walk; resume next batch
                }
                last = last.max(rowid);
                if crate::storage::row_codec::decode_row_selective(
                    payload,
                    n_cols,
                    &wanted,
                    rowid,
                    rowid_alias,
                    &mut sel_buf,
                )
                .is_err()
                {
                    return true;
                }
                if !sel_pred.eval(&sel_buf, &positions, params) {
                    return true; // non-matching row: selective decode only
                }
                // Matching row: materialize the full row for the chain above.
                let mut row: Row = match pool.pop() {
                    Some(mut r) => {
                        r.clear();
                        r
                    }
                    None => Vec::with_capacity(n_cols + 1),
                };
                if crate::storage::row_codec::decode_row_into(
                    payload,
                    n_cols,
                    rowid,
                    rowid_alias,
                    &mut row,
                )
                .is_ok()
                {
                    if self.append_rowid {
                        row.push(Value::Integer(rowid));
                    }
                    out.push(row);
                } else {
                    row.clear();
                    pool.push(row);
                }
                true
            })?;
        }
        self.eof = hit_end && out.len() < budget;
        self.last_rowid = last;
        Ok(out)
    }
}

/// Rowid-range scan with resume.
struct RangeDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    start: Option<Expr>,
    end: Option<Expr>,
    residual: Option<Expr>,
    /// Trailing hidden-rowid slot (no-alias tables; see ScanDriver).
    append_rowid: bool,
    last_rowid: i64,
    eof: bool,
}

impl RangeDriver {
    fn new(
        table: Arc<crate::schema::Table>,
        alias: Option<String>,
        start: Option<Expr>,
        end: Option<Expr>,
        residual: Option<Expr>,
    ) -> Self {
        let append_rowid = crate::planner::wants_rowid_slot(&table);
        let columns = if append_rowid {
            scan_columns_with_rowid(&table, alias.as_deref())
        } else {
            scan_columns(&table, alias.as_deref())
        };
        Self {
            table,
            columns,
            start,
            end,
            residual,
            append_rowid,
            last_rowid: i64::MIN,
            eof: false,
        }
    }
}

impl Driver for RangeDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.read_maps();
        let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        for p in params {
            ctx.bind_positional(p.clone());
        }
        let _g = db.plugin_scope();
        let _c = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = crate::executor::EvalContext::new(&empty_row, &empty_cols, params, named);
        let lo = match &self.start {
            Some(e) => {
                let v = crate::executor::expr::evaluate(e, &eval_ctx)?.as_integer();
                if self.last_rowid != i64::MIN && self.last_rowid + 1 > v {
                    self.last_rowid + 1
                } else {
                    v
                }
            }
            None => {
                if self.last_rowid == i64::MIN {
                    i64::MIN
                } else {
                    self.last_rowid + 1
                }
            }
        };
        let hi = match &self.end {
            Some(e) => crate::executor::expr::evaluate(e, &eval_ctx)?.as_integer(),
            None => i64::MAX,
        };
        if lo > hi {
            self.eof = true;
            return Ok(Vec::new());
        }
        let root = ctx.table_root(&self.table);
        let mut bt = crate::storage::btree::Btree::new(ctx.pager, root, false);
        let n_cols = self.table.n_columns();
        let rowid_alias = self.table.rowid_alias;
        let residual = self.residual.as_ref();
        // Predicate scratch only when a residual exists (see ScanDriver).
        let col_names: Vec<String> = if residual.is_some() {
            self.columns.iter().cloned().collect()
        } else {
            Vec::new()
        };
        let params_owned: Vec<Value> = if residual.is_some() {
            params.to_vec()
        } else {
            Vec::new()
        };
        // Budget contract: wrapper drivers (LIMIT/OFFSET, Filter) count on
        // AT MOST `budget` rows back; widening happens in `step()` only.
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        let mut batch_bytes = 0usize;
        bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
            if out.len() >= budget {
                hit_end = false;
                return false;
            }
            let mut row: Row = match pool.pop() {
                Some(mut r) => {
                    r.clear();
                    r
                }
                None => Vec::with_capacity(n_cols + 1),
            };
            if crate::storage::row_codec::decode_row_into(
                payload,
                n_cols,
                rowid,
                rowid_alias,
                &mut row,
            )
            .is_ok()
            {
                // Hidden rowid slot BEFORE residual eval (see ScanDriver).
                if self.append_rowid {
                    row.push(Value::Integer(rowid));
                }
                if let Some(pred) = residual {
                    match crate::executor::eval_row(pred, &row, &col_names, &params_owned, named) {
                        Ok(v) if v.is_truthy() => {}
                        _ => {
                            last = last.max(rowid);
                            row.clear();
                            pool.push(row);
                            return true;
                        }
                    }
                }
                batch_bytes += row_heap_bytes(&row);
                out.push(row);
                // `last` tracks PUSHED rows BEFORE any stop: a byte-cap
                // stop AFTER a push must resume past the pushed row.
                last = last.max(rowid);
                if batch_bytes >= MAX_BATCH_BYTES {
                    // Wide rows: bound ONE batch's live footprint (see
                    // ProjectedScanDriver).
                    hit_end = false;
                    return false;
                }
                return true;
            }
            last = last.max(rowid);
            true
        })?;
        self.eof = hit_end && out.len() < budget;
        self.last_rowid = last;
        Ok(out)
    }
}

/// Virtual-table driver: one module cursor stepped across batches (true
/// streaming — the cursor keeps its own position). `eof` latches when the
/// cursor is exhausted so subsequent batches return empty (the cursor
/// itself is dropped then — reopening it would restart the scan).
struct VtabDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    cursor: Option<Box<dyn crate::plugin::VirtualTableCursor>>,
    eof: bool,
}

impl VtabDriver {
    fn new(table: Arc<crate::schema::Table>, alias: Option<String>) -> Self {
        let columns = scan_columns(&table, alias.as_deref());
        Self {
            table,
            columns,
            cursor: None,
            eof: false,
        }
    }
}

impl Driver for VtabDriver {
    fn columns(&self) -> Arc<[String]> {
        self.columns.clone()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        _params: &[Value],
        _named: &HashMap<String, Value>,
        budget: usize,
        _pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let inst = self
            .table
            .vtab
            .as_ref()
            .ok_or_else(|| Error::corruption("vtab driver on a non-virtual table".to_string()))?
            .clone();
        let mut cursor: Box<dyn crate::plugin::VirtualTableCursor> = match self.cursor.take() {
            Some(c) => c,
            None => {
                let _g = db.plugin_scope();
                let mut c: Box<dyn crate::plugin::VirtualTableCursor> = inst.with_table(|vt| {
                    let _info = vt.best_index(&[])?;
                    vt.open()
                })?;
                c.filter(0, None, &[])?;
                c
            }
        };
        let n_cols = self.table.n_columns();
        let mut out = Vec::with_capacity(budget.min(BATCH));
        while !cursor.eof() && out.len() < budget {
            let mut row = Vec::with_capacity(n_cols);
            for i in 0..n_cols {
                row.push(cursor.column(i)?);
            }
            out.push(row);
            cursor.next()?;
        }
        if cursor.eof() {
            // Exhausted: latch EOF and drop the cursor.
            self.eof = true;
        } else {
            self.cursor = Some(cursor);
        }
        Ok(out)
    }
}

/// Filter wrapper: predicate applied per row over a base driver.
struct FilterDriver {
    base: Box<dyn Driver>,
    predicate: Expr,
    /// Leftover rows that matched but exceeded the budget.
    leftover: Vec<Row>,
    base_eof: bool,
}

impl FilterDriver {
    fn new(base: Box<dyn Driver>, predicate: Expr) -> Self {
        Self {
            base,
            predicate,
            leftover: Vec::new(),
            base_eof: false,
        }
    }
}

impl Driver for FilterDriver {
    fn columns(&self) -> Arc<[String]> {
        self.base.columns()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if !self.leftover.is_empty() {
            let take = self.leftover.len().min(budget);
            let out: Vec<Row> = self.leftover.drain(..take).collect();
            return Ok(out);
        }
        if self.base_eof {
            return Ok(Vec::new());
        }
        let cols = self.base.columns();
        let col_names: Vec<String> = cols.iter().cloned().collect();
        let params_owned: Vec<Value> = params.to_vec();
        let mut matched: Vec<Row> = Vec::new();
        while matched.len() < budget {
            let batch = self
                .base
                .next_batch(db, params, named, (budget * 4).max(BATCH), pool)?;
            if batch.is_empty() {
                self.base_eof = true;
                break;
            }
            for row in batch {
                let keep = crate::executor::eval_row(
                    &self.predicate,
                    &row,
                    &col_names,
                    &params_owned,
                    named,
                )
                .map(|v| v.is_truthy())
                .unwrap_or(false);
                if keep {
                    matched.push(row);
                    if matched.len() >= budget {
                        break;
                    }
                }
            }
        }
        Ok(matched)
    }
}

/// Projection wrapper: evaluates projection expressions per row.
struct ProjectDriver {
    base: Box<dyn Driver>,
    exprs: Vec<crate::planner::plan::ProjectExpr>,
}

impl ProjectDriver {
    fn new(base: Box<dyn Driver>, columns: Vec<crate::planner::plan::ProjectExpr>) -> Self {
        Self {
            base,
            exprs: columns,
        }
    }
}

impl Driver for ProjectDriver {
    fn columns(&self) -> Arc<[String]> {
        let inner_cols = self.base.columns();
        let inner: Vec<String> = inner_cols
            .iter()
            .filter(|c| !crate::planner::is_hidden_rowid(c))
            .cloned()
            .collect();
        let mut names: Vec<String> = Vec::new();
        for c in &self.exprs {
            match &c.expr {
                Expr::Column { name, .. } if name == "*" => {
                    names.extend(inner.iter().cloned());
                }
                _ => {
                    names.push(c.alias.clone().unwrap_or_else(|| expr_display(&c.expr)));
                }
            }
        }
        names.into()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        let cols = self.base.columns();
        let col_names: Vec<String> = cols.iter().cloned().collect();
        // Hidden rowid slots are TRAILING (ScanDriver & friends append
        // them last): `SELECT *` must expand only the VISIBLE columns, so
        // slice the trailing slots off each row before extending.
        let hidden_count = col_names
            .iter()
            .filter(|c| crate::planner::is_hidden_rowid(c))
            .count();
        let params_owned: Vec<Value> = params.to_vec();
        let exprs: Vec<Expr> = self.exprs.iter().map(|c| c.expr.clone()).collect();
        let batch = self.base.next_batch(db, params, named, budget, pool)?;
        let mut out = Vec::with_capacity(batch.len());
        for row in batch {
            let mut projected = match pool.pop() {
                Some(mut r) => {
                    r.clear();
                    r
                }
                None => Vec::with_capacity(exprs.len()),
            };
            for e in &exprs {
                match e {
                    Expr::Column { name, .. } if name == "*" => {
                        // Hidden slots are TRAILING: expand only the
                        // visible columns.
                        let take = row.len().saturating_sub(hidden_count);
                        projected.extend_from_slice(&row[..take]);
                    }
                    _ => projected.push(crate::executor::eval_row(
                        e,
                        &row,
                        &col_names,
                        &params_owned,
                        named,
                    )?),
                }
            }
            out.push(projected);
        }
        Ok(out)
    }
}

/// LIMIT / OFFSET wrapper.
struct LimitDriver {
    base: Box<dyn Driver>,
    count: Expr,
    offset: Expr,
    /// Remaining rows to deliver (None = no limit).
    remaining: Option<i64>,
    /// Rows still to skip.
    offset_left: i64,
    initialized: bool,
    base_eof: bool,
}

impl LimitDriver {
    fn new(base: Box<dyn Driver>, count: Expr, offset: Expr) -> Self {
        Self {
            base,
            count,
            offset,
            remaining: None,
            offset_left: 0,
            initialized: false,
            base_eof: false,
        }
    }
}

impl Driver for LimitDriver {
    fn columns(&self) -> Arc<[String]> {
        self.base.columns()
    }
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
        pool: &mut Vec<Row>,
    ) -> Result<Vec<Row>> {
        if !self.initialized {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let ec = crate::executor::EvalContext::new(&empty_row, &empty_cols, params, named);
            let count = crate::executor::expr::evaluate(&self.count, &ec)?.as_integer();
            let offset = crate::executor::expr::evaluate(&self.offset, &ec)?.as_integer();
            self.remaining = if count < 0 { None } else { Some(count) };
            self.offset_left = offset.max(0);
            self.initialized = true;
        }
        if self.remaining == Some(0) {
            return Ok(Vec::new());
        }
        // Skip offset rows.
        while self.offset_left > 0 {
            if self.base_eof {
                return Ok(Vec::new());
            }
            // offset_left > 0 here, so the value is already >= 1 — clamp
            // (not min/max pairs) documents the [1, BATCH] window.
            let want = (self.offset_left as usize).clamp(1, BATCH);
            let batch = self.base.next_batch(db, params, named, want, pool)?;
            if batch.is_empty() {
                self.base_eof = true;
                return Ok(Vec::new());
            }
            self.offset_left -= batch.len() as i64;
        }
        if self.base_eof {
            return Ok(Vec::new());
        }
        let limit_left = self.remaining.unwrap_or(i64::MAX);
        let want = (budget as i64).min(limit_left).max(0) as usize;
        if want == 0 {
            return Ok(Vec::new());
        }
        let batch = self.base.next_batch(db, params, named, want, pool)?;
        if batch.is_empty() {
            self.base_eof = true;
        }
        if let Some(r) = &mut self.remaining {
            *r -= batch.len() as i64;
        }
        Ok(batch)
    }
}

/// Display name for an expression column (a practical subset of the
/// executor's naming rules, sufficient for statement consumers).
fn expr_display(e: &Expr) -> String {
    match e {
        Expr::Column {
            table: Some(t),
            name,
        } => format!("{}.{}", t, name),
        Expr::Column { table: None, name } => name.clone(),
        Expr::Literal(Value::Text(s)) => s.as_str().to_string(),
        Expr::Literal(Value::Integer(i)) => i.to_string(),
        Expr::Literal(Value::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

/// Output columns of a precompiled FastPath result.
fn fp_output_columns(fp: &FastPath) -> Arc<[String]> {
    fp.output_columns_public()
}
