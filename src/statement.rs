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
//! let db = Database::open_in_memory().unwrap();
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
use crate::executor::{execute, ExecContext};
use crate::planner::plan::Plan;
use crate::sql::ast::{Expr, Statement as AstStatement};
use crate::types::{Row, Value};
use std::collections::VecDeque;
use std::collections::HashMap;
use std::sync::Arc;

/// Batch size for streaming drivers.
const BATCH: usize = 64;

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
    plan: Option<Plan>,
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
    columns: Option<Arc<[String]>>,
    current_row: Option<Row>,
    done: bool,
    /// Post-execution map deltas (DML merge-back, mirrors query()).
    deltas: CtxDeltas,
    changes_at_start: i64,
}

#[derive(Default)]
struct CtxDeltas {
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
        let plan = cached.plan.as_ref().map(|p| (**p).clone());
        // Output columns at PREPARE time when a streaming driver covers
        // the plan (SQLite's column_count/column_name work before the
        // first step). Materialized shapes set columns on first step.
        let prep_columns = plan.as_ref().and_then(try_build_driver).map(|d| d.columns());
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
            columns: prep_columns,
            current_row: None,
            done: false,
            deltas: CtxDeltas::default(),
            changes_at_start: db.total_changes(),
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
        self.pending.clear();
        self.columns = None;
        self.current_row = None;
        self.done = false;
        self.deltas = CtxDeltas::default();
        self.changes_at_start = self.db.total_changes();
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
            self.start()?;
        }
        // Serve one buffered row.
        if let Some(row) = self.pending.pop_front() {
            self.current_row = Some(row);
            return Ok(StepResult::Row);
        }
        match &mut self.stream {
            StreamState::Driver(drv) => {
                let db = self.db;
                let params = self.params.clone();
                let named = self.named.clone();
                let batch = drv.next_batch(db, &params, &named, BATCH)?;
                if batch.is_empty() {
                    self.done = true;
                    self.stream = StreamState::Exhausted;
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
                    self.current_row = Some(row);
                    Ok(StepResult::Row)
                }
                None => {
                    self.done = true;
                    self.stream = StreamState::Exhausted;
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
        if self.db.deferred_flush.load(std::sync::atomic::Ordering::Acquire)
            && self.db.pager.has_dirty_pages()
        {
            let _ = self.db.pager.flush();
        }

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

        // Precompiled point/range fast paths: a handful of rows.
        if let Some(fp) = self.fast_path.clone() {
            let rows = self.db.run_fast_path_public(&fp, &self.params)?;
            self.columns = Some(fp_output_columns(&fp));
            self.stream = StreamState::Materialized(rows.into_iter());
            return Ok(());
        }

        if let Some(plan) = self.plan.clone() {
            // Streaming driver?
            if let Some(drv) = try_build_driver(&plan) {
                self.columns = Some(drv.columns());
                self.stream = StreamState::Driver(drv);
                return Ok(());
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
                plan,
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
                self.merge_dml_maps();
                self.db.sync_schema_roots_public()?;
            } else if self.deltas.max_rowids_changed {
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
        let shared = db.maps.read().clone();
        let in_txn = db.in_transaction.load(std::sync::atomic::Ordering::Acquire);
        let txn_snap = if in_txn { db.txn_snapshot.lock().clone() } else { None };
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
        let out = out?;
        // Capture deltas (query()'s DML merge-back semantics).
        self.deltas = CtxDeltas {
            max_rowids: ctx.max_rowids.clone(),
            max_rowids_invalidated: ctx.max_rowids_invalidated.clone(),
            max_rowids_changed: ctx.max_rowids_changed || !ctx.max_rowids_invalidated.is_empty(),
        };
        Ok(out)
    }

    fn merge_dml_maps(&mut self) {
        // DML via the shared path: merge root/index/max-rowid deltas into
        // the Database's bookkeeping maps (mirrors Database::query).
        let mut m = self.db.maps.write();
        let bk = Arc::make_mut(&mut *m);
        if self.deltas.max_rowids_changed {
            bk.max_rowids.extend(self.deltas.max_rowids.drain());
            for k in self.deltas.max_rowids_invalidated.drain(..) {
                bk.max_rowids.remove(&k);
            }
        }
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

    fn walk_expr(e: &Expr, max_pos: &mut usize, saw_numeric: &mut bool, named: &mut Vec<String>, counter: &mut usize) {
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
                        && !named
                            .iter()
                            .any(|n| n.trim_start_matches([':', '@', '$']).eq_ignore_ascii_case(&bare))
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
            Expr::Between { expr, low, high, .. } => {
                walk_expr(expr, max_pos, saw_numeric, named, counter);
                walk_expr(low, max_pos, saw_numeric, named, counter);
                walk_expr(high, max_pos, saw_numeric, named, counter);
            }
            Expr::In { expr, source, .. } => {
                walk_expr(expr, max_pos, saw_numeric, named, counter);
                if let crate::sql::ast::InSource::List(items) = source {
                    for i in items {
                        walk_expr(i, max_pos, saw_numeric, named, counter);
                    }
                }
            }
            Expr::Like { expr, pattern, escape, .. } => {
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
            Expr::Case { operand, whens, else_ } => {
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

    fn walk_body(b: &crate::sql::ast::SelectBody, max_pos: &mut usize, saw_numeric: &mut bool, named: &mut Vec<String>, counter: &mut usize) {
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

    fn walk_table(te: &crate::sql::ast::TableExpression, max_pos: &mut usize, saw_numeric: &mut bool, named: &mut Vec<String>, counter: &mut usize) {
        match te {
            crate::sql::ast::TableExpression::Subquery { select, .. } => {
                walk_select(select, max_pos, saw_numeric, named, counter)
            }
            crate::sql::ast::TableExpression::Join { left, right, constraint, .. } => {
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
        AstStatement::Select(s) => walk_select(s, &mut max_pos, &mut saw_numeric, named, &mut counter),
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
    fn next_batch(
        &mut self,
        db: &Database,
        params: &[Value],
        named: &HashMap<String, Value>,
        budget: usize,
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

/// Try to build a streaming driver for a plan shape. `None` → materialize.
fn try_build_driver(plan: &Plan) -> Option<Box<dyn Driver>> {
    match plan {
        Plan::Scan { table, alias, index: None, predicate } => {
            if table.vtab.is_some() {
                Some(Box::new(VtabDriver::new(table.clone(), alias.clone())))
            } else {
                Some(Box::new(ScanDriver::new(table.clone(), alias.clone(), predicate.clone())))
            }
        }
        Plan::RowidRange { table, alias, start, end, residual } => Some(Box::new(RangeDriver::new(
            table.clone(),
            alias.clone(),
            start.clone(),
            end.clone(),
            residual.clone(),
        ))),
        Plan::Filter { input, predicate } => {
            let base = try_build_driver(input)?;
            Some(Box::new(FilterDriver::new(base, predicate.clone())))
        }
        Plan::Project { input, columns } => {
            let base = try_build_driver(input)?;
            Some(Box::new(ProjectDriver::new(base, columns.clone())))
        }
        Plan::Limit { input, count, offset } => {
            let base = try_build_driver(input)?;
            Some(Box::new(LimitDriver::new(base, count.clone(), offset.clone())))
        }
        _ => None,
    }
}

/// Full-table scan with rowid resume: each batch re-seeks to
/// (last_rowid + 1) and scans up to `budget` rows — one B+tree descent
/// plus the batch's rows, never the whole table at once.
struct ScanDriver {
    table: Arc<crate::schema::Table>,
    columns: Arc<[String]>,
    predicate: Option<Expr>,
    last_rowid: i64,
    eof: bool,
}

impl ScanDriver {
    fn new(table: Arc<crate::schema::Table>, alias: Option<String>, predicate: Option<Expr>) -> Self {
        let columns = scan_columns(&table, alias.as_deref());
        Self { table, columns, predicate, last_rowid: i64::MIN, eof: false }
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
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.maps.read().clone();
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
        let col_names: Vec<String> = self.columns.iter().cloned().collect();
        let params_owned: Vec<Value> = params.to_vec();
        let start = if self.last_rowid == i64::MIN { i64::MIN } else { self.last_rowid + 1 };
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        bt.scan_table_range_borrowed(start, i64::MAX, |rowid, payload| {
            if out.len() >= budget {
                hit_end = false;
                return false; // stop the walk; resume next batch
            }
            if let Ok(row) =
                crate::storage::row_codec::decode_row(payload, n_cols, rowid, rowid_alias)
            {
                if let Some(pred) = predicate {
                    match crate::executor::eval_row(pred, &row, &col_names, &params_owned, named) {
                        Ok(v) if v.is_truthy() => {}
                        _ => {
                            last = last.max(rowid);
                            return true;
                        }
                    }
                }
                out.push(row);
            }
            last = last.max(rowid);
            true
        })?;
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
        let columns = scan_columns(&table, alias.as_deref());
        Self { table, columns, start, end, residual, last_rowid: i64::MIN, eof: false }
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
    ) -> Result<Vec<Row>> {
        if self.eof {
            return Ok(Vec::new());
        }
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.maps.read().clone();
        let mut ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        for p in params {
            ctx.bind_positional(p.clone());
        }
        let _g = db.plugin_scope();
        let _c = crate::executor::CorrGuard::install(&mut ctx as *mut _);
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx =
            crate::executor::EvalContext::new(&empty_row, &empty_cols, params, named);
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
        let col_names: Vec<String> = self.columns.iter().cloned().collect();
        let params_owned: Vec<Value> = params.to_vec();
        let mut out: Vec<Row> = Vec::with_capacity(budget.min(BATCH));
        let mut last = self.last_rowid;
        let mut hit_end = true;
        bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
            if out.len() >= budget {
                hit_end = false;
                return false;
            }
            if let Ok(row) =
                crate::storage::row_codec::decode_row(payload, n_cols, rowid, rowid_alias)
            {
                if let Some(pred) = residual {
                    match crate::executor::eval_row(pred, &row, &col_names, &params_owned, named) {
                        Ok(v) if v.is_truthy() => {}
                        _ => {
                            last = last.max(rowid);
                            return true;
                        }
                    }
                }
                out.push(row);
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
        Self { table, columns, cursor: None, eof: false }
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
        Self { base, predicate, leftover: Vec::new(), base_eof: false }
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
            let batch = self.base.next_batch(db, params, named, (budget * 4).max(BATCH))?;
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
        Self { base, exprs: columns }
    }
}

impl Driver for ProjectDriver {
    fn columns(&self) -> Arc<[String]> {
        let inner_cols = self.base.columns();
        let inner: Vec<String> = inner_cols.iter().cloned().collect();
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
    ) -> Result<Vec<Row>> {
        let cols = self.base.columns();
        let col_names: Vec<String> = cols.iter().cloned().collect();
        let params_owned: Vec<Value> = params.to_vec();
        let exprs: Vec<Expr> = self.exprs.iter().map(|c| c.expr.clone()).collect();
        let batch = self.base.next_batch(db, params, named, budget)?;
        let mut out = Vec::with_capacity(batch.len());
        for row in batch {
            let mut projected = Vec::with_capacity(exprs.len());
            for e in &exprs {
                match e {
                    Expr::Column { name, .. } if name == "*" => {
                        projected.extend_from_slice(&row);
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
        Self { base, count, offset, remaining: None, offset_left: 0, initialized: false, base_eof: false }
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
            let want = (self.offset_left as usize).min(BATCH).max(1);
            let batch = self.base.next_batch(db, params, named, want)?;
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
        let batch = self.base.next_batch(db, params, named, want)?;
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
        Expr::Column { table: Some(t), name } => format!("{}.{}", t, name),
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
