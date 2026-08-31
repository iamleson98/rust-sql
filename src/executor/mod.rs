//! Executor: evaluates logical plans and produces rows.
//!
//! The executor walks a logical plan and returns a list of rows (and column
//! names). We use a recursive evaluator (collect-all model) rather than the
//! Volcano iterator model: this is simpler to reason about with Rust's borrow
//! checker and avoids the lifetime gymnastics that streaming iterators would
//! require when the same `&Pager` is shared between operators.
//!
//! For large result sets, the executor materializes everything in memory. A
//! production engine would use a pull-based streaming model with `Rc<RefCell<>>`
//! for shared state, but that adds complexity that doesn't pay off until you
//! have working code in the first place.

pub mod datetime;
pub mod expr;
pub mod json;
pub(crate) mod triggers;
pub(crate) mod explain;
pub(crate) mod predicate;

pub use expr::{apply_binary, evaluate, EvalContext};

// ============================================================================
// Correlated subqueries — evaluate-time execution bridge
// ----------------------------------------------------------------------------
// Uncorrelated subqueries are substituted at plan-rewrite time (mirrors
// SQLite's OP_Once). A CORRELATED subquery must re-execute per outer row
// with the outer row's columns in scope (SQLite's EP_VarSelect). The
// evaluator (`expr::evaluate`) has no access to the heavy `ExecContext`
// (pager, catalog, snapshots) needed to run a SELECT, so the statement
// executor installs a bridge into a thread-local before executing plans
// whose expressions may contain correlated subqueries:
//
//   api::query / api::execute   ──install──▶  CORR_STATE.ctx = *mut ExecContext
//        │
//        ▼
//   expr::evaluate(Expr::Subquery)  ──uses──▶ CORR_STATE.ctx + outer-scope stack
//        │   pushes (row, column_names) of the EvalContext being evaluated
//        ▼
//   exec_select_statement(subquery) ──column refs miss locally──▶ outer stack
//
// The outer-scope stack holds raw slices borrowed from the EvalContext
// that triggered the subquery — valid for the whole nested execution
// (it lives on the call stack above us). Frames are pushed/popped with a
// panic-safe guard; the bridge itself is cleared by a Drop guard at
// statement end, so a panic can never leave a dangling pointer behind.
// ============================================================================

mod corr {
    use super::{ExecContext, ExecResult};
    use crate::error::Result;
    use crate::sql::ast::SelectStatement;
    use crate::types::Value;
    use std::cell::RefCell;

    /// One outer-row scope: the (row, column_names) of the EvalContext
    /// whose expression is executing a correlated subquery.
    pub(crate) struct OuterFrame {
        pub(crate) row: *const [Value],
        pub(crate) names: *const [String],
    }

    impl OuterFrame {
        #[inline]
        fn row(&self) -> &[Value] {
            // SAFETY: the frame is pushed by `push_outer` from a live
            // `&EvalContext` and popped (innermost-first) before that
            // context can end. The borrow outlives every nested execution.
            unsafe { &*self.row }
        }
        #[inline]
        fn names(&self) -> &[String] {
            // SAFETY: as above.
            unsafe { &*self.names }
        }
    }

    struct CorrState {
        /// Statement's ExecContext — installed while a statement executes.
        /// SAFETY: only dereferenced while the installing guard is alive
        /// (guard Drop clears it before the ExecContext can be dropped).
        ctx: *mut ExecContext<'static>,
        depth: usize,
        outer: Vec<OuterFrame>,
    }

    thread_local! {
        static CORR: RefCell<CorrState> = RefCell::new(CorrState {
            ctx: std::ptr::null_mut(),
            depth: 0,
            outer: Vec::new(),
        });
    }

    /// Install the execution bridge for a statement. Nested installs (an
    /// API call inside a statement — rare) keep the outermost bridge.
    pub(crate) struct Guard {
        installed: bool,
    }

    impl Guard {
        pub(crate) fn install(ctx: *mut ExecContext<'_>) -> Guard {
            // SAFETY: the pointer is erased to 'static only for storage;
            // it is only dereferenced while `guard` is alive, which the
            // caller keeps on the same stack frame as the real borrow.
            let erased = ctx as *mut ExecContext<'static>;
            let installed = CORR.with(|c| {
                let mut c = c.borrow_mut();
                c.depth += 1;
                if c.depth == 1 {
                    c.ctx = erased;
                    true
                } else {
                    false
                }
            });
            Guard { installed }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            CORR.with(|c| {
                let mut c = c.borrow_mut();
                if self.installed {
                    c.ctx = std::ptr::null_mut();
                    c.outer.clear();
                }
                c.depth = c.depth.saturating_sub(1);
            });
        }
    }

    /// Push one outer scope while a correlated subquery executes.
    /// Returns a guard that pops it.
    struct FrameGuard;
    impl Drop for FrameGuard {
        fn drop(&mut self) {
            CORR.with(|c| {
                c.borrow_mut().outer.pop();
            });
        }
    }

    fn push_outer(row: *const [Value], names: *const [String]) -> FrameGuard {
        CORR.with(|c| {
            c.borrow_mut().outer.push(OuterFrame { row, names });
        });
        FrameGuard
    }

    /// Resolve a column against the outer-scope stack, innermost first.
    /// Mirrors `EvalContext::lookup_in_main`'s matching rules per frame:
    /// exact (case-insensitive) name, then suffix ("u.id" matches "id").
    /// Qualified refs try "qual.name" exactly in every frame FIRST, then
    /// fall back to bare-name matching — the outer frame's column list can
    /// be bare (the UPDATE/DELETE paths push unqualified names), and a
    /// `users.id` reference must resolve there, not to an inner table's
    /// same-named column.
    pub(crate) fn lookup_outer(table: Option<&str>, name: &str) -> Option<Value> {
        CORR.with(|c| {
            let c = c.borrow();
            if c.outer.is_empty() {
                return None;
            }
            if let Some(t) = table {
                // Pass 1: exact "qual.name" in each frame, innermost first.
                let qual_lower = format!("{}.{}", t.to_ascii_lowercase(), name.to_ascii_lowercase());
                for frame in c.outer.iter().rev() {
                    let names = frame.names();
                    for (i, n) in names.iter().enumerate() {
                        if n.to_ascii_lowercase() == qual_lower {
                            return frame.row().get(i).cloned();
                        }
                    }
                }
                // Pass 2: bare-name match (frame stores unqualified names —
                // UPDATE/DELETE source rows).
                for frame in c.outer.iter().rev() {
                    let names = frame.names();
                    for (i, n) in names.iter().enumerate() {
                        if n.eq_ignore_ascii_case(name) {
                            return frame.row().get(i).cloned();
                        }
                    }
                    for (i, n) in names.iter().enumerate() {
                        if let Some(pos) = n.rfind('.') {
                            if n[pos + 1..].eq_ignore_ascii_case(name) {
                                return frame.row().get(i).cloned();
                            }
                        }
                    }
                }
            } else {
                // Unqualified: exact, then suffix — same as local.
                for frame in c.outer.iter().rev() {
                    let names = frame.names();
                    for (i, n) in names.iter().enumerate() {
                        if n.eq_ignore_ascii_case(name) {
                            return frame.row().get(i).cloned();
                        }
                    }
                    for (i, n) in names.iter().enumerate() {
                        if let Some(pos) = n.rfind('.') {
                            if n[pos + 1..].eq_ignore_ascii_case(name) {
                                return frame.row().get(i).cloned();
                            }
                        }
                    }
                }
            }
            None
        })
    }

    /// Execute a correlated scalar subquery against the installed
    /// statement context, with the given EvalContext as the outer scope.
    pub(crate) fn exec_scalar(
        sel: &SelectStatement,
        outer_row: *const [Value],
        outer_names: *const [String],
    ) -> Result<Value> {
        let ctx_ptr = CORR.with(|c| c.borrow().ctx);
        if ctx_ptr.is_null() {
            return Err(crate::error::Error::Unsupported(
                "scalar subqueries via evaluator (use executor)",
            ));
        }
        let _frame = push_outer(outer_row, outer_names);
        // SAFETY: the pointer is valid for the lifetime of the installing
        // Guard, which our caller keeps alive above this frame.
        let res: ExecResult = unsafe { exec_select(sel, ctx_ptr)? };
        Ok(res
            .rows
            .first()
            .and_then(|r| r.first().cloned())
            .unwrap_or(Value::Null))
    }

    /// Execute a correlated EXISTS subquery.
    pub(crate) fn exec_exists(
        sel: &SelectStatement,
        outer_row: *const [Value],
        outer_names: *const [String],
    ) -> Result<Value> {
        let ctx_ptr = CORR.with(|c| c.borrow().ctx);
        if ctx_ptr.is_null() {
            return Err(crate::error::Error::Unsupported(
                "EXISTS via evaluator (use executor)",
            ));
        }
        let _frame = push_outer(outer_row, outer_names);
        // SAFETY: as in exec_scalar.
        let res: ExecResult = unsafe { exec_select(sel, ctx_ptr)? };
        Ok(Value::Integer(if res.rows.is_empty() { 0 } else { 1 }))
    }

    /// Execute a correlated IN-subquery and collect its first column.
    pub(crate) fn exec_in_list(
        sel: &SelectStatement,
        outer_row: *const [Value],
        outer_names: *const [String],
    ) -> Result<Vec<Value>> {
        let ctx_ptr = CORR.with(|c| c.borrow().ctx);
        if ctx_ptr.is_null() {
            return Err(crate::error::Error::Unsupported(
                "IN subquery via evaluator (use executor)",
            ));
        }
        let _frame = push_outer(outer_row, outer_names);
        // SAFETY: as in exec_scalar.
        let res: ExecResult = unsafe { exec_select(sel, ctx_ptr)? };
        Ok(res
            .rows
            .iter()
            .map(|r| r.first().cloned().unwrap_or(Value::Null))
            .collect())
    }

    /// SAFETY: caller must guarantee `ctx` is alive and not mutably
    /// aliased outside this call.
    unsafe fn exec_select(sel: &SelectStatement, ctx: *mut ExecContext<'static>) -> Result<ExecResult> {
        let ctx = unsafe { &mut *ctx };
        super::exec_select_statement(sel, ctx)
    }
}

pub(crate) use corr::Guard as CorrGuard;

/// Evaluator-facing wrapper: execute a correlated scalar subquery with the
/// given EvalContext as the outer scope.
pub(crate) fn corr_exec_scalar(
    sel: &SelectStatement,
    eval_ctx: &EvalContext<'_>,
) -> Result<Value> {
    corr::exec_scalar(sel, eval_ctx.row as *const [Value], eval_ctx.column_names as *const [String])
}

/// Evaluator-facing wrapper: correlated EXISTS.
pub(crate) fn corr_exec_exists(
    sel: &SelectStatement,
    eval_ctx: &EvalContext<'_>,
) -> Result<Value> {
    corr::exec_exists(sel, eval_ctx.row as *const [Value], eval_ctx.column_names as *const [String])
}

/// Evaluator-facing wrapper: correlated IN-subquery list.
pub(crate) fn corr_exec_in_list(
    sel: &SelectStatement,
    eval_ctx: &EvalContext<'_>,
) -> Result<Vec<Value>> {
    corr::exec_in_list(sel, eval_ctx.row as *const [Value], eval_ctx.column_names as *const [String])
}

/// Outer-scope lookup for a qualified column ref (unqualified refs go
/// through the `lookup_in_main` fallback instead).
pub(crate) fn corr_outer_qualified(table: &str, name: &str) -> Option<Value> {
    corr::lookup_outer(Some(table), name)
}

/// Outer-scope lookup for an unqualified column ref.
pub(crate) fn corr_outer_lookup(table: Option<&str>, name: &str) -> Option<Value> {
    corr::lookup_outer(table, name)
}


use crate::error::{Error, Result};
use crate::planner::plan::*;
use crate::schema::Table;
use crate::sql::ast::*;
use crate::storage::btree::{Btree, LookupResult};
use crate::storage::page::PageId;
use crate::storage::pager::Pager;
use crate::storage::row_codec::{decode_row, decode_row_into, decode_row_selective, encode_row_aliased, encode_row_aliased_into};
use crate::types::{Row, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Shared execution state.
/// Combined per-Database bookkeeping maps, shared with statements behind
/// a single `Arc` — ONE read-lock acquisition + ONE refcount bump per
/// query (previously three separate `RwLock<Arc<HashMap>>` fields:
/// ~3 locks + 3 atomic bumps + 3 throwaway `Arc::new` allocations per
/// query just to hand a statement its snapshots).
#[derive(Default, Clone)]
pub struct StmtMaps {
    /// table name (lowercase) -> current root page
    pub roots: HashMap<String, u32>,
    /// index name (lowercase) -> current root page
    pub index_roots: HashMap<String, u32>,
    /// table name (lowercase) -> cached max rowid
    pub max_rowids: HashMap<String, i64>,
}

impl StmtMaps {
    pub fn empty() -> Self {
        Self::default()
    }
}

pub struct ExecContext<'a> {
    pub pager: &'a Pager,
    /// Positional bound parameters (`?` placeholders), indexed 0..N.
    /// Pushed by `bind_positional`. This is the common case — virtually
    /// all real-world queries use `?` placeholders.
    ///
    /// Previously this was a `HashMap<String, Value>` keyed by the param
    /// name (e.g. "0", "1"). The HashMap allocated a bucket array on first
    /// insert (~200-500 ns per query) and required a hash + lookup per
    /// evaluation. The Vec is allocated once per query (cheap) and indexed
    /// by usize (single memory load).
    pub params: Vec<Value>,
    /// Named parameters (:name, @col, $var). Allocated lazily — empty for
    /// the 99% case of purely positional `?` placeholders.
    pub named_params: HashMap<String, Value>,
    pub last_insert_rowid: i64,
    pub changes: i64,
    /// When true (inside BEGIN..COMMIT), DML operators skip per-statement flushes.
    pub in_transaction: bool,
    /// Set when the last statement was a ROLLBACK — the caller (api.rs)
    /// must rebuild its persisted-root bookkeeping because root moves and
    /// schema-row rewrites from the transaction were discarded.
    pub rolled_back: bool,
    /// When true (Database::deferred_flush), DML operators skip per-statement
    /// flushes even outside an explicit transaction. Mirrors SQLite's WAL+
    /// synchronous=NORMAL behaviour. The caller (Database::execute) is
    /// responsible for forcing a flush on the next SELECT, on reaching the
    /// dirty-page threshold, or on an explicit `Database::flush()` call.
    pub deferred_flush: bool,
    /// Snapshot taken at BEGIN, used by ROLLBACK to restore the pager.
    pub txn_snapshot: Option<crate::storage::pager::PagerSnapshot>,
    /// Catalog snapshot taken before the statement started. Used to look up
    /// tables and indexes for DML. This is a raw pointer to avoid lifetime
    /// conflicts with the mutable catalog borrow in api.rs. The caller
    /// (api.rs) guarantees the catalog outlives the context.
    pub catalog_ptr: *const crate::schema::Catalog,
    /// LOCAL root-page overrides written by THIS statement (table_name ->
    /// current root page). Starts empty; merged into the Database's shared
    /// snapshot at statement end (see `shared`).
    pub root_overrides: HashMap<String, u32>,
    /// READ-ONLY snapshot of the Database's accumulated bookkeeping maps
    /// (table roots, index roots, max-rowid cache) behind ONE Arc.
    /// Lookups check the local overlays first, then these.
    pub shared: std::sync::Arc<StmtMaps>,
    /// LOCAL index-root overrides written by this statement.
    pub index_roots: HashMap<String, u32>,
    /// LOCAL max-rowid cache entries written by this statement.
    pub max_rowids: HashMap<String, i64>,
    /// Set when a table/index root actually MOVED this statement (B+tree
    /// split). Gates `sync_schema_roots` in api.rs — previously that call
    /// ran after EVERY statement, taking two read locks and collecting
    /// `Vec<(String, u32)>`s (with String clones) even when nothing moved.
    pub roots_changed: bool,
    /// Set when `max_rowids` gained local entries this statement (gates the
    /// write-back merge — root moves are rare, but the max-rowid scan cache
    /// populates on first INSERT and on first index-range heuristic use).
    pub max_rowids_changed: bool,
    /// Table-name keys whose cached max-rowid was INVALIDATED by a DELETE
    /// in this statement. The shared map merge is `extend` (adds/updates
    /// only), so removals must be recorded and replayed — otherwise the
    /// next INSERT on another statement reads a stale max and skips
    /// rowids (SQLite reuses rowids after the max row is deleted).
    pub max_rowids_invalidated: Vec<String>,
    /// Set when `index_roots` gained local entries this statement.
    pub index_roots_changed: bool,
    /// Pinned right-most leaf for sequential table appends, valid ONLY
    /// within the current statement (see insert_table_append_hinted). Keyed
    /// by the Arc<Table> identity so a hint never crosses tables.
    pub table_append_hint: Option<(usize, u32)>,
    /// Materialized CTEs of the statement being executed — consulted by
    /// subquery planning (exec_select_statement) so `IN (SELECT .. FROM cte)`
    /// inside a WITH statement resolves the CTE.
    pub ctes: Option<HashMap<String, (std::sync::Arc<Vec<crate::types::Row>>, std::sync::Arc<[String]>)>>,
    /// Current trigger-firing depth (guards runaway recursion).
    pub trigger_depth: u32,
    /// Marker to keep the lifetime.
    _marker: std::marker::PhantomData<&'a crate::schema::Catalog>,
}

impl<'a> ExecContext<'a> {
    pub fn new(pager: &'a Pager, catalog: *const crate::schema::Catalog) -> Self {
        Self {
            pager,
            params: Vec::new(),
            named_params: HashMap::new(),
            last_insert_rowid: 0,
            changes: 0,
            in_transaction: false,
            rolled_back: false,
            deferred_flush: false,
            txn_snapshot: None,
            catalog_ptr: catalog,
            root_overrides: HashMap::new(),
            shared: std::sync::Arc::new(StmtMaps::empty()),
            index_roots: HashMap::new(),
            max_rowids: HashMap::new(),
            roots_changed: false,
            max_rowids_changed: false,
            max_rowids_invalidated: Vec::new(),
            index_roots_changed: false,
            table_append_hint: None,
            ctes: None,
            trigger_depth: 0,
            _marker: std::marker::PhantomData,
        }
    }

    /// Zero-allocation constructor for reader (query) paths: takes the
    /// shared maps Arc by value (one refcount bump, paid by the caller)
    /// instead of allocating three throwaway empty `Arc<HashMap>`s that
    /// `new()` would create and immediately overwrite.
    pub fn new_reader(
        pager: &'a Pager,
        catalog: *const crate::schema::Catalog,
        shared: std::sync::Arc<StmtMaps>,
    ) -> Self {
        // Full struct literal — `..Self::new(..)` would allocate a
        // throwaway empty `Arc<StmtMaps>` just to overwrite it.
        Self {
            pager,
            params: Vec::new(),
            named_params: HashMap::new(),
            last_insert_rowid: 0,
            changes: 0,
            in_transaction: false,
            rolled_back: false,
            deferred_flush: false,
            txn_snapshot: None,
            catalog_ptr: catalog,
            root_overrides: HashMap::new(),
            shared,
            index_roots: HashMap::new(),
            max_rowids: HashMap::new(),
            roots_changed: false,
            max_rowids_changed: false,
            max_rowids_invalidated: Vec::new(),
            index_roots_changed: false,
            table_append_hint: None,
            ctes: None,
            trigger_depth: 0,
            _marker: std::marker::PhantomData,
        }
    }

    /// Get a reference to the catalog. Safe because the caller guarantees
    /// the catalog outlives the context and no mutable access happens during
    /// execution (DDL is handled separately).
    pub fn catalog(&self) -> &'a crate::schema::Catalog {
        unsafe { &*self.catalog_ptr }
    }

    /// Get the current root page for a table, checking overrides first.
    /// Zero-allocation fast path: map keys are stored lowercased and table
    /// names are usually already lowercase, so try the exact name first
    /// (borrowed lookup) and only pay the `to_ascii_lowercase()` String
    /// allocation for mixed-case names.
    pub fn table_root(&self, table: &Table) -> u32 {
        if let Some(&r) = self.root_overrides.get(&table.name) {
            return r;
        }
        if let Some(&r) = self.shared.roots.get(&table.name) {
            return r;
        }
        let lc = table.name.to_ascii_lowercase();
        if let Some(&r) = self.root_overrides.get(&lc) {
            return r;
        }
        self.shared.roots.get(&lc).copied().unwrap_or(table.root_page)
    }

    /// Update the root page override for a table.
    pub fn set_table_root(&mut self, table_name: &str, root: u32) {
        self.set_table_root_lc(&table_name.to_ascii_lowercase(), root);
    }

    /// Current root page of an index B+tree (override-aware). Same
    /// zero-allocation exact-name fast path as `table_root`.
    pub fn index_root(&self, index: &crate::schema::Index) -> u32 {
        if let Some(&r) = self.index_roots.get(&index.name) {
            return r;
        }
        if let Some(&r) = self.shared.index_roots.get(&index.name) {
            return r;
        }
        let lc = index.name.to_ascii_lowercase();
        if let Some(&r) = self.index_roots.get(&lc) {
            return r;
        }
        self.shared.index_roots.get(&lc).copied().unwrap_or(index.root_page)
    }

    /// Update the index root override (called after an index B+tree split).
    pub fn set_index_root(&mut self, index_name: &str, root: u32) {
        if self.index_roots.get(index_name) == Some(&root)
            || self.shared.index_roots.get(index_name) == Some(&root)
        {
            return;
        }
        let key = index_name.to_ascii_lowercase();
        if self.index_roots.get(&key) == Some(&root)
            || self.shared.index_roots.get(&key) == Some(&root)
        {
            return;
        }
        self.index_roots.insert(key, root);
        self.roots_changed = true;
        self.index_roots_changed = true;
    }

    /// Fast-path set_table_root for callers that have already lower-cased
    /// the table name (e.g. exec_insert hoists this out of the per-row
    /// loop). Avoids the per-call `to_ascii_lowercase()` String allocation,
    /// and — critically — does NOTHING (no HashMap write, no String
    /// allocation) when the root is unchanged, which is the common case:
    /// roots only move on B+tree splits. Previously this allocated a fresh
    /// key String + inserted into the map on EVERY inserted row.
    pub fn set_table_root_lc(&mut self, table_name_lc: &str, root: u32) {
        // Skip when the value already matches either the local overlay or
        // the shared snapshot (roots only move on B+tree splits).
        if self.root_overrides.get(table_name_lc) == Some(&root)
            || self.shared.roots.get(table_name_lc) == Some(&root)
        {
            return;
        }
        self.root_overrides.insert(table_name_lc.to_string(), root);
        self.roots_changed = true;
    }

    /// Get the cached max rowid for a table, or scan if not cached.
    /// Avoids the `to_ascii_lowercase()` String allocation on the fast
    /// path: table names are lowercased at cache-key time, and most table
    /// names in practice are already lowercase, so try the exact name
    /// first (borrowed lookup, zero allocation).
    pub fn get_or_scan_max_rowid(&mut self, table: &Table) -> Result<i64> {
        // Fast path 1: table name is already lowercase (the common case).
        if let Some(&max) = self.max_rowids.get(&table.name) {
            return Ok(max);
        }
        if let Some(&max) = self.shared.max_rowids.get(&table.name) {
            // Seed the local overlay so later set_max_rowid_lc calls in
            // this statement hit the in-place fast path.
            self.max_rowids.insert(table.name.clone(), max);
            self.max_rowids_changed = true;
            return Ok(max);
        }
        // Fast path 2: mixed-case name with a cached lowercase key.
        let key = table.name.to_ascii_lowercase();
        if let Some(&max) = self.max_rowids.get(&key) {
            return Ok(max);
        }
        if let Some(&max) = self.shared.max_rowids.get(&key) {
            self.max_rowids.insert(key, max);
            self.max_rowids_changed = true;
            return Ok(max);
        }
        let root = self.table_root(table);
        let max = find_max_rowid(self.pager, root)?;
        self.max_rowids.insert(key, max);
        self.max_rowids_changed = true;
        Ok(max)
    }

    /// Update the cached max rowid for a table.
    pub fn set_max_rowid(&mut self, table_name: &str, rowid: i64) {
        self.set_max_rowid_lc(&table_name.to_ascii_lowercase(), rowid);
    }

    /// Invalidate the cached max-rowid for a table (DELETE paths). Records
    /// the key so the statement-end merge removes it from the shared map
    /// too — `extend` alone would leave the stale value behind.
    pub fn invalidate_max_rowid(&mut self, table_name_lc: &str) {
        self.max_rowids.remove(table_name_lc);
        self.max_rowids_invalidated.push(table_name_lc.to_string());
        self.max_rowids_changed = true;
    }

    /// DELETE-path helper: invalidate the cached max-rowid when the
    /// statement deleted the rowid the cache is keyed on. The cache may
    /// live in the LOCAL overlay or only in the SHARED map (populated by
    /// an earlier statement) — check both, else the next INSERT reads the
    /// stale shared value and skips rowids.
    pub fn invalidate_max_rowid_if_deleted(&mut self, table_name_lc: &str, max_deleted: i64) {
        let local = self.max_rowids.get(table_name_lc).copied();
        let shared = self.shared.max_rowids.get(table_name_lc).copied();
        let cached = local.or(shared);
        if let Some(cached) = cached {
            if max_deleted >= cached {
                self.invalidate_max_rowid(table_name_lc);
            }
        }
    }

    /// Fast-path set_max_rowid for pre-lower-cased names. Updates the value
    /// in place when the key already exists (the common case —
    /// `get_or_scan_max_rowid` seeds it) instead of re-allocating the key
    /// String on every inserted row.
    pub fn set_max_rowid_lc(&mut self, table_name_lc: &str, rowid: i64) {
        // Fast path: local entry exists (seeded by get_or_scan_max_rowid's
        // copy-on-read, or written earlier in this statement) — update in
        // place. This is the INSERT hot path: the rowid always advances
        // the max, so no unchanged-check is worthwhile.
        if let Some(v) = self.max_rowids.get_mut(table_name_lc) {
            *v = rowid;
            self.max_rowids_changed = true;
            return;
        }
        if self.shared.max_rowids.get(table_name_lc) == Some(&rowid) {
            // Already at this value in the shared snapshot — nothing to do.
            return;
        }
        self.max_rowids.insert(table_name_lc.to_string(), rowid);
        self.max_rowids_changed = true;
    }

    /// Bind a positional parameter (the common `?` placeholder case).
    /// Pushes to the params Vec. Cheaper than `bind(name, value)` because
    /// it skips the String key allocation and HashMap insert.
    pub fn bind_positional(&mut self, value: Value) {
        self.params.push(value);
    }

    /// Bind a parameter by name. For numeric names (e.g. "0", "1"), pushes
    /// to the positional Vec at the parsed index (extending with Nulls if
    /// needed). For named params (:name, @col, $var), inserts into the
    /// named_params HashMap.
    ///
    /// Kept for backwards compatibility with existing callers that pass
    /// `&format!("{}", i)` as the name. New code should prefer
    /// `bind_positional` for the positional case.
    pub fn bind(&mut self, name: &str, value: Value) {
        if let Ok(idx) = name.parse::<usize>() {
            // Positional — extend the Vec with Nulls if needed.
            while self.params.len() <= idx {
                self.params.push(Value::Null);
            }
            self.params[idx] = value;
        } else {
            self.named_params.insert(name.to_string(), value);
        }
    }
}

/// Result of executing a plan: column names + rows.
pub struct ExecResult {
    /// Output column names. `Arc<[String]>` so base-table operators can
    /// return the cached names from `Table::col_names` / `qualified_col_names`
    /// with a single refcount bump — no per-query `String` allocations.
    pub columns: Arc<[String]>,
    pub rows: Vec<Row>,
}

impl ExecResult {
    pub fn empty() -> Self {
        Self { columns: Arc::from(Vec::new()), rows: Vec::new() }
    }
}

/// Execute a plan and return all rows.
/// Per-thread change counters backing the `changes()` / `total_changes()`
/// SQL functions (SQLite semantics: changes() = rows modified by the most
/// recent INSERT/UPDATE/DELETE on this connection; total_changes() = the
/// running sum). Thread-local because a Database handle is used from one
/// thread at a time (writers hold `&mut self`; readers never modify).
pub mod change_counters {
    use std::cell::Cell;
    thread_local! {
        static LAST: Cell<i64> = const { Cell::new(0) };
        static TOTAL: Cell<i64> = const { Cell::new(0) };
    }
    /// Record a completed statement's change count.
    pub fn record(changes: i64) {
        LAST.with(|c| c.set(changes));
        TOTAL.with(|t| t.set(t.get() + changes));
    }
    /// Rows modified by the most recent statement.
    pub fn last() -> i64 {
        LAST.with(|c| c.get())
    }
    /// Running total across all statements.
    pub fn total() -> i64 {
        TOTAL.with(|t| t.get())
    }
}

pub fn execute(plan: &Plan, ctx: &mut ExecContext<'_>) -> Result<ExecResult> {
    match plan {
        Plan::Scan { table, alias, .. } => exec_scan(ctx, table.clone(), alias.clone()),
        Plan::Values { rows } => exec_values(ctx, rows),
        Plan::Filter { input, predicate } => exec_filter(ctx, input, predicate),
        Plan::Project { input, columns } => {
            // FUSED PATH: Project over a Hash Join. The join can emit the
            // projected columns directly, skipping the intermediate
            // full-width combined rows (one Vec + copies per row) and the
            // second pass of value cloning. Falls back internally to
            // normal join + apply_projection when fusion isn't applicable
            // (non-column projections, residual predicates, ...).
            if let Plan::Join { left, right, join_type, condition, algorithm } = &**input {
                if *algorithm == crate::planner::plan::JoinAlgorithm::Hash {
                    return exec_hash_join(ctx, left, right, *join_type, condition, Some(columns));
                }
            }
            // FUSED PATH: Project over a RowidRange / RowidLookup with
            // bare-column projections — decode ONLY the projected columns
            // per row (skipping e.g. the rowid marker and un-referenced
            // wide text columns), with no second cloning pass.
            if let Plan::RowidRange { table, alias: _, start, end, residual: None } = &**input {
                if let Some((project, out_cols)) = bare_column_projection(columns, table) {
                    return exec_rowid_range_projected(ctx, table.clone(), start.as_ref(), end.as_ref(), project.as_deref(), out_cols);
                }
            }
            if let Plan::RowidLookup { table, rowid, .. } = &**input {
                if let Some((project, out_cols)) = bare_column_projection(columns, table) {
                    return exec_rowid_lookup_projected(ctx, table.clone(), rowid, project.as_deref(), out_cols);
                }
            }
            // FUSED PATH: Project over an Index Nested-Loop Join — the join
            // emits only the projected columns (no full-width combined row,
            // no second cloning pass). Mirrors the Hash Join fusion.
            if let Plan::IndexNestedLoopJoin { outer, inner_table, inner_alias, inner_index, outer_key_col } = &**input {
                return exec_index_nested_loop_join(ctx, outer, inner_table.clone(), inner_alias.clone(), inner_index.clone(), *outer_key_col, Some(columns));
            }
            exec_project(ctx, input, columns)
        }
        Plan::Sort { input, terms } => exec_sort(ctx, input, terms),
        Plan::Limit { input, count, offset } => exec_limit(ctx, input, count, offset),
        Plan::Aggregate { input, group_by, aggregates } => exec_aggregate(ctx, input, group_by, aggregates),
        Plan::Window { input, windows } => exec_window(ctx, input, windows),
        Plan::Join { left, right, join_type, condition, algorithm } => {
            if *algorithm == crate::planner::plan::JoinAlgorithm::Hash {
                exec_hash_join(ctx, left, right, *join_type, condition, None)
            } else {
                exec_join(ctx, left, right, *join_type, condition)
            }
        }
        Plan::IndexNestedLoopJoin { outer, inner_table, inner_alias, inner_index, outer_key_col } => {
            exec_index_nested_loop_join(ctx, outer, inner_table.clone(), inner_alias.clone(), inner_index.clone(), *outer_key_col, None)
        }
        Plan::Distinct { input } => exec_distinct(ctx, input),
        Plan::Union { left, right, all } => exec_union(ctx, left, right, *all),
        Plan::Intersect { left, right } => exec_intersect(ctx, left, right),
        Plan::Except { left, right } => exec_except(ctx, left, right),
        Plan::Subquery { plan } => execute(plan, ctx),
        Plan::CteRows { rows, columns } => Ok(ExecResult {
            columns: columns.clone(),
            rows: rows.as_ref().clone(),
        }),
        Plan::RowidLookup { table, rowid, .. } => exec_rowid_lookup(ctx, table.clone(), rowid),
        Plan::RowidRange { table, alias: _, start, end, residual } => exec_rowid_range(ctx, table.clone(), start.as_ref(), end.as_ref(), residual.as_ref()),
        Plan::IndexLookup { table, alias: _, index, key_exprs } => exec_index_lookup(ctx, table.clone(), index.clone(), key_exprs),
        Plan::IndexRange { table, alias, index, start, end, residual } => exec_index_range(ctx, table.clone(), alias.clone(), index.clone(), start.as_ref(), end.as_ref(), residual.as_ref()),
        Plan::Insert { table, source, columns, on_conflict, upsert, returning } => exec_insert(ctx, table.clone(), source, columns.clone(), *on_conflict, upsert.as_ref(), returning.as_deref()),
        Plan::Update { table, source, assignments, returning } => exec_update(ctx, table.clone(), source, assignments, returning.as_deref()),
        Plan::Delete { table, source, returning } => exec_delete(ctx, table.clone(), source, returning.as_deref()),
    }
}

// Helper: evaluate an expression against a single row.
fn eval_row(expr: &Expr, row: &[Value], col_names: &[String], params: &[Value], named_params: &HashMap<String, Value>) -> Result<Value> {
    let ctx = EvalContext::new(row, col_names, params, named_params);
    evaluate(expr, &ctx)
}

/// Crate-public wrapper around `eval_row` — used by api.rs for index
/// backfill (partial-index WHERE clause evaluation against existing rows).
pub(crate) fn eval_row_public(
    expr: &Expr,
    row: &[Value],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<Value> {
    eval_row(expr, row, col_names, params, named_params)
}

/// Enforce NOT NULL and CHECK constraints on a (new or updated) row.
/// Returns a semantic error naming the offending column / table.
///
/// `col_names` is only needed when the table has CHECK constraints
/// (NOT NULL checks work purely positionally) — pass an empty slice for
/// tables without CHECKs to avoid building the name Vec on hot paths.
fn enforce_row_constraints(
    table: &Table,
    row: &[Value],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<()> {
    for (i, col) in table.columns.iter().enumerate() {
        if !col.nullable && row.get(i).map(|v| v.is_null()).unwrap_or(true) {
            return Err(Error::semantic(format!(
                "NOT NULL constraint failed: {}.{}",
                table.name, col.name
            )));
        }
    }
    for expr in &table.check_exprs {
        let v = eval_row(expr, row, col_names, params, named_params)?;
        if !v.is_truthy() {
            return Err(Error::semantic(format!(
                "CHECK constraint failed: {}",
                table.name
            )));
        }
    }
    Ok(())
}

// ============================================================================
// FOREIGN KEY enforcement
// ============================================================================

/// Does a parent row exist for the given foreign key values?
///
/// Resolution order for the parent key columns:
/// 1. `fk.ref_columns` (explicit `REFERENCES p(c)`) — resolved on the parent.
/// 2. Empty `ref_columns` — the parent's PRIMARY KEY column(s).
///
/// Lookup strategy:
/// - single referenced column that is the parent's rowid alias → O(log N)
///   `lookup_table` point probe;
/// - otherwise → full table scan with an equality check (correct, and the
///   common case for small reference tables; an index-based probe can be
///   added when the parent has a matching unique index).
fn fk_parent_exists(
    ctx: &ExecContext<'_>,
    fk: &crate::schema::ForeignKeyClause,
    key_values: &[Value],
) -> Result<bool> {
    let catalog = ctx.catalog();
    let Some(parent) = catalog.get_table(&fk.ref_table) else {
        // Parent missing: SQLite rejects the DDL at definition time; if we
        // got here the schema is inconsistent — treat as violation.
        return Err(Error::semantic(format!(
            "FOREIGN KEY constraint failed: no such table: {}",
            fk.ref_table
        )));
    };
    // Resolve parent key column indices: explicit list, else PK columns.
    let parent_cols: Vec<usize> = if !fk.ref_columns.is_empty() {
        let mut v = Vec::with_capacity(fk.ref_columns.len());
        for rc in &fk.ref_columns {
            match parent.find_column(rc) {
                Some(i) => v.push(i),
                None => {
                    return Err(Error::semantic(format!(
                        "FOREIGN KEY constraint failed: no such column: {}.{}",
                        parent.name, rc
                    )))
                }
            }
        }
        v
    } else {
        let pk: Vec<usize> = parent
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect();
        if pk.is_empty() {
            // No PK on parent: SQLite requires a PK or unique index; the
            // rowid (implicit PK) is the fallback.
            vec![usize::MAX] // marker: compare against the rowid
        } else {
            pk
        }
    };

    let root = ctx.table_root(&parent);
    let n_cols = parent.n_columns();
    let alias = parent.rowid_alias;

    // Fast path: single referenced column == parent rowid alias → point probe.
    if parent_cols.len() == 1 && parent_cols[0] != usize::MAX && alias == Some(parent_cols[0]) {
        let rowid = key_values[0].as_integer();
        let mut bt = Btree::new(ctx.pager, root, false);
        return Ok(matches!(bt.lookup_table(rowid)?, LookupResult::Found(_)));
    }

    // General path: scan the parent, comparing the key columns.
    let rowid_marker = parent_cols.len() == 1 && parent_cols[0] == usize::MAX;
    let mut found = false;
    let mut buf: Vec<Value> = Vec::with_capacity(n_cols);
    let mut bt = Btree::new(ctx.pager, root, false);
    bt.scan_table_borrowed(|rowid, payload| {
        buf.clear();
        if decode_row_into(payload, n_cols, rowid, alias, &mut buf).is_err() {
            return true;
        }
        let matched = if rowid_marker {
            key_values.len() == 1 && Value::Integer(rowid) == key_values[0]
        } else {
            parent_cols
                .iter()
                .zip(key_values.iter())
                .all(|(&pc, kv)| buf.get(pc).map(|v| v == kv).unwrap_or(false))
        };
        if matched {
            found = true;
            false // stop
        } else {
            true
        }
    })?;
    Ok(found)
}

/// Child-side enforcement: every FK of `table` must reference an existing
/// parent row (NULL child keys pass — SQL MATCH SIMPLE semantics, same as
/// SQLite's default). Called from INSERT and UPDATE before the write lands.
fn enforce_child_fks(ctx: &ExecContext<'_>, table: &Table, row: &[Value]) -> Result<()> {
    if table.foreign_keys.is_empty() || !ctx.pager.foreign_keys_enabled() {
        return Ok(());
    }
    for fk in &table.foreign_keys {
        let key_values: Vec<Value> =
            fk.columns.iter().map(|&i| row.get(i).cloned().unwrap_or(Value::Null)).collect();
        // MATCH SIMPLE: any NULL in the key → constraint satisfied.
        if key_values.iter().any(|v| v.is_null()) {
            continue;
        }
        if !fk_parent_exists(ctx, fk, &key_values)? {
            let cols: Vec<String> = fk.columns.iter().map(|&i| table.columns[i].name.clone()).collect();
            return Err(Error::semantic(format!(
                "FOREIGN KEY constraint failed: {}.{} -> {}",
                table.name,
                cols.join(", "),
                fk.ref_table
            )));
        }
    }
    Ok(())
}

/// Find child rows referencing `old_row`'s key through `fk` (a clause of
/// `child_table` that references `parent_table`). Returns (rowid, key-values)
/// pairs for each referencing child row.
fn fk_find_child_rows(
    ctx: &ExecContext<'_>,
    child_table: &Table,
    fk: &crate::schema::ForeignKeyClause,
    parent_col_idxs: &[usize],
    old_row: &[Value],
    parent_rowid: i64,
) -> Result<Vec<(i64, Vec<Value>)>> {
    let parent_key: Vec<Value> = if parent_col_idxs.is_empty() {
        vec![Value::Integer(parent_rowid)]
    } else {
        parent_col_idxs
            .iter()
            .map(|&i| old_row.get(i).cloned().unwrap_or(Value::Null))
            .collect()
    };
    let root = ctx.table_root(child_table);
    let n_cols = child_table.n_columns();
    let alias = child_table.rowid_alias;
    let mut hits: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut buf: Vec<Value> = Vec::with_capacity(n_cols);
    let mut bt = Btree::new(ctx.pager, root, false);
    bt.scan_table_borrowed(|rowid, payload| {
        buf.clear();
        if decode_row_into(payload, n_cols, rowid, alias, &mut buf).is_err() {
            return true;
        }
        let matched = fk
            .columns
            .iter()
            .zip(parent_key.iter())
            .all(|(&ci, pv)| buf.get(ci).map(|v| v == pv).unwrap_or(false));
        if matched {
            hits.push((rowid, buf.clone()));
        }
        true
    })?;
    Ok(hits)
}

/// Parent-side enforcement for a DELETE (or a parent-key UPDATE that moves
/// the key away): apply the ON DELETE action of every referencing FK.
///
/// - NoAction / Restrict: reject if any child references the key.
/// - Cascade: delete the referencing children (recursively — children may
///   themselves be parents).
/// - SetNull / SetDefault: rewrite the referencing children's key columns.
fn enforce_parent_delete_fks(
    ctx: &mut ExecContext<'_>,
    parent: &Table,
    old_row: &[Value],
    parent_rowid: i64,
    depth: usize,
) -> Result<()> {
    if !ctx.pager.foreign_keys_enabled() || depth > 16 {
        return Ok(());
    }
    // Every table whose FK references `parent` (case-insensitive).
    // (Collected first: the loop below mutates ctx, and `catalog()` borrows
    // it immutably — collecting the Arc clones up-front detaches the borrow.)
    let referencing: Vec<(Arc<Table>, Vec<crate::schema::ForeignKeyClause>)> = ctx
        .catalog()
        .all_tables()
        .into_iter()
        .filter(|(_, t)| !t.name.eq_ignore_ascii_case(&parent.name) && !t.foreign_keys.is_empty())
        .filter(|(_, t)| {
            t.foreign_keys.iter().any(|fk| fk.ref_table.eq_ignore_ascii_case(&parent.name))
        })
        .map(|(_, t)| {
            let fks: Vec<crate::schema::ForeignKeyClause> = t
                .foreign_keys
                .iter()
                .filter(|fk| fk.ref_table.eq_ignore_ascii_case(&parent.name))
                .cloned()
                .collect();
            (t, fks)
        })
        .collect();
    for (child, fks) in referencing {
        for fk in &fks {
            // Resolve the parent-side columns this FK points at.
            let parent_cols: Vec<usize> = if !fk.ref_columns.is_empty() {
                let mut v = Vec::with_capacity(fk.ref_columns.len());
                for rc in &fk.ref_columns {
                    if let Some(i) = parent.find_column(rc) {
                        v.push(i);
                    }
                }
                v
            } else {
                parent
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key)
                    .map(|(i, _)| i)
                    .collect()
            };
            // Does this FK point at the parent's PRIMARY KEY columns being
            // deleted? When the FK targets specific columns, only enforce
            // when those columns are part of the parent key set — otherwise
            // (FK on a non-key column) it's still a reference we must check.
            let children = fk_find_child_rows(
                ctx, &child, fk, &parent_cols, old_row, parent_rowid,
            )?;
            if children.is_empty() {
                continue;
            }
            match fk.on_delete {
                ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                    return Err(Error::semantic(format!(
                        "FOREIGN KEY constraint failed: {} rows reference {}",
                        child.name, parent.name
                    )));
                }
                ForeignKeyAction::Cascade => {
                    // Recursively delete each child row (children may be
                    // parents themselves — nested FKs cascade too).
                    for (rowid, crow) in children {
                        let child2 = child.clone();
                        enforce_parent_delete_fks(ctx, &child2, &crow, rowid, depth + 1)?;
                        let root = ctx.table_root(&child2);
                        let new_root;
                        {
                            let mut bt = Btree::new(ctx.pager, root, false);
                            bt.delete_table(rowid)?;
                            new_root = bt.root;
                        }
                        let lc = child2.name.to_ascii_lowercase();
                        ctx.set_table_root_lc(&lc, new_root);
                        // Index maintenance for the deleted child row.
                        let indexes = ctx.catalog().indexes_on_table(&child2.name);
                        for idx in indexes {
                            delete_index_entry(ctx, &idx, &child2, &crow, rowid)?;
                        }
                        ctx.changes += 1;
                        if let Some(&cached) = ctx.max_rowids.get(&lc) {
                            if rowid >= cached {
                                ctx.invalidate_max_rowid(&lc);
                            }
                        }
                    }
                }
                ForeignKeyAction::SetNull | ForeignKeyAction::SetDefault => {
                    // Rewrite each child row's FK columns to NULL / defaults.
                    for (rowid, mut crow) in children {
                        for &ci in &fk.columns {
                            crow[ci] = if fk.on_delete == ForeignKeyAction::SetNull {
                                Value::Null
                            } else {
                                let default_val = child.columns[ci].default.as_ref().map(|e| {
                                    let names: Vec<String> =
                                        child.columns.iter().map(|c| c.name.clone()).collect();
                                    eval_row(e, &crow, &names, &ctx.params, &ctx.named_params)
                                        .unwrap_or(Value::Null)
                                });
                                default_val.unwrap_or(Value::Null)
                            };
                        }
                        let payload = encode_row_aliased(&crow, child.rowid_alias);
                        let root = ctx.table_root(&child);
                        let new_root;
                        {
                            let mut bt = Btree::new(ctx.pager, root, false);
                            let updated = bt.update_table(rowid, &payload).unwrap_or(false);
                            if !updated {
                                bt.delete_table(rowid)?;
                                bt.insert_table(rowid, &payload)?;
                            }
                            new_root = bt.root;
                        }
                        ctx.set_table_root_lc(&child.name.to_ascii_lowercase(), new_root);
                        let indexes = ctx.catalog().indexes_on_table(&child.name);
                        for idx in indexes {
                            delete_index_entry(ctx, &idx, &child, &crow, rowid)?;
                        }
                        ctx.changes += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Project the RETURNING clause for one affected row.
/// `Star` expands to all columns; `Expr` is evaluated against the row.
fn project_returning_row(
    returning: &[crate::sql::ast::ResultColumn],
    row: &[Value],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(returning.len());
    for rc in returning {
        match rc {
            crate::sql::ast::ResultColumn::Star => out.extend_from_slice(row),
            crate::sql::ast::ResultColumn::TableStar(_) => out.extend_from_slice(row),
            crate::sql::ast::ResultColumn::Expr { expr, .. } => {
                out.push(eval_row(expr, row, col_names, params, named_params)?);
            }
        }
    }
    Ok(out)
}

/// Column names for a RETURNING result (used for `query_with_columns`).
fn returning_column_names(
    returning: &[crate::sql::ast::ResultColumn],
    col_names: &[String],
) -> Vec<String> {
    let mut out = Vec::with_capacity(returning.len());
    for rc in returning {
        match rc {
            crate::sql::ast::ResultColumn::Star => out.extend_from_slice(col_names),
            crate::sql::ast::ResultColumn::TableStar(_) => out.extend_from_slice(col_names),
            crate::sql::ast::ResultColumn::Expr { expr, alias } => {
                if let Some(a) = alias {
                    out.push(a.clone());
                } else {
                    out.push(expr_display_name(expr));
                }
            }
        }
    }
    out
}

// ============================================================================
// Subquery substitution (uncorrelated scalar / IN / EXISTS subqueries)
// ============================================================================

/// Does this expression tree contain any subquery expression nodes?
pub fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::Exists(_) => true,
        Expr::In { source: InSource::Subquery(_), .. } => true,
        _ => {
            let mut found = false;
            map_expr_children(e, &mut |child| {
                if expr_has_subquery(child) {
                    found = true;
                }
                Ok(child.clone())
            })
            .ok();
            found
        }
    }
}

/// Apply a function to each immediate child expression of `e`, rebuilding
/// the node with the (possibly replaced) children.
fn map_expr_children(e: &Expr, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Expr> {
    macro_rules! r {
        ($x:expr) => {
            f($x)?
        };
    }
    Ok(match e {
        Expr::Literal(v) => Expr::Literal(v.clone()),
        Expr::Parameter(p) => Expr::Parameter(p.clone()),
        Expr::Column { table, name } => Expr::Column { table: table.clone(), name: name.clone() },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(r!(left)),
            right: Box::new(r!(right)),
        },
        Expr::Unary { op, expr } => Expr::Unary { op: *op, expr: Box::new(r!(expr)) },
        Expr::Between { expr, low, high, negated } => Expr::Between {
            expr: Box::new(r!(expr)),
            low: Box::new(r!(low)),
            high: Box::new(r!(high)),
            negated: *negated,
        },
        Expr::In { expr, source, negated } => {
            let new_source = match source {
                InSource::List(list) => {
                    let mut new_list = Vec::with_capacity(list.len());
                    for item in list {
                        new_list.push(r!(item));
                    }
                    InSource::List(new_list)
                }
                other => other.clone(),
            };
            Expr::In { expr: Box::new(r!(expr)), source: new_source, negated: *negated }
        }
        Expr::Like { op, expr, pattern, escape, negated } => {
            let new_escape = match escape.as_ref() {
                Some(es) => Some(Box::new(r!(es.as_ref()))),
                None => None,
            };
            Expr::Like {
                op: *op,
                expr: Box::new(r!(expr)),
                pattern: Box::new(r!(pattern)),
                escape: new_escape,
                negated: *negated,
            }
        }
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: Box::new(r!(expr)), negated: *negated },
        Expr::Is { left, right, negated } => Expr::Is {
            left: Box::new(r!(left)),
            right: Box::new(r!(right)),
            negated: *negated,
        },
        Expr::Function { name, distinct, args, filter, over } => {
            let mut new_args = Vec::with_capacity(args.len());
            for a in args {
                new_args.push(r!(a));
            }
            let new_filter = match filter.as_ref() {
                Some(ftr) => Some(Box::new(r!(ftr.as_ref()))),
                None => None,
            };
            Expr::Function {
                name: name.clone(),
                distinct: *distinct,
                args: new_args,
                filter: new_filter,
                over: over.clone(),
            }
        }
        Expr::Case { operand, whens, else_ } => {
            let new_operand = match operand.as_ref() {
                Some(o) => Some(Box::new(r!(o.as_ref()))),
                None => None,
            };
            let mut new_whens = Vec::with_capacity(whens.len());
            for (w, v) in whens {
                new_whens.push((r!(w), r!(v)));
            }
            let new_else = match else_.as_ref() {
                Some(el) => Some(Box::new(r!(el.as_ref()))),
                None => None,
            };
            Expr::Case { operand: new_operand, whens: new_whens, else_: new_else }
        }
        Expr::Row(items) => {
            let mut new_items = Vec::with_capacity(items.len());
            for i in items {
                new_items.push(r!(i));
            }
            Expr::Row(new_items)
        }
        Expr::Subquery(sel) => Expr::Subquery(sel.clone()),
        Expr::Exists(sel) => Expr::Exists(sel.clone()),
        Expr::Cast { expr, type_name } => Expr::Cast { expr: Box::new(r!(expr)), type_name: type_name.clone() },
        Expr::Collate { expr, collation } => Expr::Collate { expr: Box::new(r!(expr)), collation: collation.clone() },
        Expr::Raise { action, message } => {
            let new_message = match message.as_ref() {
                Some(m) => Some(Box::new(r!(m.as_ref()))),
                None => None,
            };
            Expr::Raise {
                action: *action,
                message: new_message,
            }
        }
    })
}

/// Rewrite all UNCORRELATED subquery expressions in a plan into literals /
/// IN-lists by executing them once (mirrors SQLite's OP_Once caching).
/// Correlated subqueries are left untouched (they error at eval time).
pub fn rewrite_plan_subqueries(plan: &Plan, ctx: &mut ExecContext<'_>) -> Result<Plan> {
    let mut new_plan = plan.clone();
    rewrite_plan_subqueries_in_place(&mut new_plan, ctx)?;
    Ok(new_plan)
}

/// Quick scan: does this plan contain any subquery expressions?
pub fn plan_has_subqueries(plan: &Plan) -> bool {
    fn exprs_in_plan<'a>(p: &'a Plan, out: &mut Vec<&'a Expr>) {
        match p {
            Plan::CteRows { .. } => {}
            Plan::Scan { predicate, .. } => {
                if let Some(e) = predicate.as_ref() {
                    out.push(e);
                }
            }
            Plan::RowidLookup { rowid, .. } => out.push(rowid),
            Plan::RowidRange { start, end, residual, .. } => {
                if let Some(s) = start.as_ref() {
                    out.push(s);
                }
                if let Some(e) = end.as_ref() {
                    out.push(e);
                }
                if let Some(r) = residual.as_ref() {
                    out.push(r);
                }
            }
            Plan::IndexLookup { key_exprs, .. } => {
                for k in key_exprs {
                    out.push(k);
                }
            }
            Plan::IndexRange { start, end, residual, .. } => {
                if let Some((s, _)) = start.as_ref() {
                    out.push(s);
                }
                if let Some((e, _)) = end.as_ref() {
                    out.push(e);
                }
                if let Some(r) = residual.as_ref() {
                    out.push(r);
                }
            }
            Plan::Values { rows } => {
                for row in rows {
                    for e in row {
                        out.push(e);
                    }
                }
            }
            Plan::Filter { input, predicate } => {
                out.push(predicate);
                exprs_in_plan(input, out);
            }
            Plan::Project { input, columns } => {
                for c in columns {
                    out.push(&c.expr);
                }
                exprs_in_plan(input, out);
            }
            Plan::Sort { input, terms } => {
                for t in terms {
                    out.push(&t.expr);
                }
                exprs_in_plan(input, out);
            }
            Plan::Limit { input, count, offset } => {
                out.push(count);
                out.push(offset);
                exprs_in_plan(input, out);
            }
            Plan::Aggregate { input, group_by, aggregates } => {
                for g in group_by {
                    out.push(g);
                }
                for a in aggregates {
                    if let Some(arg) = a.arg.as_ref() {
                        out.push(arg);
                    }
                }
                exprs_in_plan(input, out);
            }
            Plan::Window { input, windows } => {
                for w in windows {
                    if let Some(arg) = w.arg.as_ref() {
                        out.push(arg);
                    }
                    for p in &w.partition_by {
                        out.push(p);
                    }
                    for t in &w.order_by {
                        out.push(&t.expr);
                    }
                }
                exprs_in_plan(input, out);
            }
            Plan::Join { left, right, condition, .. } => {
                if let Some(c) = condition.as_ref() {
                    out.push(c);
                }
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::IndexNestedLoopJoin { outer, .. } => exprs_in_plan(outer, out),
            Plan::Subquery { plan } => exprs_in_plan(plan, out),
            Plan::Distinct { input } => exprs_in_plan(input, out),
            Plan::Union { left, right, .. } => {
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::Intersect { left, right } => {
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::Except { left, right } => {
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::Insert { source, .. } => exprs_in_plan(source, out),
            Plan::Update { source, assignments, .. } => {
                for (_i, e) in assignments {
                    out.push(e);
                }
                exprs_in_plan(source, out);
            }
            Plan::Delete { source, .. } => exprs_in_plan(source, out),
        }
    }
    let mut exprs = Vec::new();
    exprs_in_plan(plan, &mut exprs);
    exprs.iter().any(|e| expr_has_subquery(e))
}

fn rewrite_plan_subqueries_in_place(plan: &mut Plan, ctx: &mut ExecContext<'_>) -> Result<()> {
    match plan {
        Plan::CteRows { .. } => {}
        Plan::Scan { predicate, .. } => {
            if let Some(p) = predicate.as_mut() {
                rewrite_expr_in_place(p, ctx)?;
            }
        }
        Plan::RowidLookup { rowid, .. } => {
            rewrite_expr_in_place(rowid, ctx)?;
        }
        Plan::RowidRange { start, end, residual, .. } => {
            if let Some(s) = start.as_mut() {
                rewrite_expr_in_place(s, ctx)?;
            }
            if let Some(e) = end.as_mut() {
                rewrite_expr_in_place(e, ctx)?;
            }
            if let Some(r) = residual.as_mut() {
                rewrite_expr_in_place(r, ctx)?;
            }
        }
        Plan::IndexLookup { key_exprs, .. } => {
            for k in key_exprs.iter_mut() {
                rewrite_expr_in_place(k, ctx)?;
            }
        }
        Plan::IndexRange { start, end, residual, .. } => {
            if let Some((s, _)) = start.as_mut() {
                rewrite_expr_in_place(s, ctx)?;
            }
            if let Some((e, _)) = end.as_mut() {
                rewrite_expr_in_place(e, ctx)?;
            }
            if let Some(r) = residual.as_mut() {
                rewrite_expr_in_place(r, ctx)?;
            }
        }
        Plan::Values { rows } => {
            for row in rows.iter_mut() {
                for e in row.iter_mut() {
                    rewrite_expr_in_place(e, ctx)?;
                }
            }
        }
        Plan::Filter { input, predicate } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            rewrite_expr_in_place(predicate, ctx)?;
        }
        Plan::Project { input, columns } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for c in columns.iter_mut() {
                rewrite_expr_in_place(&mut c.expr, ctx)?;
            }
        }
        Plan::Sort { input, terms } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for t in terms.iter_mut() {
                rewrite_expr_in_place(&mut t.expr, ctx)?;
            }
        }
        Plan::Limit { input, count, offset } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            rewrite_expr_in_place(count, ctx)?;
            rewrite_expr_in_place(offset, ctx)?;
        }
        Plan::Aggregate { input, group_by, aggregates } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for g in group_by.iter_mut() {
                rewrite_expr_in_place(g, ctx)?;
            }
            for a in aggregates.iter_mut() {
                if let Some(arg) = a.arg.as_mut() {
                    rewrite_expr_in_place(arg, ctx)?;
                }
            }
        }
        Plan::Window { input, windows } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for w in windows.iter_mut() {
                if let Some(arg) = w.arg.as_mut() {
                    rewrite_expr_in_place(arg, ctx)?;
                }
                for p in w.partition_by.iter_mut() {
                    rewrite_expr_in_place(p, ctx)?;
                }
                for t in w.order_by.iter_mut() {
                    rewrite_expr_in_place(&mut t.expr, ctx)?;
                }
            }
        }
        Plan::Join { left, right, condition, .. } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
            if let Some(c) = condition.as_mut() {
                rewrite_expr_in_place(c, ctx)?;
            }
        }
        Plan::IndexNestedLoopJoin { outer, .. } => {
            rewrite_plan_subqueries_in_place(outer, ctx)?;
        }
        Plan::Subquery { plan } => {
            rewrite_plan_subqueries_in_place(plan, ctx)?;
        }
        Plan::Distinct { input } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
        }
        Plan::Union { left, right, .. } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
        }
        Plan::Intersect { left, right } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
        }
        Plan::Except { left, right } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
        }
        Plan::Insert { source, .. } => {
            rewrite_plan_subqueries_in_place(source, ctx)?;
        }
        Plan::Update { source, assignments, .. } => {
            rewrite_plan_subqueries_in_place(source, ctx)?;
            for (_idx, e) in assignments.iter_mut() {
                rewrite_expr_in_place(e, ctx)?;
            }
        }
        Plan::Delete { source, .. } => {
            rewrite_plan_subqueries_in_place(source, ctx)?;
        }
    }
    Ok(())
}

/// Rewrite subqueries inside a single expression, in place.
fn rewrite_expr_in_place(expr: &mut Expr, ctx: &mut ExecContext<'_>) -> Result<()> {
    if !expr_has_subquery(expr) {
        return Ok(());
    }
    let rewritten = rewrite_subqueries_rec(expr, ctx)?;
    *expr = rewritten;
    Ok(())
}

/// Bottom-up rewrite of subquery nodes in an expression.
fn rewrite_subqueries_rec(e: &Expr, ctx: &mut ExecContext<'_>) -> Result<Expr> {
    // First, rewrite children.
    let e = map_expr_children(e, &mut |child| rewrite_subqueries_rec(child, ctx))?;
    // Then, handle this node if it is a subquery node.
    match e {
        Expr::Subquery(sel) => {
            // Rewrite any NESTED subqueries inside this subquery first.
            let mut sel = *sel;
            rewrite_select_subqueries(&mut sel, ctx)?;
            let (cte_names, cte_cols) = cte_scope_of(ctx);
            if subquery_is_correlated(&sel, ctx.catalog(), &cte_names, &cte_cols) {
                Ok(Expr::Subquery(Box::new(sel)))
            } else {
                let res = exec_select_statement(&sel, ctx)?;
                let v = res
                    .rows
                    .first()
                    .and_then(|r| r.first().cloned())
                    .unwrap_or(Value::Null);
                Ok(Expr::Literal(v))
            }
        }
        Expr::Exists(sel) => {
            let mut sel = *sel;
            rewrite_select_subqueries(&mut sel, ctx)?;
            let (cte_names, cte_cols) = cte_scope_of(ctx);
            if subquery_is_correlated(&sel, ctx.catalog(), &cte_names, &cte_cols) {
                Ok(Expr::Exists(Box::new(sel)))
            } else {
                let res = exec_select_statement(&sel, ctx)?;
                Ok(Expr::Literal(Value::Integer(if res.rows.is_empty() { 0 } else { 1 })))
            }
        }
        Expr::In { expr, source, negated } => {
            let inner = match source {
                InSource::Subquery(sel) => {
                    let mut sel = *sel;
                    rewrite_select_subqueries(&mut sel, ctx)?;
                    let (cte_names, cte_cols) = cte_scope_of(ctx);
                    if subquery_is_correlated(&sel, ctx.catalog(), &cte_names, &cte_cols) {
                        InSource::Subquery(Box::new(sel))
                    } else {
                        let res = exec_select_statement(&sel, ctx)?;
                        let list: Vec<Expr> = res
                            .rows
                            .iter()
                            .map(|r| Expr::Literal(r.first().cloned().unwrap_or(Value::Null)))
                            .collect();
                        InSource::List(list)
                    }
                }
                other => other,
            };
            Ok(Expr::In { expr, source: inner, negated })
        }
        other => Ok(other),
    }
}

/// Rewrite subquery expressions inside a SELECT statement (its WHERE,
/// projection, HAVING, GROUP BY, ORDER BY, LIMIT, join conditions —
/// including nested subqueries).
fn rewrite_select_subqueries(sel: &mut SelectStatement, ctx: &mut ExecContext<'_>) -> Result<()> {
    rewrite_body_subqueries(&mut sel.body, ctx)?;
    for t in sel.order_by.iter_mut() {
        rewrite_expr_in_place(&mut t.expr, ctx)?;
    }
    if let Some(l) = sel.limit.as_mut() {
        rewrite_expr_in_place(l, ctx)?;
    }
    if let Some(o) = sel.offset.as_mut() {
        rewrite_expr_in_place(o, ctx)?;
    }
    Ok(())
}

fn rewrite_body_subqueries(body: &mut SelectBody, ctx: &mut ExecContext<'_>) -> Result<()> {
    match body {
        SelectBody::Simple(s) => {
            for c in s.columns.iter_mut() {
                if let ResultColumn::Expr { expr, .. } = c {
                    rewrite_expr_in_place(expr, ctx)?;
                }
            }
            if let Some(w) = s.where_clause.as_mut() {
                rewrite_expr_in_place(w, ctx)?;
            }
            for g in s.group_by.iter_mut() {
                rewrite_expr_in_place(g, ctx)?;
            }
            if let Some(h) = s.having.as_mut() {
                rewrite_expr_in_place(h, ctx)?;
            }
            if let Some(from) = s.from.as_mut() {
                rewrite_table_expression_subqueries(from, ctx)?;
            }
        }
        SelectBody::Binary { left, right, .. } => {
            rewrite_body_subqueries(left, ctx)?;
            rewrite_body_subqueries(right, ctx)?;
        }
    }
    Ok(())
}

fn rewrite_table_expression_subqueries(te: &mut TableExpression, ctx: &mut ExecContext<'_>) -> Result<()> {
    match te {
        TableExpression::Table { .. } => {}
        TableExpression::Subquery { select, .. } => {
            rewrite_select_subqueries(select, ctx)?;
        }
        TableExpression::Join { left, right, constraint, .. } => {
            rewrite_table_expression_subqueries(left, ctx)?;
            rewrite_table_expression_subqueries(right, ctx)?;
            if let JoinConstraint::On(e) = constraint {
                rewrite_expr_in_place(e, ctx)?;
            }
        }
    }
    Ok(())
}

/// Plan + execute a SELECT statement (used by subquery substitution).
fn exec_select_statement(sel: &SelectStatement, ctx: &mut ExecContext<'_>) -> Result<ExecResult> {
    let catalog = ctx.catalog();
    let mut planner = crate::planner::Planner::new(catalog);
    if let Some(ctes) = ctx.ctes.clone() {
        planner.set_ctes(ctes);
    }
    let plan = planner.plan_select(sel)?;
    execute(&plan, ctx)
}

/// Conservative correlated-subquery detector: collect every column ref in
/// the subquery (including nested subqueries) and every source name (also
/// including nested sources). If any ref's qualifier isn't a local source,
/// or any unqualified ref doesn't match a local source column, treat the
/// subquery as correlated (outer refs present).
fn subquery_is_correlated(
    sel: &SelectStatement,
    catalog: &crate::schema::Catalog,
    cte_names: &std::collections::HashSet<String>,
    cte_cols: &HashMap<String, std::sync::Arc<[String]>>,
) -> bool {
    let mut sources: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut source_tables: Vec<Arc<Table>> = Vec::new();
    collect_select_sources(sel, catalog, &mut sources, &mut source_tables);
    let refs = collect_select_refs(sel);
    for (qual, name) in refs {
        if let Some(q) = qual {
            if !sources.contains(&q.to_ascii_lowercase()) {
                return true;
            }
        } else {
            // Unqualified: must match a local source column or a CTE column
            // in scope (CTEs aren't catalog tables — without this check,
            // every ref to a CTE column was misclassified as an outer
            // reference, marking uncorrelated subqueries "correlated").
            let known_table = source_tables.iter().any(|t| t.find_column(&name).is_some());
            let known_cte = cte_names.iter().any(|cn| {
                cte_cols
                    .get(cn)
                    .map(|cols| {
                        cols.iter().any(|c| {
                            let suffix = c.rsplit('.').next().unwrap_or(c);
                            suffix.eq_ignore_ascii_case(&name)
                        })
                    })
                    .unwrap_or(false)
            });
            if !known_table && !known_cte {
                return true;
            }
        }
    }
    false
}

/// CTE scope info extracted from an ExecContext for correlation analysis.
fn cte_scope_of(
    ctx: &ExecContext<'_>,
) -> (std::collections::HashSet<String>, HashMap<String, std::sync::Arc<[String]>>) {
    let mut names = std::collections::HashSet::new();
    let mut cols = HashMap::new();
    if let Some(ctes) = &ctx.ctes {
        for (k, (_, c)) in ctes {
            names.insert(k.clone());
            cols.insert(k.clone(), c.clone());
        }
    }
    (names, cols)
}

/// Collect source aliases/table names from a SELECT's FROM clause,
/// including join trees and nested subqueries-in-FROM.
fn collect_select_sources(
    sel: &SelectStatement,
    catalog: &crate::schema::Catalog,
    sources: &mut std::collections::HashSet<String>,
    source_tables: &mut Vec<Arc<Table>>,
) {
    collect_body_sources(&sel.body, catalog, sources, source_tables);
    // Nested subqueries inside expressions of this SELECT also bring their
    // own sources into scope for their own refs — collect them too so a
    // ref to a nested alias isn't misclassified as outer.
    let mut refs_and_subs: Vec<&SelectStatement> = Vec::new();
    collect_nested_selects(sel, &mut refs_and_subs);
    for s in refs_and_subs {
        collect_body_sources(&s.body, catalog, sources, source_tables);
    }
}

fn collect_body_sources(
    body: &SelectBody,
    catalog: &crate::schema::Catalog,
    sources: &mut std::collections::HashSet<String>,
    source_tables: &mut Vec<Arc<Table>>,
) {
    match body {
        SelectBody::Simple(s) => {
            if let Some(from) = &s.from {
                collect_table_expression_sources(from, catalog, sources, source_tables);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_sources(left, catalog, sources, source_tables);
            collect_body_sources(right, catalog, sources, source_tables);
        }
    }
}

fn collect_table_expression_sources(
    te: &TableExpression,
    catalog: &crate::schema::Catalog,
    sources: &mut std::collections::HashSet<String>,
    source_tables: &mut Vec<Arc<Table>>,
) {
    match te {
        TableExpression::Table { name, alias, .. } => {
            sources.insert(
                alias
                    .clone()
                    .unwrap_or_else(|| name.clone())
                    .to_ascii_lowercase(),
            );
            if let Some(t) = catalog.get_table(name) {
                source_tables.push(t);
            }
        }
        TableExpression::Subquery { alias, select, .. } => {
            if let Some(a) = alias {
                sources.insert(a.to_ascii_lowercase());
            }
            // The derived table's columns come from its own SELECT —
            // collect its output names so unqualified refs can match.
            collect_body_sources(&select.body, catalog, sources, source_tables);
        }
        TableExpression::Join { left, right, .. } => {
            collect_table_expression_sources(left, catalog, sources, source_tables);
            collect_table_expression_sources(right, catalog, sources, source_tables);
        }
    }
}

/// Collect every column reference in a SELECT statement (all clauses,
/// including nested subqueries).
fn collect_select_refs(sel: &SelectStatement) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    collect_body_refs(&sel.body, &mut out);
    for t in &sel.order_by {
        collect_expr_refs(&t.expr, &mut out);
    }
    if let Some(l) = &sel.limit {
        collect_expr_refs(l, &mut out);
    }
    if let Some(o) = &sel.offset {
        collect_expr_refs(o, &mut out);
    }
    out
}

fn collect_body_refs(body: &SelectBody, out: &mut Vec<(Option<String>, String)>) {
    match body {
        SelectBody::Simple(s) => {
            for c in &s.columns {
                if let ResultColumn::Expr { expr, .. } = c {
                    collect_expr_refs(expr, out);
                }
            }
            if let Some(w) = &s.where_clause {
                collect_expr_refs(w, out);
            }
            for g in &s.group_by {
                collect_expr_refs(g, out);
            }
            if let Some(h) = &s.having {
                collect_expr_refs(h, out);
            }
            if let Some(from) = &s.from {
                collect_table_expression_refs(from, out);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_refs(left, out);
            collect_body_refs(right, out);
        }
    }
}

fn collect_table_expression_refs(te: &TableExpression, out: &mut Vec<(Option<String>, String)>) {
    match te {
        TableExpression::Table { .. } => {}
        TableExpression::Subquery { select, .. } => {
            collect_body_refs(&select.body, out);
            for t in &select.order_by {
                collect_expr_refs(&t.expr, out);
            }
            if let Some(l) = &select.limit {
                collect_expr_refs(l, out);
            }
        }
        TableExpression::Join { left, right, constraint, .. } => {
            collect_table_expression_refs(left, out);
            collect_table_expression_refs(right, out);
            if let JoinConstraint::On(e) = constraint {
                collect_expr_refs(e, out);
            }
        }
    }
}

fn collect_expr_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => {
            out.push((table.clone(), name.clone()));
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_refs(left, out);
            collect_expr_refs(right, out);
        }
        Expr::Unary { expr, .. } => collect_expr_refs(expr, out),
        Expr::Between { expr, low, high, .. } => {
            collect_expr_refs(expr, out);
            collect_expr_refs(low, out);
            collect_expr_refs(high, out);
        }
        Expr::In { expr, source, .. } => {
            collect_expr_refs(expr, out);
            match source {
                InSource::List(list) => {
                    for item in list {
                        collect_expr_refs(item, out);
                    }
                }
                InSource::Subquery(sel) => {
                    collect_body_refs(&sel.body, out);
                }
                InSource::Table(_) => {}
            }
        }
        Expr::Like { expr, pattern, escape, .. } => {
            collect_expr_refs(expr, out);
            collect_expr_refs(pattern, out);
            if let Some(es) = escape {
                collect_expr_refs(es, out);
            }
        }
        Expr::IsNull { expr, .. } => collect_expr_refs(expr, out),
        Expr::Is { left, right, .. } => {
            collect_expr_refs(left, out);
            collect_expr_refs(right, out);
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                collect_expr_refs(a, out);
            }
            if let Some(f) = filter {
                collect_expr_refs(f, out);
            }
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand {
                collect_expr_refs(o, out);
            }
            for (w, v) in whens {
                collect_expr_refs(w, out);
                collect_expr_refs(v, out);
            }
            if let Some(e) = else_ {
                collect_expr_refs(e, out);
            }
        }
        Expr::Row(items) => {
            for i in items {
                collect_expr_refs(i, out);
            }
        }
        Expr::Subquery(sel) => {
            collect_body_refs(&sel.body, out);
        }
        Expr::Exists(sel) => {
            collect_body_refs(&sel.body, out);
        }
        Expr::Cast { expr, .. } => collect_expr_refs(expr, out),
        Expr::Collate { expr, .. } => collect_expr_refs(expr, out),
        Expr::Raise { message, .. } => {
            if let Some(m) = message {
                collect_expr_refs(m, out);
            }
        }
        _ => {}
    }
}

/// Collect nested SELECT statements inside a SELECT's expressions.
fn collect_nested_selects<'a>(sel: &'a SelectStatement, out: &mut Vec<&'a SelectStatement>) {
    let mut body_stack: Vec<&'a SelectBody> = vec![&sel.body];
    while let Some(body) = body_stack.pop() {
        match body {
            SelectBody::Simple(s) => {
                for c in &s.columns {
                    if let ResultColumn::Expr { expr, .. } = c {
                        collect_expr_selects(expr, out);
                    }
                }
                if let Some(w) = &s.where_clause {
                    collect_expr_selects(w, out);
                }
                for g in &s.group_by {
                    collect_expr_selects(g, out);
                }
                if let Some(h) = &s.having {
                    collect_expr_selects(h, out);
                }
            }
            SelectBody::Binary { left, right, .. } => {
                body_stack.push(left);
                body_stack.push(right);
            }
        }
    }
}

fn collect_expr_selects<'a>(e: &'a Expr, out: &mut Vec<&'a SelectStatement>) {
    match e {
        Expr::Subquery(sel) | Expr::Exists(sel) => {
            out.push(sel);
            collect_nested_selects(sel, out);
        }
        Expr::In { source, .. } => {
            if let InSource::Subquery(sel) = source {
                out.push(sel);
                collect_nested_selects(sel, out);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_selects(left, out);
            collect_expr_selects(right, out);
        }
        Expr::Unary { expr, .. } => collect_expr_selects(expr, out),
        Expr::Function { args, .. } => {
            for a in args {
                collect_expr_selects(a, out);
            }
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand {
                collect_expr_selects(o, out);
            }
            for (w, v) in whens {
                collect_expr_selects(w, out);
                collect_expr_selects(v, out);
            }
            if let Some(e) = else_ {
                collect_expr_selects(e, out);
            }
        }
        _ => {}
    }
}

// ============================================================================
// Scan
// ============================================================================

fn exec_scan(ctx: &mut ExecContext<'_>, table: Arc<Table>, alias: Option<String>) -> Result<ExecResult> {
    let mut rows = Vec::new();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let rowid_alias = table.rowid_alias;
    let n_cols = table.n_columns();
    bt.scan_table_borrowed(|rowid, payload| {
        if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
            rows.push(row);
        }
        true
    })?;
    // Column names: if there's an alias, prefix with "alias." so qualified
    // references in the planner/evaluator can find them. We also include
    // the unqualified name for backward compat.
    //
    // FAST PATH: when the effective prefix equals the table name (no alias,
    // or an alias identical to the table), return the cached
    // `qualified_col_names` built once in `build_table` — one refcount bump
    // instead of N `format!()` allocations per query.
    let prefix = alias.as_deref().unwrap_or(&table.name);
    let columns: Arc<[String]> = if prefix == table.name {
        table.qualified_col_names.clone()
    } else {
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect::<Vec<String>>()
            .into()
    };
    Ok(ExecResult {
        columns,
        rows,
    })
}

// ============================================================================
// Values
// ============================================================================

fn exec_values(ctx: &mut ExecContext<'_>, rows: &[Vec<Expr>]) -> Result<ExecResult> {
    let n = rows.first().map(|r| r.len()).unwrap_or(0);
    let columns: Arc<[String]> = (0..n)
        .map(|i| format!("column{}", i + 1))
        .collect::<Vec<String>>()
        .into();
    let mut out = Vec::with_capacity(rows.len());
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    for exprs in rows {
        let mut row = Vec::with_capacity(exprs.len());
        for e in exprs {
            row.push(evaluate(e, &EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params))?);
        }
        out.push(row);
    }
    Ok(ExecResult { columns, rows: out })
}

// ============================================================================
// Filter
// ============================================================================

fn exec_filter(ctx: &mut ExecContext<'_>, input: &Plan, predicate: &Expr) -> Result<ExecResult> {
    // FUSED PATH: Filter over a bare table Scan with a compilable predicate.
    // Scans with the predicate evaluated positionally (compiled once) and
    // skips materializing rows that fail it — non-matching rows cost a
    // payload decode and nothing else (no Vec<Value>, no clone, no move).
    if let Plan::Scan { table, alias, index: None, predicate: None } = input {
        let prefix = alias.as_deref().unwrap_or(&table.name);
        if let Some(pred) = crate::executor::predicate::compile_predicate(predicate, table, prefix) {
            let n_cols = table.n_columns();
            let root = ctx.table_root(table);
            let mut bt = Btree::new(ctx.pager, root, false);
            let rowid_alias = table.rowid_alias;
            let params: &[Value] = &ctx.params;
            // Identity positions: rows are decoded in full table order.
            let positions: Vec<usize> = (0..n_cols).collect();
            let mut rows = Vec::new();
            bt.scan_table_borrowed(|rowid, payload| {
                // Decode the full row (the Filter's output must expose all
                // columns for the parent operators).
                if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
                    if pred.eval(&row, &positions, params) {
                        rows.push(row);
                    }
                }
                true
            })?;
            let columns: Arc<[String]> = if prefix == table.name {
                table.qualified_col_names.clone()
            } else {
                table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect::<Vec<String>>().into()
            };
            return Ok(ExecResult { columns, rows });
        }
    }

    let inner = execute(input, ctx)?;
    // Compiled-predicate eval on the materialized rows (identity positions
    // against the input's column order) — avoids the per-row AST walk and
    // name lookups of eval_row when the predicate shape is supported.
    if let Plan::Scan { table, alias, .. } = input {
        let prefix = alias.as_deref().unwrap_or(&table.name);
        if let Some(pred) = crate::executor::predicate::compile_predicate(predicate, table, prefix) {
            let n_cols = table.n_columns();
            let positions: Vec<usize> = (0..n_cols).collect();
            let params: &[Value] = &ctx.params;
            let mut rows = Vec::new();
            for row in inner.rows {
                if pred.eval(&row, &positions, params) {
                    rows.push(row);
                }
            }
            return Ok(ExecResult { columns: inner.columns, rows });
        }
    }
    let mut rows = Vec::new();
    for row in inner.rows {
        let v = eval_row(predicate, &row, &inner.columns, &ctx.params, &ctx.named_params)?;
        if v.is_truthy() {
            rows.push(row);
        }
    }
    Ok(ExecResult { columns: inner.columns, rows })
}

// ============================================================================
// Project
// ============================================================================

fn exec_project(ctx: &mut ExecContext<'_>, input: &Plan, columns: &[ProjectExpr]) -> Result<ExecResult> {
    let inner = execute(input, ctx)?;
    apply_projection(inner, columns, ctx)
}

/// Apply a projection to an ALREADY-EXECUTED input. Split out of
/// `exec_project` so the fused hash-join path can fall back to normal
/// projection semantics when its fusion preconditions don't hold.
fn apply_projection(inner: ExecResult, columns: &[ProjectExpr], ctx: &ExecContext<'_>) -> Result<ExecResult> {
    // Compute output columns, expanding `*` and `table.*` to the underlying
    // input column names.
    let mut out_columns: Vec<String> = Vec::new();
    let mut star_expansions: Vec<Vec<String>> = Vec::new(); // for each column, the list of expanded names (or empty)
    for c in columns {
        if let Expr::Column { name, .. } = &c.expr {
            if name == "*" {
                // Expand to all input columns.
                let expanded: Vec<String> = inner.columns.iter().map(|c| {
                    if let Some(pos) = c.rfind('.') {
                        c[pos + 1..].to_string()
                    } else {
                        c.clone()
                    }
                }).collect();
                out_columns.extend(expanded.clone());
                star_expansions.push(expanded);
                continue;
            }
        }
        let display = if let Some(a) = &c.alias {
            a.clone()
        } else {
            expr_display_name(&c.expr)
        };
        out_columns.push(display);
        star_expansions.push(Vec::new());
    }

    // ---- FAST PATH: pre-resolve bare column references to row indices ----
    //
    // For the overwhelmingly common `SELECT a, b, c FROM ...` shape, every
    // projected expression is a plain `Expr::Column`. Resolving each one
    // through `eval_row` → `EvalContext::lookup` costs a linear scan with
    // case-insensitive string compares PER ROW PER COLUMN (~50-150 ns).
    // We resolve each column to its index in `inner.columns` ONCE, then
    // per row it's a Vec index + a Value clone (~2-5 ns).
    //
    // The resolution order exactly mirrors `EvalContext::lookup`:
    // qualified match first (when the ref carries a table qualifier), then
    // unqualified exact match, then suffix match on "prefix.name" columns.
    let resolved: Vec<Option<usize>> = columns
        .iter()
        .map(|c| match &c.expr {
            Expr::Column { table, name } if name != "*" => {
                resolve_column_index(&inner.columns, table.as_deref(), name)
            }
            _ => None,
        })
        .collect();
    let all_resolved = resolved.iter().all(|r| r.is_some())
        && columns.iter().all(|c| !matches!(&c.expr, Expr::Column { name, .. } if name == "*"));

    // ---- IDENTITY FAST PATH ----
    // `SELECT *`, `SELECT t.*`-style star projections, or any projection
    // that selects every input column in order produce rows identical to
    // the input rows. Move them instead of cloning every value into fresh
    // Vecs — for a 1000-row scan that's 1000 allocations + 2000+ Value
    // clones eliminated. Only the column NAMES can differ (aliases / star
    // unqualification); the values are byte-identical.
    let single_star = columns.len() == 1
        && matches!(&columns[0].expr, Expr::Column { name, .. } if name == "*");
    let identity_projection = single_star
        || (all_resolved
            && resolved.len() == inner.columns.len()
            && resolved.iter().enumerate().all(|(i, r)| r == &Some(i)));
    if identity_projection {
        return Ok(ExecResult { columns: out_columns.into(), rows: inner.rows });
    }

    let mut out_rows = Vec::with_capacity(inner.rows.len());
    if all_resolved {
        for row in &inner.rows {
            let mut out = Vec::with_capacity(out_columns.len());
            for r in &resolved {
                // unwrap is safe: all_resolved guarantees Some
                out.push(row[r.unwrap()].clone());
            }
            out_rows.push(out);
        }
    } else {
        for row in &inner.rows {
            let mut out = Vec::with_capacity(out_columns.len());
            for (i, c) in columns.iter().enumerate() {
                if let Some(Some(idx)) = resolved.get(i) {
                    out.push(row[*idx].clone());
                    continue;
                }
                if let Expr::Column { name, .. } = &c.expr {
                    if name == "*" {
                        out.extend(row.iter().cloned());
                        continue;
                    }
                }
                out.push(eval_row(&c.expr, row, &inner.columns, &ctx.params, &ctx.named_params)?);
            }
            out_rows.push(out);
        }
    }
    Ok(ExecResult { columns: out_columns.into(), rows: out_rows })
}

/// Resolve a column reference to an index in `col_names`, mirroring
/// `EvalContext::lookup`'s resolution order:
///
/// 1. Qualified (`Some(table)`): look for `"table.column"` (case-insensitive),
///    falling back to the unqualified rules.
/// 2. Unqualified: exact case-insensitive match, then suffix match on
///    `"prefix.name"` columns (e.g. ref `id` matches column `u.id`).
///
/// Returns `None` when the reference can't be statically resolved (caller
/// falls back to the general evaluator, which yields NULL for unknown cols).
fn resolve_column_index(
    col_names: &[String],
    table: Option<&str>,
    name: &str,
) -> Option<usize> {
    if let Some(t) = table {
        let tl = t.to_ascii_lowercase();
        let nl = name.to_ascii_lowercase();
        // Qualified exact match: "table.column".
        for (i, n) in col_names.iter().enumerate() {
            if n.len() == tl.len() + 1 + nl.len() {
                let n_lower = n.to_ascii_lowercase();
                if n_lower.as_str().get(tl.len()..tl.len() + 1) == Some(".")
                    && n_lower[..tl.len()] == tl
                    && &n_lower[tl.len() + 1..] == &nl
                {
                    return Some(i);
                }
            }
        }
        // Fall through to unqualified resolution (mirrors lookup()).
    }
    // Exact match first.
    for (i, n) in col_names.iter().enumerate() {
        if n.eq_ignore_ascii_case(name) {
            return Some(i);
        }
    }
    // Qualified match by suffix (e.g. "u.id" matches ref "id").
    for (i, n) in col_names.iter().enumerate() {
        if let Some(pos) = n.rfind('.') {
            let suffix = &n[pos + 1..];
            if suffix.eq_ignore_ascii_case(name) {
                return Some(i);
            }
        }
    }
    None
}

/// Pretty-print an expression for column header display.
/// For aggregate references (rewritten to `__agg_N`) the original function
/// name is not recoverable at this point — the Project's caller should
/// supply an alias.
fn expr_display_name(e: &Expr) -> String {
    match e {
        Expr::Column { table: _, name } => {
            if name.starts_with("__agg_") {
                // Look up the actual column name from the input — but the input
                // column for an aggregate is also __agg_N. We can't recover the
                // original function name here without more context. The Project's
                // caller should supply an alias.
                name.clone()
            } else if let Some(pos) = name.rfind('.') {
                name[pos + 1..].to_string()
            } else {
                name.clone()
            }
        }
        Expr::Literal(v) => format!("{}", v),
        Expr::Function { name, .. } => format!("{}(...)", name),
        _ => "?".to_string(),
    }
}

// ============================================================================
// Sort
// ============================================================================

fn exec_sort(ctx: &mut ExecContext<'_>, input: &Plan, terms: &[OrderTerm]) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    // Borrow params directly from ctx — `inner` is a local value, and the
    // sort comparator only reads it, so no clone (1-2 allocs) is needed.
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let columns = inner.columns.clone();
    inner.rows.sort_by(|a, b| {
        for term in terms {
            let va = eval_row(&term.expr, a, &columns, params, named_params).unwrap_or(Value::Null);
            let vb = eval_row(&term.expr, b, &columns, params, named_params).unwrap_or(Value::Null);
            let ord = va.cmp(&vb);
            let ord = if term.order == Order::Desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(inner)
}

// ============================================================================
// Limit
// ============================================================================

fn exec_limit(ctx: &mut ExecContext<'_>, input: &Plan, count: &Expr, offset: &Expr) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let count_val = evaluate(count, &eval_ctx)?.as_integer();
    let offset_val = evaluate(offset, &eval_ctx)?.as_integer();
    let offset = offset_val.max(0) as usize;
    if offset >= inner.rows.len() {
        inner.rows.clear();
    } else {
        inner.rows.drain(0..offset);
        if count_val >= 0 {
            let count = count_val as usize;
            if inner.rows.len() > count {
                inner.rows.truncate(count);
            }
        }
    }
    Ok(inner)
}

// ============================================================================
// Aggregate
// ============================================================================

#[derive(Clone, Debug)]
struct AggState {
    count: i64,
    sum: f64,
    sum_is_int: bool,
    int_sum: i64,
    min: Option<Value>,
    max: Option<Value>,
    distinct: std::collections::HashSet<SqlValueKey>,
    concat: String,
    seen_value: bool,
}

/// A single SQL value wrapped for use as a HashSet key with SQL equality
/// semantics (numeric cross-type equality). Replaces the old `format!("{:?}")`
/// String keys in DISTINCT aggregates — one Value clone (free for Int/Real)
/// instead of a Debug-format + heap String per row.
#[derive(Clone, Debug)]
struct SqlValueKey(Value);

impl PartialEq for SqlValueKey {
    fn eq(&self, other: &Self) -> bool {
        crate::types::values_sql_equal(&self.0, &other.0)
    }
}
impl Eq for SqlValueKey {}
impl std::hash::Hash for SqlValueKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        hash_sql_value(&self.0, state);
    }
}

/// Hash one value with SQL grouping semantics: integral REALs collide with
/// INTEGERs, -0.0 collides with 0, NULL has its own tag.
fn hash_sql_value<H: std::hash::Hasher>(v: &Value, state: &mut H) {
    match v {
        Value::Null => state.write_u8(0),
        Value::Integer(i) => {
            state.write_u8(1);
            state.write_i64(*i);
        }
        Value::Real(f) => {
            if f.is_finite() && *f == f.trunc() && f.abs() <= 9.007_199_254_740_992e15 {
                state.write_u8(1);
                state.write_i64(*f as i64);
            } else {
                state.write_u8(2);
                state.write_u64(f.to_bits());
            }
        }
        Value::Text(s) => {
            state.write_u8(3);
            s.hash(state);
        }
        Value::Blob(b) => {
            state.write_u8(4);
            b.hash(state);
        }
    }
}

/// The aggregate function, resolved once per query from the lowercased name.
/// Replaces per-row `&str` matching in `update_agg_state` (the planner
/// lowercases names, so this only needs the lowercase forms).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AggFunc {
    Count,
    Sum,
    Total,
    Avg,
    Min,
    Max,
    GroupConcat,
    Other,
}

impl AggFunc {
    fn from_name(name: &str) -> Self {
        match name {
            "count" => AggFunc::Count,
            "sum" => AggFunc::Sum,
            "total" => AggFunc::Total,
            "avg" => AggFunc::Avg,
            "min" => AggFunc::Min,
            "max" => AggFunc::Max,
            "group_concat" => AggFunc::GroupConcat,
            _ => AggFunc::Other,
        }
    }
}

/// Zero-allocation hash grouper for GROUP BY: buckets of group indices
/// keyed by the SQL-semantic hash of the group key, with linear probing
/// inside a bucket using SQL value equality.
///
/// Replaces the previous scheme — `format!("{:?}")` per key value +
/// `join("|")` per row + `HashMap<String, _>` + a separate `group_order`
/// Vec of cloned Strings — which cost 2-4 heap allocations and several
/// hundred ns PER ROW. This costs one hash per row and zero allocations
/// after warmup (each bucket Vec amortizes), and preserves first-seen
/// group order exactly like the old implementation.
struct HashGrouper {
    buckets: HashMap<u64, Vec<usize>>,
    /// (key values, per-aggregate states) in first-seen order.
    groups: Vec<(Vec<Value>, Vec<AggState>)>,
}

impl Default for HashGrouper {
    fn default() -> Self {
        Self { buckets: HashMap::new(), groups: Vec::new() }
    }
}

impl HashGrouper {
    /// Find or create the group for `key`, returning its index.
    /// `key` is typically a reusable scratch buffer — its contents are
    /// cloned only when a NEW group is created.
    fn intern(&mut self, key: &[Value]) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for v in key {
            hash_sql_value(v, &mut hasher);
        }
        let h = hasher.finish();
        if let Some(bucket) = self.buckets.get(&h) {
            for &gi in bucket {
                let (existing, _) = &self.groups[gi];
                if existing.len() == key.len()
                    && existing
                        .iter()
                        .zip(key.iter())
                        .all(|(a, b)| crate::types::values_sql_equal(a, b))
                {
                    return gi;
                }
            }
        }
        // New group.
        let gi = self.groups.len();
        self.groups.push((key.to_vec(), Vec::new()));
        self.buckets.entry(h).or_default().push(gi);
        gi
    }

    /// Number of distinct groups so far.
    fn len(&self) -> usize {
        self.groups.len()
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

impl Default for AggState {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            sum_is_int: true,  // Optimistic: assume int until we see a Real.
            int_sum: 0,
            min: None,
            max: None,
            distinct: std::collections::HashSet::new(),
            concat: String::new(),
            seen_value: false,
        }
    }
}

/// Streaming-aggregate fast path: when the input plan is a bare `Scan`
/// (no Filter, no Project, no Join), iterate the B+tree directly and
/// decode each row into a single reusable buffer.
///
/// This avoids materializing `Vec<Vec<Value>>` in `exec_scan` — which
/// for a 10k-row table is 10k `Vec<Value>` allocations + 10k+ `Value`
/// allocations (one per Text/Blob column per row). The reusable buffer
/// means we allocate ONCE for the row Vec and reuse the inner `Value`s
/// by overwriting them (which is free for Integer/Real/Null; for
/// Text/Blob the old Value gets dropped and the new one allocated
/// in-place, same total allocation count as exec_scan but no per-row
/// Vec allocation overhead).
///
/// If `filter_predicate` is `Some(pred)`, applies the predicate inline
/// (skipping rows that don't match) — handles the
/// `SELECT COUNT(*) FROM t WHERE x > 0` pattern without materializing.
///
/// Returns the same `ExecResult` shape as `exec_aggregate` so the rest
/// of the executor pipeline (Sort, Project, Limit) doesn't notice.
fn exec_aggregate_streaming_scan(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    filter_predicate: Option<&Expr>,
    group_by: &[Expr],
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    // Fast path: no GROUP BY. We can skip the per-row key computation,
    // the per-row key String formatting, and the HashMap lookup. There is
    // exactly one group, so we accumulate directly into a Vec<AggState>.
    //
    // Additionally, if all aggregate args are bare Column references
    // (the common case for `SELECT SUM(col), MAX(col), COUNT(*) FROM t`),
    // we resolve the column index ONCE upfront and read directly from the
    // row buffer at that index — no per-row `eval_row` call (which does a
    // name lookup, type coercion, etc.).
    if group_by.is_empty() {
        return exec_aggregate_no_group_by(ctx, table, alias, filter_predicate, aggregates);
    }

    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let n_cols = table.n_columns();
    let prefix = alias.as_deref().unwrap_or(&table.name);
    // Qualified column names ("t.col") — needed for eval_row calls on the
    // slow path (filter predicates / non-column group keys / non-column
    // aggregate args). The fast path avoids them entirely.
    let columns: Vec<String> = table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect();

    // ---- Vectorized setup: resolve everything ONCE before the scan ----
    //
    // 1. Group-by expressions → column indices (when they're bare Column
    //    refs — the overwhelmingly common `GROUP BY cat` case).
    // 2. Aggregate args → column indices (same).
    // 3. Aggregate function names → AggFunc enums (no per-row &str match).
    // 4. When there's no filter predicate AND everything resolved, we can
    //    use `decode_row_selective` to decode ONLY the referenced columns,
    //    skipping the decode of all other columns per row.
    let key_col_indices: Vec<Option<usize>> = group_by
        .iter()
        .map(|e| match e {
            Expr::Column { table: ref_t, name } => {
                let matches = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
                if matches {
                    table.find_column(name)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();
    let agg_col_indices: Vec<Option<usize>> = aggregates
        .iter()
        .map(|agg| match &agg.arg {
            Some(Expr::Column { table: ref_t, name }) => {
                let matches = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
                if matches {
                    table.find_column(name)
                } else {
                    None
                }
            }
            _ => None, // COUNT(*) or a non-column arg
        })
        .collect();
    let agg_funcs: Vec<AggFunc> = aggregates.iter().map(|a| AggFunc::from_name(&a.func)).collect();

    let all_resolved = key_col_indices.iter().all(|x| x.is_some())
        && agg_col_indices.iter().all(|x| x.is_some());
    let selective_eligible = all_resolved && filter_predicate.is_none();

    // Sorted, deduped list of column indices to decode on the selective path.
    let wanted: Vec<usize> = if selective_eligible {
        let mut w: Vec<usize> = key_col_indices.iter().filter_map(|x| *x).collect();
        w.extend(agg_col_indices.iter().filter_map(|x| *x));
        w.sort_unstable();
        w.dedup();
        w
    } else {
        Vec::new()
    };

    let mut grouper = HashGrouper::default();
    let n_aggs = aggregates.len();

    // Scratch buffers, reused across rows.
    let mut key_buf: Vec<Value> = Vec::with_capacity(group_by.len());
    let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
    let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len().max(1));

    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);

    if selective_eligible {
        // ==== Fully vectorized path: selective decode + direct indexing ====
        let rowid_alias = table.rowid_alias;
        bt.scan_table_borrowed(|rowid, payload| {
            if decode_row_selective(payload, n_cols, &wanted, rowid, rowid_alias, &mut sel_buf).is_err() {
                return true; // skip corrupt rows
            }
            // Build the group key from the decoded slice. Map each group-by
            // column index to its position in `wanted` (both sides are
            // precomputed; the find is a tiny scan over a few entries).
            key_buf.clear();
            for kidx in key_col_indices.iter().map(|x| x.unwrap()) {
                let pos = wanted.iter().position(|x| *x == kidx).unwrap_or(usize::MAX);
                key_buf.push(if pos != usize::MAX && pos < sel_buf.len() {
                    sel_buf[pos].clone()
                } else {
                    Value::Null
                });
            }
            let gi = grouper.intern(&key_buf);
            if grouper.groups[gi].1.is_empty() {
                grouper.groups[gi].1 = (0..n_aggs).map(|_| AggState::default()).collect();
            }
            for (i, agg) in aggregates.iter().enumerate() {
                let arg_val = match agg_col_indices[i] {
                    Some(aidx) => {
                        let pos = wanted.iter().position(|x| *x == aidx).unwrap_or(usize::MAX);
                        if pos != usize::MAX && pos < sel_buf.len() {
                            sel_buf[pos].clone()
                        } else {
                            Value::Null
                        }
                    }
                    None => Value::Integer(1), // COUNT(*)
                };
                update_agg_state(&mut grouper.groups[gi].1[i], agg_funcs[i], &arg_val, agg.distinct);
            }
            true
        })?;
    } else {
        // ==== General path: full decode + eval_row for whatever didn't resolve ====
        // The filter, however, may still COMPILE: evaluate it positionally
        // against the full row (identity positions) instead of walking the
        // AST with per-row name lookups.
        let compiled_filter = filter_predicate
            .and_then(|p| crate::executor::predicate::compile_predicate(p, &table, prefix));
        let identity: Vec<usize> = (0..n_cols).collect();
        let rowid_alias = table.rowid_alias;
        bt.scan_table_borrowed(|rowid, payload| {
            row_buf.clear();
            if decode_row_into(payload, n_cols, rowid, rowid_alias, &mut row_buf).is_err() {
                return true; // skip corrupt rows
            }
            // Apply the filter predicate inline (if any).
            if let Some(pred) = filter_predicate {
                let keep = if let Some(cp) = &compiled_filter {
                    cp.eval(&row_buf, &identity, params)
                } else {
                    match eval_row(pred, &row_buf, &columns, params, named_params) {
                        Ok(v) => v.is_truthy(),
                        Err(_) => false,
                    }
                };
                if !keep {
                    return true; // skip — predicate false
                }
            }
            // Compute the group-by key: direct index when resolved, eval_row
            // otherwise (e.g. GROUP BY x+y, GROUP BY upper(name)).
            key_buf.clear();
            let mut key_ok = true;
            for (gi_expr, kidx) in group_by.iter().zip(key_col_indices.iter()) {
                match kidx {
                    Some(idx) => key_buf.push(row_buf[*idx].clone()),
                    None => match eval_row(gi_expr, &row_buf, &columns, params, named_params) {
                        Ok(v) => key_buf.push(v),
                        Err(_) => {
                            key_ok = false;
                            break;
                        }
                    },
                }
            }
            if !key_ok {
                return true;
            }
            let gi = grouper.intern(&key_buf);
            if grouper.groups[gi].1.is_empty() {
                grouper.groups[gi].1 = (0..n_aggs).map(|_| AggState::default()).collect();
            }
            for (i, agg) in aggregates.iter().enumerate() {
                let arg_val = match (&agg.arg, agg_col_indices[i]) {
                    (Some(_), Some(idx)) => row_buf[idx].clone(),
                    (Some(arg), None) => match eval_row(arg, &row_buf, &columns, params, named_params) {
                        Ok(v) => v,
                        Err(_) => Value::Null,
                    },
                    (None, _) => Value::Integer(1), // COUNT(*)
                };
                update_agg_state(&mut grouper.groups[gi].1[i], agg_funcs[i], &arg_val, agg.distinct);
            }
            true
        })?;
    }

    // Emit one output row per group, in first-seen order (matches the
    // previous implementation's `group_order` behavior).
    let mut out_rows = Vec::with_capacity(grouper.len());
    for (key, states) in grouper.groups {
        let mut row = key;
        for (i, agg) in aggregates.iter().enumerate() {
            row.push(finalize_agg(&states[i], &agg.func));
        }
        out_rows.push(row);
    }

    let mut out_cols = Vec::new();
    for (i, g) in group_by.iter().enumerate() {
        let name = match g {
            Expr::Column { table: None, name } => name.clone(),
            Expr::Column { table: Some(t), name } => format!("{}.{}", t, name),
            _ => format!("col{}", i + 1),
        };
        out_cols.push(name);
    }
    for (i, _agg) in aggregates.iter().enumerate() {
        out_cols.push(format!("__agg_{}", i));
    }

    Ok(ExecResult { columns: out_cols.into(), rows: out_rows })
}

/// Vectorized fast path for `SELECT <aggregates> FROM t [WHERE pred]`
/// (no GROUP BY). Key optimizations vs the generic streaming-scan path:
///
/// 1. **No HashMap of groups** — only one group, so we accumulate directly
///    into a `Vec<AggState>`. Saves the per-row String key formatting
///    (which was ~200 ns/row on a 4-column table — 4× Debug-formatted
///    Values + a join("|") allocation).
///
/// 2. **Column index resolution upfront** — if every aggregate's arg is a
///    bare `Expr::Column`, we resolve the column index ONCE before the
///    scan, and during the scan we read `row_buf[idx]` directly (a Vec
///    index, ~1 ns) instead of calling `eval_row` (which does name lookup
///    + type coercion + Result wrapping, ~100 ns/row).
///
/// 3. **No per-row column-name formatting** — we only build the column-name
///    Vec if we actually need it (filter predicate or non-Column aggregate
///    args). For `SELECT SUM(x), COUNT(*) FROM t` (no predicate), we never
///    build it.
///
/// Together these optimizations cut the per-row overhead by ~10x for the
/// common OLAP case, bringing aggregate scan within ~2x of SQLite (from
/// the previous ~6x gap).
fn exec_aggregate_no_group_by(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    let n_cols = table.n_columns();
    let prefix = alias.as_deref().unwrap_or(&table.name);

    // Try to resolve each aggregate's arg as a bare Column index.
    // If ALL of them are Columns (or COUNT(*), which has no arg),
    // we can use the index-based fast path. Otherwise, fall back to eval_row.
    let mut agg_col_indices: Vec<Option<usize>> = Vec::with_capacity(aggregates.len());
    let mut all_columns = filter_predicate.is_none();
    for agg in aggregates {
        if let Some(arg) = &agg.arg {
            if let Expr::Column { table: ref_t, name } = arg {
                // Verify the table matches (or is None).
                let matches = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
                if matches {
                    if let Some(idx) = table.find_column(name) {
                        agg_col_indices.push(Some(idx));
                    } else {
                        agg_col_indices.push(None);
                        all_columns = false;
                    }
                } else {
                    agg_col_indices.push(None);
                    all_columns = false;
                }
            } else {
                agg_col_indices.push(None);
                all_columns = false;
            }
        } else {
            // COUNT(*) — no arg, no index needed.
            agg_col_indices.push(None);
        }
    }

    // Build column names only if we need them for eval_row calls.
    let columns: Option<Vec<String>> = if all_columns {
        None
    } else {
        Some(table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect())
    };
    let columns_ref = columns.as_ref();

    let agg_funcs: Vec<AggFunc> = aggregates.iter().map(|a| AggFunc::from_name(&a.func)).collect();
    let mut states: Vec<AggState> = (0..aggregates.len()).map(|_| AggState::default()).collect();
    let mut saw_any_row = false;

    // === Compiled-predicate path ===
    // When the filter compiles to positional comparisons (col op literal /
    // param, AND/OR/NOT, BETWEEN, IN, IS NULL, LIKE), evaluate it by direct
    // index into the selectively-decoded row slice — no full-row expansion,
    // no per-row name lookups, no AST walk. For `SELECT COUNT(*) FROM t
    // WHERE val > 5000` this is the difference between ~114 ns/row and
    // ~35 ns/row.
    let compiled = filter_predicate
        .and_then(|p| crate::executor::predicate::compile_predicate(p, &table, prefix));
    if let Some(pred) = compiled {
        // Eligible when every aggregate is COUNT(*) (no arg) or has a
        // resolved bare-column arg. (COUNT(*) pushes None into
        // agg_col_indices — that's fine, not a resolution failure.)
        let agg_ok = aggregates
            .iter()
            .zip(agg_col_indices.iter())
            .all(|(agg, idx)| agg.arg.is_none() || idx.is_some());
        if agg_ok {
            // Build the wanted-column list: predicate columns + aggregate args.
            let mut wanted: Vec<usize> = agg_col_indices.iter().filter_map(|x| *x).collect();
            crate::executor::predicate::compiled_columns(&pred, &mut wanted);
            wanted.sort_unstable();
            wanted.dedup();
            // positions[table_col] = position in the decoded slice.
            let mut positions = vec![usize::MAX; n_cols];
            for (pos, &c) in wanted.iter().enumerate() {
                positions[c] = pos;
            }
            // Wanted positions for each aggregate arg (usize::MAX when the
            // arg is COUNT(*) — no column).
            let agg_pos: Vec<usize> = agg_col_indices
                .iter()
                .map(|a| a.map(|c| positions[c]).unwrap_or(usize::MAX))
                .collect();

            let params = &ctx.params;
            let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
            let root = ctx.table_root(&table);
            let mut bt = Btree::new(ctx.pager, root, false);
            let rowid_alias = table.rowid_alias;
            bt.scan_table_borrowed(|rowid, payload| {
                if decode_row_selective(payload, n_cols, &wanted, rowid, rowid_alias, &mut sel_buf).is_err() {
                    return true;
                }
                if !pred.eval(&sel_buf, &positions, params) {
                    return true;
                }
                saw_any_row = true;
                for i in 0..aggregates.len() {
                    let arg_val = if agg_pos[i] == usize::MAX {
                        Value::Integer(1) // COUNT(*)
                    } else if agg_pos[i] < sel_buf.len() {
                        sel_buf[agg_pos[i]].clone()
                    } else {
                        Value::Null
                    };
                    update_agg_state(&mut states[i], agg_funcs[i], &arg_val, aggregates[i].distinct);
                }
                true
            })?;
            return finish_no_group_by(aggregates, states, saw_any_row);
        }
    }

    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();

    // === Selective-decode fast path ===
    // When ALL aggregate args are bare Column refs (or COUNT(*)) AND the
    // filter predicate also only references bare Columns, we can decode
    // only the referenced columns per row instead of the entire row.
    // For `SELECT SUM(val) FROM t` on a 3-col table, we skip decoding 2 cols
    // per row — directly cutting the dominant cost of `decode_row_into`.
    //
    // The filter predicate is allowed to be None (no WHERE clause) — in that
    // case the set of needed columns is just the aggregate args.
    let selective_eligible = agg_col_indices.iter().all(|x| x.is_some())
        && filter_predicate
            .map(|p| expr_only_columns(p, &table, prefix))
            .unwrap_or(true);

    if selective_eligible {
        // Build the sorted, deduped list of column indices we need to decode.
        let mut wanted: Vec<usize> = agg_col_indices.iter().filter_map(|x| *x).collect();
        if let Some(pred) = filter_predicate {
            collect_column_indices(pred, &table, prefix, &mut wanted);
        }
        wanted.sort_unstable();
        wanted.dedup();

        // Build column names only for the wanted columns (used by filter eval).
        let wanted_names: Option<Vec<String>> = if let Some(_) = filter_predicate {
            // The filter eval path needs `col_names` to match row indices.
            // Build the full col_names vec since eval_row expects all cols.
            Some(table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect())
        } else {
            None
        };

        let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
        let mut full_row_buf: Vec<Value> = Vec::with_capacity(n_cols);

        let root = ctx.table_root(&table);
        let mut bt = Btree::new(ctx.pager, root, false);
        // Use scan_table_borrowed — bypasses Cell::decode's per-row Vec<u8>
        // allocation by passing &[u8] borrows directly into the page buffer.
        // For 10k rows, this saves 10k malloc+free pairs.
        let rowid_alias = table.rowid_alias;
        bt.scan_table_borrowed(|rowid, payload| {
            // Decode only the wanted columns.
            if decode_row_selective(payload, n_cols, &wanted, rowid, rowid_alias, &mut sel_buf).is_err() {
                return true;
            }
            // Apply the filter predicate, if any. We need a full row buffer
            // because eval_row indexes by column position. The cheap path
            // is to expand sel_buf back into a full row (NULLs for un-wanted
            // columns). For aggregates only on a few cols, this is still
            // cheaper than decode_row_into because we skip the heavy Text/Blob
            // allocations on un-wanted cols (only Integer/Real decoded).
            if let Some(pred) = filter_predicate {
                let cols = wanted_names.as_ref().expect("col_names built when filter is present");
                // Expand into full_row_buf at the correct positions.
                full_row_buf.clear();
                full_row_buf.resize(n_cols, Value::Null);
                for (i, &col_idx) in wanted.iter().enumerate() {
                    if i < sel_buf.len() {
                        full_row_buf[col_idx] = sel_buf[i].clone();
                    }
                }
                match eval_row(pred, &full_row_buf, cols, &params, &named_params) {
                    Ok(v) => {
                        if !v.is_truthy() {
                            return true;
                        }
                    }
                    Err(_) => return true,
                }
            }
            saw_any_row = true;
            for (i, agg) in aggregates.iter().enumerate() {
                let arg_val = if let Some(wanted_pos) = agg_col_indices[i]
                    .as_ref()
                    .and_then(|widx| wanted.iter().position(|x| x == widx))
                {
                    sel_buf[wanted_pos].clone()
                } else if let Some(arg) = &agg.arg {
                    let cols = wanted_names.as_ref().expect("col_names built when not all are Column");
                    // Fall back to eval_row for non-Column args.
                    let mut full_row = vec![Value::Null; n_cols];
                    for (j, &col_idx) in wanted.iter().enumerate() {
                        if j < sel_buf.len() {
                            full_row[col_idx] = sel_buf[j].clone();
                        }
                    }
                    match eval_row(arg, &full_row, cols, &params, &named_params) {
                        Ok(v) => v,
                        Err(_) => Value::Null,
                    }
                } else {
                    Value::Integer(1)
                };
                update_agg_state(&mut states[i], agg_funcs[i], &arg_val, agg.distinct);
            }
            true
        })?;
    } else {
        // Fallback: decode the entire row.
        let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
        let root = ctx.table_root(&table);
        let mut bt = Btree::new(ctx.pager, root, false);
        let rowid_alias = table.rowid_alias;
        bt.scan_table_borrowed(|rowid, payload| {
            row_buf.clear();
            if decode_row_into(payload, n_cols, rowid, rowid_alias, &mut row_buf).is_err() {
                return true; // skip corrupt rows
            }
            // Apply the filter predicate inline (if any).
            if let Some(pred) = filter_predicate {
                let cols = columns_ref.expect("columns were built because predicate is Some");
                match eval_row(pred, &row_buf, cols, &params, &named_params) {
                    Ok(v) => {
                        if !v.is_truthy() {
                            return true;
                        }
                    }
                    Err(_) => return true,
                }
            }
            saw_any_row = true;
            for (i, agg) in aggregates.iter().enumerate() {
                let arg_val = if let Some(idx) = agg_col_indices[i] {
                    // Fast path: pre-resolved column index.
                    row_buf[idx].clone()
                } else if let Some(arg) = &agg.arg {
                    // Slow path: eval_row (e.g. SUM(x + 1), AVG(x * 2)).
                    let cols = columns_ref.expect("columns were built because not all are Column");
                    match eval_row(arg, &row_buf, cols, &params, &named_params) {
                        Ok(v) => v,
                        Err(_) => Value::Null,
                    }
                } else {
                    Value::Integer(1)
                };
                update_agg_state(&mut states[i], agg_funcs[i], &arg_val, agg.distinct);
            }
            true
        })?;
    }

    finish_no_group_by(aggregates, states, saw_any_row)
}

/// Finalize a no-GROUP-BY aggregate into its single output row.
/// SQLite semantics: an empty input emits one row of NULLs (COUNT → 0).
fn finish_no_group_by(aggregates: &[AggExpr], states: Vec<AggState>, _saw_any_row: bool) -> Result<ExecResult> {
    let mut out_row: Vec<Value> = Vec::with_capacity(aggregates.len());
    for (i, agg) in aggregates.iter().enumerate() {
        out_row.push(finalize_agg(&states[i], &agg.func));
    }
    let out_cols: Vec<String> = aggregates.iter().enumerate().map(|(i, _)| format!("__agg_{}", i)).collect();
    Ok(ExecResult { columns: out_cols.into(), rows: vec![out_row] })
}

/// True iff `expr` only references `Column` references that resolve
/// against `table` (so we can use the selective-decode fast path
/// instead of decoding the entire row). If the expression contains
/// `Parameter`, `Literal`, or other constructs, that's fine — we just
/// need every Column to resolve.
///
/// Used by `exec_aggregate_no_group_by`'s selective-decode fast path:
/// when the WHERE clause only references a few bare columns, we decode
/// only those columns per row instead of the full row.
fn expr_only_columns(expr: &Expr, table: &Table, prefix: &str) -> bool {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Subquery(_) => true,
        Expr::Column { table: ref_t, name } => {
            let matches_t = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
            matches_t && table.find_column(name).is_some()
        }
        Expr::Binary { left, right, .. } => {
            expr_only_columns(left, table, prefix) && expr_only_columns(right, table, prefix)
        }
        Expr::Unary { expr, .. } => expr_only_columns(expr, table, prefix),
        Expr::Between { expr, low, high, .. } => {
            expr_only_columns(expr, table, prefix)
                && expr_only_columns(low, table, prefix)
                && expr_only_columns(high, table, prefix)
        }
        Expr::IsNull { expr, .. } => expr_only_columns(expr, table, prefix),
        Expr::Is { left, right, .. } => {
            expr_only_columns(left, table, prefix) && expr_only_columns(right, table, prefix)
        }
        Expr::Like { expr, pattern, escape, .. } => {
            expr_only_columns(expr, table, prefix)
                && expr_only_columns(pattern, table, prefix)
                && escape.as_ref().map(|e| expr_only_columns(e, table, prefix)).unwrap_or(true)
        }
        Expr::Function { args, .. } => args.iter().all(|a| expr_only_columns(a, table, prefix)),
        Expr::Case { operand, whens, else_ } => {
            operand.as_ref().map(|o| expr_only_columns(o, table, prefix)).unwrap_or(true)
                && whens.iter().all(|(w, t)| expr_only_columns(w, table, prefix) && expr_only_columns(t, table, prefix))
                && else_.as_ref().map(|e| expr_only_columns(e, table, prefix)).unwrap_or(true)
        }
        Expr::Row(exprs) => exprs.iter().all(|e| expr_only_columns(e, table, prefix)),
        // Conservatively, anything else (subqueries with complex sources,
        // IN (SELECT...), EXISTS, etc.) disqualifies the fast path.
        _ => false,
    }
}

/// Collect every column index referenced by `expr` into `out`.
/// Only resolves bare Column refs against `table` (matching name or alias).
/// Used by `exec_aggregate_no_group_by`'s selective-decode fast path.
fn collect_column_indices(expr: &Expr, table: &Table, prefix: &str, out: &mut Vec<usize>) {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Subquery(_) => {}
        Expr::Column { table: ref_t, name } => {
            let matches_t = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
            if matches_t {
                if let Some(idx) = table.find_column(name) {
                    out.push(idx);
                }
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_column_indices(left, table, prefix, out);
            collect_column_indices(right, table, prefix, out);
        }
        Expr::Unary { expr, .. } => collect_column_indices(expr, table, prefix, out),
        Expr::Between { expr, low, high, .. } => {
            collect_column_indices(expr, table, prefix, out);
            collect_column_indices(low, table, prefix, out);
            collect_column_indices(high, table, prefix, out);
        }
        Expr::IsNull { expr, .. } => collect_column_indices(expr, table, prefix, out),
        Expr::Is { left, right, .. } => {
            collect_column_indices(left, table, prefix, out);
            collect_column_indices(right, table, prefix, out);
        }
        Expr::Like { expr, pattern, escape, .. } => {
            collect_column_indices(expr, table, prefix, out);
            collect_column_indices(pattern, table, prefix, out);
            if let Some(e) = escape {
                collect_column_indices(e, table, prefix, out);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_column_indices(a, table, prefix, out);
            }
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand { collect_column_indices(o, table, prefix, out); }
            for (w, t) in whens {
                collect_column_indices(w, table, prefix, out);
                collect_column_indices(t, table, prefix, out);
            }
            if let Some(e) = else_ { collect_column_indices(e, table, prefix, out); }
        }
        Expr::Row(exprs) => {
            for e in exprs { collect_column_indices(e, table, prefix, out); }
        }
        _ => {}
    }
}

fn exec_aggregate(ctx: &mut ExecContext<'_>, input: &Plan, group_by: &[Expr], aggregates: &[AggExpr]) -> Result<ExecResult> {
    // Fast path #0: SELECT COUNT(*) FROM t  (no WHERE, no GROUP BY, single COUNT(*)).
    // Uses `Btree::count_rows` which skips decoding every cell payload —
    // just sums `n_cells` across all leaf pages. For a 10k-row table this is
    // ~10x faster than the streaming scan + decode path.
    if group_by.is_empty()
        && aggregates.len() == 1
        && matches!(input, Plan::Scan { predicate: None, .. })
    {
        if let Plan::Scan { table, .. } = input {
            let agg = &aggregates[0];
            // COUNT(*) — arg is None (the planner emits COUNT(*) with no arg).
            // COUNT(col) — arg is Some(Column). We can't use the fast path
            // for COUNT(col) because we need to skip NULLs, which requires
            // decoding.
            if agg.func == "count" && agg.arg.is_none() && !agg.distinct {
                let root = ctx.table_root(table);
                let mut bt = Btree::new(ctx.pager, root, false);
                let n = bt.count_rows()?;
                let mut row = Vec::with_capacity(1);
                row.push(Value::Integer(n as i64));
                return Ok(ExecResult {
                    columns: Arc::from(vec!["__agg_0".to_string()]),
                    rows: vec![row],
                });
            }
        }
    }
    // Fast path #1: input is a bare Scan.
    // Handles: `SELECT SUM/AVG/MIN/MAX/COUNT(*) FROM t`
    //          `SELECT col, COUNT(*) FROM t GROUP BY col`
    if let Plan::Scan { table, alias, index: None, predicate: None } = input {
        return exec_aggregate_streaming_scan(ctx, table.clone(), alias.clone(), None, group_by, aggregates);
    }
    // Fast path #1b: COUNT(*) over a RowidRange — count leaf CELLS in the
    // rowid range with no payload decoding. `SELECT COUNT(*) FROM t WHERE
    // id BETWEEN ? AND ?` used to fall to the general path, which
    // materialized every row in the range (full payload decode + a Vec of
    // Values per row) just to count them. `count_rows_range` binary
    // searches each leaf for the first rowid >= start and counts cells
    // until rowid > end. Only for a residual-free range: a residual
    // predicate (strict < / >, extra filters) needs row data.
    if group_by.is_empty()
        && aggregates.len() == 1
        && aggregates[0].func == "count"
        && aggregates[0].arg.is_none()
        && !aggregates[0].distinct
    {
        if let Plan::RowidRange { table, start, end, residual: None, .. } = input {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            let start_v = match start {
                Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
                None => i64::MIN,
            };
            let end_v = match end {
                Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
                None => i64::MAX,
            };
            let root = ctx.table_root(table);
            let mut bt = Btree::new(ctx.pager, root, false);
            let n = bt.count_rows_range(start_v, end_v)?;
            let mut row = Vec::with_capacity(1);
            row.push(Value::Integer(n as i64));
            return Ok(ExecResult {
                columns: Arc::from(vec!["__agg_0".to_string()]),
                rows: vec![row],
            });
        }
    }
    // Fast path #2: input is Filter(Scan, predicate).
    // Handles: `SELECT COUNT(*) FROM t WHERE val > 5000`
    //          `SELECT col, COUNT(*) FROM t WHERE x > 0 GROUP BY col`
    if let Plan::Filter { input: inner, predicate } = input {
        if let Plan::Scan { table, alias, index: None, predicate: None } = inner.as_ref() {
            return exec_aggregate_streaming_scan(ctx, table.clone(), alias.clone(), Some(predicate), group_by, aggregates);
        }
    }
    // Fast path #3: COUNT(*) over an IndexRange — COVERING INDEX count.
    // `SELECT COUNT(*) FROM t WHERE indexed_col > ?` counts index ENTRIES
    // directly: no row fetching, no decoding. The general path materialized
    // every matching row (a B+tree descent per rowid) just to count them.
    if group_by.is_empty()
        && aggregates.len() == 1
        && aggregates[0].func == "count"
        && aggregates[0].arg.is_none()
        && !aggregates[0].distinct
    {
        let covering = match input {
            Plan::IndexRange { table, index, start, end, residual, .. } => {
                if residual.is_none() {
                    Some((table.clone(), index.clone(), start.as_ref(), end.as_ref()))
                } else {
                    None
                }
            }
            Plan::Filter { input: inner, .. } => {
                // The planner sometimes wraps IndexRange in a Filter with
                // the range predicate as residual; only cover when the
                // filter is exactly the index range (residual == predicate).
                if let Plan::IndexRange { table, index, start, end, residual, .. } = inner.as_ref() {
                    // Only cover when there is NO residual predicate —
                    // any residual condition needs row data, so the index
                    // alone can't answer the count. (Conservative: even a
                    // residual identical to the filter is skipped; those
                    // plans fall through to the general path.)
                    if residual.is_none() {
                        Some((table.clone(), index.clone(), start.as_ref(), end.as_ref()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((_table, index, start, end)) = covering {
            // Evaluate the bounds (same logic as exec_index_range).
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            let start_key: Option<(Vec<u8>, bool)> = match start {
                Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
                None => None,
            };
            let end_key: Option<(Vec<u8>, bool)> = match end {
                Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
                None => None,
            };
            let scan_start: Vec<u8> = start_key.as_ref().map(|(k, _)| k.clone()).unwrap_or_default();
            let mut n: i64 = 0;
            let index_root = ctx.index_root(&index);
            let mut index_bt = Btree::new(ctx.pager, index_root, true);
            index_bt.scan_index_from(&scan_start, |_rowid, cell_key| {
                if let Some((k, false)) = &start_key {
                    if cell_key.starts_with(k) {
                        return true; // exclusive lower bound
                    }
                }
                if let Some((k, inc)) = &end_key {
                    match index_key_prefix_cmp(cell_key, k) {
                        std::cmp::Ordering::Greater => return false,
                        std::cmp::Ordering::Equal if !*inc => return false,
                        _ => {}
                    }
                }
                n += 1;
                true
            })?;
            let mut row = Vec::with_capacity(1);
            row.push(Value::Integer(n));
            return Ok(ExecResult {
                columns: Arc::from(vec!["__agg_0".to_string()]),
                rows: vec![row],
            });
        }
    }
    let inner = execute(input, ctx)?;
    // Borrow params directly (inner is an owned local — no conflict).
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let agg_funcs: Vec<AggFunc> = aggregates.iter().map(|a| AggFunc::from_name(&a.func)).collect();
    // Resolve group-by exprs and agg args against the input's column names
    // once, so per-row work is an index read whenever possible.
    let key_col_indices: Vec<Option<usize>> = group_by
        .iter()
        .map(|e| match e {
            Expr::Column { table, name } => resolve_column_index(&inner.columns, table.as_deref(), name),
            _ => None,
        })
        .collect();
    let agg_col_indices: Vec<Option<usize>> = aggregates
        .iter()
        .map(|agg| match &agg.arg {
            Some(Expr::Column { table, name }) => {
                resolve_column_index(&inner.columns, table.as_deref(), name)
            }
            _ => None,
        })
        .collect();

    let mut grouper = HashGrouper::default();
    let n_aggs = aggregates.len();
    let mut key_buf: Vec<Value> = Vec::with_capacity(group_by.len());

    for row in &inner.rows {
        // Group key: direct index when resolvable, eval_row otherwise.
        key_buf.clear();
        let mut key_ok = true;
        for (ge, kidx) in group_by.iter().zip(key_col_indices.iter()) {
            match kidx {
                Some(idx) => key_buf.push(row[*idx].clone()),
                None => match eval_row(ge, row, &inner.columns, params, named_params) {
                    Ok(v) => key_buf.push(v),
                    Err(_) => {
                        key_ok = false;
                        break;
                    }
                },
            }
        }
        if !key_ok {
            continue;
        }
        let gi = grouper.intern(&key_buf);
        if grouper.groups[gi].1.is_empty() {
            grouper.groups[gi].1 = (0..n_aggs).map(|_| AggState::default()).collect();
        }
        for (i, agg) in aggregates.iter().enumerate() {
            let arg_val = match (&agg.arg, agg_col_indices[i]) {
                (Some(_), Some(idx)) => row[idx].clone(),
                (Some(arg), None) => eval_row(arg, row, &inner.columns, params, named_params)?,
                (None, _) => Value::Integer(1),
            };
            update_agg_state(&mut grouper.groups[gi].1[i], agg_funcs[i], &arg_val, agg.distinct);
        }
    }

    // SQLite semantics: if there is no GROUP BY clause AND no rows were
    // produced by the input, the aggregate still emits ONE row (with
    // COUNT=0, SUM=NULL, AVG=NULL, MIN=NULL, MAX=NULL). This handles the
    // common `SELECT COUNT(*) FROM empty_table` case.
    if group_by.is_empty() && grouper.is_empty() && !aggregates.is_empty() {
        grouper.groups.push((Vec::new(), (0..n_aggs).map(|_| AggState::default()).collect()));
    }

    let mut out_rows = Vec::with_capacity(grouper.len());
    for (key, states) in grouper.groups {
        let mut row = key;
        for (i, agg) in aggregates.iter().enumerate() {
            row.push(finalize_agg(&states[i], &agg.func));
        }
        out_rows.push(row);
    }

    let mut out_cols = Vec::new();
    for (i, g) in group_by.iter().enumerate() {
        // Name the group-by output column after the source expression so
        // that downstream Sort / Filter operators can find it by name. For
        // `GROUP BY cat` we name the column "cat"; for `GROUP BY t.cat` we
        // name it "t.cat". For non-column group-by exprs (e.g.
        // `GROUP BY x+y`), fall back to "colN" since there's no obvious name.
        // Without this, ORDER BY cat on an Aggregate plan would fail to
        // resolve the column and sort by NULLs only — surfacing as
        // "GROUP BY with NULL keys puts NULL group in the wrong order".
        let name = match g {
            Expr::Column { table: None, name } => name.clone(),
            Expr::Column { table: Some(t), name } => format!("{}.{}", t, name),
            _ => format!("col{}", i + 1),
        };
        out_cols.push(name);
    }
    for (i, agg) in aggregates.iter().enumerate() {
        // Use a synthetic name that the planner's rewrite_aggregates() can find.
        // The alias (if any) is still used as the display name in the Project.
        let _ = agg.alias;
        let _ = i;
        out_cols.push(format!("__agg_{}", i));
    }

    Ok(ExecResult { columns: out_cols.into(), rows: out_rows })
}

fn update_agg_state(state: &mut AggState, func: AggFunc, v: &Value, distinct: bool) {
    // Only compute the distinct key if we're actually doing a DISTINCT
    // aggregate. For non-DISTINCT aggregates, this skips per-row hashing
    // entirely. The key is a `SqlValueKey` (a Value clone — free for
    // Integer/Real) instead of the old `format!("{:?}")` String, saving
    // a heap allocation per row for DISTINCT aggregates.
    if distinct {
        if !state.distinct.insert(SqlValueKey(v.clone())) {
            return;
        }
    }
    // Only mark "seen_value" for non-NULL inputs. This makes SUM of all
    // NULLs return NULL (matching SQLite), rather than the previous
    // behavior of returning Integer(0) because seen_value was set
    // unconditionally before the per-func match. Bug surfaced by the
    // differential test 'sum_of_all_nulls_is_null'.
    if !v.is_null() {
        state.seen_value = true;
    }
    match func {
        // SQLite semantics: COUNT(*) counts all rows; COUNT(col) counts
        // non-NULL values of col. For COUNT(*) the planner passes a
        // non-NULL placeholder (Value::Integer(1)) as the arg, so checking
        // v.is_null() here correctly skips NULLs for COUNT(col) while
        // still counting every row for COUNT(*). This bug was caught by
        // the SLT test suite (which expected COUNT(val) to be 5 over a
        // 6-row table where one row had val=NULL).
        AggFunc::Count => {
            if v.is_null() {
                return;
            }
            state.count += 1
        }
        AggFunc::Sum | AggFunc::Total => {
            if !v.is_null() {
                if matches!(v, Value::Real(_)) {
                    state.sum_is_int = false;
                    state.sum += v.as_real();
                } else if state.sum_is_int {
                    state.int_sum = state.int_sum.saturating_add(v.as_integer());
                    state.sum = state.int_sum as f64;
                } else {
                    state.sum += v.as_real();
                }
            }
        }
        AggFunc::Avg => {
            if !v.is_null() {
                state.count += 1;
                state.sum += v.as_real();
            }
        }
        AggFunc::Min => {
            if !v.is_null() {
                if state.min.is_none() || v < state.min.as_ref().unwrap() {
                    state.min = Some(v.clone());
                }
            }
        }
        AggFunc::Max => {
            if !v.is_null() {
                if state.max.is_none() || v > state.max.as_ref().unwrap() {
                    state.max = Some(v.clone());
                }
            }
        }
        AggFunc::GroupConcat => {
            if !v.is_null() {
                if !state.concat.is_empty() {
                    state.concat.push(',');
                }
                state.concat.push_str(&v.as_text());
            }
        }
        _ => {}
    }
}

fn finalize_agg(state: &AggState, func: &str) -> Value {
    match func {
        "count" => Value::Integer(state.count),
        "sum" => {
            if !state.seen_value {
                Value::Null
            } else if state.sum_is_int {
                Value::Integer(state.int_sum)
            } else {
                Value::Real(state.sum)
            }
        }
        "total" => Value::Real(state.sum),
        "avg" => {
            if state.count == 0 {
                Value::Null
            } else {
                // Round to a reasonable precision to avoid IEEE-754 noise.
                Value::Real((state.sum / state.count as f64 * 1e10).round() / 1e10)
            }
        }
        "min" => state.min.clone().unwrap_or(Value::Null),
        "max" => state.max.clone().unwrap_or(Value::Null),
        "group_concat" => Value::Text(state.concat.clone().into()),
        _ => Value::Null,
    }
}

// ============================================================================
// Window functions
// ============================================================================

fn exec_window(ctx: &mut ExecContext<'_>, input: &Plan, windows: &[WindowExpr]) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let n_rows = inner.rows.len();

    // For each window, compute values for each row.
    let mut extra_cols: Vec<Vec<Value>> = vec![Vec::new(); n_rows];

    for (w_idx, w) in windows.iter().enumerate() {
        // Group rows by partition key, preserving original order.
        let mut partitions: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
        let mut partition_map: HashMap<String, usize> = HashMap::new();
        for (i, row) in inner.rows.iter().enumerate() {
            let key: Vec<Value> = w.partition_by.iter().map(|e| eval_row(e, row, &inner.columns, &params, &named_params)).collect::<Result<_>>()?;
            let key_str = key.iter().map(|v| format!("{:?}", v)).collect::<Vec<_>>().join("|");
            if let Some(&idx) = partition_map.get(&key_str) {
                partitions[idx].1.push(i);
            } else {
                partition_map.insert(key_str, partitions.len());
                partitions.push((key, vec![i]));
            }
        }

        for (_key, indices) in &partitions {
            let mut sorted_indices = indices.clone();
            if !w.order_by.is_empty() {
                sorted_indices.sort_by(|a, b| {
                    let va = eval_row(&w.order_by[0].expr, &inner.rows[*a], &inner.columns, &params, &named_params).unwrap_or(Value::Null);
                    let vb = eval_row(&w.order_by[0].expr, &inner.rows[*b], &inner.columns, &params, &named_params).unwrap_or(Value::Null);
                    let ord = va.cmp(&vb);
                    if w.order_by[0].order == Order::Desc { ord.reverse() } else { ord }
                });
            }

            let mut row_num;
            let mut rank = 0i64;
            let mut dense_rank = 0i64;
            let mut prev_key: Option<Vec<Value>> = None;
            let mut count_in_rank = 0i64;
            for (pos_in_partition, &row_idx) in sorted_indices.iter().enumerate() {
                row_num = (pos_in_partition + 1) as i64;
                let row = &inner.rows[row_idx];
                let key: Vec<Value> = w.order_by.iter().map(|t| eval_row(&t.expr, row, &inner.columns, &params, &named_params)).collect::<Result<_>>()?;
                if prev_key.as_ref() != Some(&key) {
                    rank += count_in_rank + 1;
                    count_in_rank = 0;
                    dense_rank += 1;
                }
                count_in_rank += 1;
                prev_key = Some(key);

                let val = compute_window_value(w, row_num, rank, dense_rank, row, &inner.columns, &params, &named_params)?;
                if extra_cols[row_idx].is_empty() {
                    extra_cols[row_idx] = vec![Value::Null; windows.len()];
                }
                extra_cols[row_idx][w_idx] = val;
            }
        }
    }

    // Append window columns to each row.
    for (i, row) in inner.rows.iter_mut().enumerate() {
        if !extra_cols[i].is_empty() {
            row.extend(extra_cols[i].drain(..));
        }
    }

    // Build column names: original + window aliases.
    let mut out_cols: Vec<String> = inner.columns.to_vec();
    for w in windows {
        out_cols.push(w.alias.clone().unwrap_or_else(|| w.display_name.clone()));
    }
    inner.columns = out_cols.into();

    Ok(inner)
}

fn compute_window_value(
    w: &WindowExpr,
    row_num: i64,
    rank: i64,
    dense_rank: i64,
    row: &Row,
    column_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<Value> {
    let eval_ctx = EvalContext::new(row, column_names, params, named_params);
    match w.func.as_str() {
        "row_number" => Ok(Value::Integer(row_num)),
        "rank" => Ok(Value::Integer(rank)),
        "dense_rank" => Ok(Value::Integer(dense_rank)),
        "percent_rank" | "cume_dist" => Ok(Value::Real(0.0)),
        "sum" | "avg" | "min" | "max" | "count" => {
            if let Some(arg) = &w.arg {
                evaluate(arg, &eval_ctx)
            } else {
                Ok(Value::Integer(row_num))
            }
        }
        "lag" | "lead" | "first_value" | "last_value" | "nth_value" => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

// ============================================================================
// Join (nested-loop)
// ============================================================================

fn exec_join(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan, join_type: crate::sql::ast::JoinType, condition: &Option<Expr>) -> Result<ExecResult> {
    let left_res = execute(left, ctx)?;
    let right_res = execute(right, ctx)?;
    let mut combined_cols: Vec<String> = left_res.columns.to_vec();
    combined_cols.extend(right_res.columns.iter().cloned());
    let n_left = left_res.columns.len();
    let n_right = right_res.columns.len();
    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();

    let mut out_rows = Vec::new();
    let mut right_matched = vec![false; right_res.rows.len()];

    for left_row in &left_res.rows {
        let mut matched = false;
        for (ri, right_row) in right_res.rows.iter().enumerate() {
            let mut combined = left_row.clone();
            combined.extend(right_row.clone());
            let ok = if let Some(cond) = condition {
                let v = eval_row(cond, &combined, &combined_cols, &params, &named_params)?;
                v.is_truthy()
            } else {
                true
            };
            if ok {
                out_rows.push(combined);
                matched = true;
                right_matched[ri] = true;
            }
        }
        if !matched && matches!(join_type, crate::sql::ast::JoinType::Left | crate::sql::ast::JoinType::Full) {
            let mut combined = left_row.clone();
            combined.extend(vec![Value::Null; n_right]);
            out_rows.push(combined);
        }
    }

    // RIGHT and FULL: emit unmatched right rows with NULL left.
    if matches!(join_type, crate::sql::ast::JoinType::Right | crate::sql::ast::JoinType::Full) {
        for (ri, right_row) in right_res.rows.iter().enumerate() {
            if !right_matched[ri] {
                let mut combined = vec![Value::Null; n_left];
                combined.extend(right_row.clone());
                out_rows.push(combined);
            }
        }
    }

    Ok(ExecResult { columns: combined_cols.into(), rows: out_rows })
}

/// Hash join: build a hash table on the smaller side, then probe with the
/// other side. Works for equi-joins where the condition is `left.col =
/// right.col` (or an AND chain of such equalities).
///
/// Performance-critical design notes (this node runs for every join in
/// every query):
///
/// 1. **Pure-equi fast path.** When the join condition consists ONLY of the
///    extracted equi-key predicates (no residual terms like `b.y > 5`),
///    matching hash keys PROVES the condition holds — the per-candidate
///    `eval_row` call is skipped entirely. That call walks the AST and
///    resolves column names by string comparison per row (~300-500 ns
///    each); skipping it is the single biggest win, ~0.4 ms on a 1k x 1k
///    join.
///
/// 2. **Order-key encoding for join keys.** Keys are built with
///    `encode_order_key_into`, which interleaves INTEGER and REAL values
///    numerically. This is both alloc-free (into a reusable buffer on the
///    probe side) and *semantically required*: SQL equality says 5 = 5.0,
///    but the old `Value::encode()` tagged encoding hashed them apart,
///    silently dropping cross-type join matches.
///
/// 3. **NULL keys never match.** `NULL = x` is NULL in SQL. NULL-keyed
///    build rows are excluded from the hash table and NULL-keyed probe
///    rows skip probing — they fall through to the unmatched-emission path
///    (which is what LEFT/RIGHT/FULL joins need; INNER just drops them).
///
/// 4. **Bucket-chain hash table.** `heads: HashMap<Vec<u8>, u32>` plus a
///    flat `chain: Vec<u32>` linked list. One allocation per build row
///    (the owned key), zero per probe. The previous layout stored
///    `Vec<u8> -> Vec<usize>` — an extra heap Vec per distinct key.
///
/// 5. **Single-allocation combined rows.** `Vec::with_capacity(n_l+n_r)` +
///    two `extend_from_slice`s, instead of cloning one side wholesale into
///    a temporary Vec that was immediately consumed by `extend`.
fn exec_hash_join(
    ctx: &mut ExecContext<'_>,
    left: &Plan,
    right: &Plan,
    join_type: crate::sql::ast::JoinType,
    condition: &Option<Expr>,
    projection: Option<&[crate::planner::plan::ProjectExpr]>,
) -> Result<ExecResult> {
    let left_res = execute(left, ctx)?;
    let right_res = execute(right, ctx)?;
    let mut combined_cols: Vec<String> = left_res.columns.to_vec();
    combined_cols.extend(right_res.columns.iter().cloned());
    let n_left = left_res.columns.len();
    let n_right = right_res.columns.len();
    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();

    // Extract the equi-join keys from the condition.
    // We expect `left.col = right.col` (a single equality or an AND of equalities).
    let eq_pairs = extract_equi_join_keys(condition, &left_res.columns, &right_res.columns);

    if eq_pairs.is_empty() {
        // No equi-join keys — fall back to nested-loop (+ optional projection).
        let res = exec_join(ctx, left, right, join_type, condition)?;
        return match projection {
            Some(cols) => apply_projection(res, cols, ctx),
            None => Ok(res),
        };
    }

    // Is the condition EXACTLY the conjunction of the equi predicates?
    // (Every leaf of the AND-tree is an `l.col = r.col` that produced a
    // pair, and there are no other leaves.) If so, a hash-key match proves
    // the whole condition — no residual evaluation needed.
    let pure_equi = {
        let n_leaves = count_eq_leaves_and_purity(condition, &left_res.columns, &right_res.columns);
        matches!(n_leaves, Some(n) if n == eq_pairs.len())
    };

    // ---- FUSED PROJECTION resolution ----
    // When this join sits under a Project of bare column references, the
    // join emits ONLY those columns directly — no full-width combined row,
    // no second cloning pass. Requires pure_equi (a residual predicate
    // would need to see the full combined row).
    let mut proj: Option<(Vec<usize>, Vec<String>)> = None;
    if let (Some(columns), true) = (projection, pure_equi) {
        if !columns.is_empty()
            && columns.iter().all(|c| {
                matches!(&c.expr, Expr::Column { name, .. } if name != "*")
            })
        {
            let mut indices = Vec::with_capacity(columns.len());
            let mut names = Vec::with_capacity(columns.len());
            let mut all_ok = true;
            for c in columns {
                if let Expr::Column { table, name } = &c.expr {
                    match resolve_column_index(&combined_cols, table.as_deref(), name) {
                        Some(i) => {
                            indices.push(i);
                            names.push(match &c.alias {
                                Some(a) => a.clone(),
                                None => expr_display_name(&c.expr),
                            });
                        }
                        None => {
                            all_ok = false;
                            break;
                        }
                    }
                } else {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                proj = Some((indices, names));
            }
        }
    }

    // Build a hash on the smaller side to minimize build cost.
    // For INNER joins we can freely pick which side to build on; for outer
    // joins we must preserve the side that's preserved by the join type, so
    // we fall back to the (correct-but-slower) right-side-build path. This
    // is the common case (real OLTP joins are overwhelmingly inner).
    let is_inner = matches!(join_type, crate::sql::ast::JoinType::Inner | crate::sql::ast::JoinType::Cross);
    let left_is_smaller = left_res.rows.len() <= right_res.rows.len();
    let build_left = is_inner && left_is_smaller;

    let (build_rows, build_key_indices): (&Vec<Row>, Vec<usize>) = if build_left {
        let idxs: Vec<usize> = eq_pairs.iter().map(|(l, _)| *l).collect();
        (&left_res.rows, idxs)
    } else {
        let idxs: Vec<usize> = eq_pairs.iter().map(|(_, r)| *r).collect();
        (&right_res.rows, idxs)
    };

    // ---- u64 FAST KEY PATH ----
    // Single numeric key column where every build key is a small INTEGER
    // (|i| <= 2^53) or a REAL: the hash key is the 8-byte order-key double
    // (double_order_key), which maps Integer(n) and Real(n as f64) to the
    // SAME key — exactly the engine's cross-type numeric equality — and is
    // injective among those values. This eliminates the per-build-row
    // Vec<u8> key allocation entirely (the dominant build cost for the
    // ubiquitous integer-FK join shape: `b.a_id = a.id`).
    let u64_mode = build_key_indices.len() == 1 && {
        let ci = build_key_indices[0];
        build_rows.iter().all(|r| match r.get(ci) {
            Some(Value::Integer(i)) => i.unsigned_abs() <= (1u64 << 53),
            Some(Value::Real(_)) => true,
            _ => false,
        })
    };

    // ---- BUILD phase ----
    // heads: key -> first row ordinal in the chain
    // chain: row ordinal -> next row ordinal with the same key (u32::MAX = end)
    let mut heads_u64: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::with_capacity(build_rows.len().max(1));
    let mut heads_bytes: std::collections::HashMap<Vec<u8>, u32> =
        std::collections::HashMap::with_capacity(build_rows.len().max(1));
    let mut chain: Vec<u32> = vec![u32::MAX; build_rows.len()];
    let mut key_buf: Vec<u8> = Vec::with_capacity(32);
    for (i, row) in build_rows.iter().enumerate() {
        if u64_mode {
            let ci = build_key_indices[0];
            let k = match &row[ci] {
                Value::Integer(iv) => crate::types::value::double_order_key(*iv as f64),
                Value::Real(fv) => crate::types::value::double_order_key(*fv),
                _ => unreachable!("u64_mode verified all build keys numeric"),
            };
            if let Some(prev) = heads_u64.get_mut(&k) {
                chain[i] = *prev;
                *prev = i as u32;
            } else {
                heads_u64.insert(k, i as u32);
            }
        } else {
            key_buf.clear();
            let mut has_null = false;
            for &ci in &build_key_indices {
                match row.get(ci) {
                    Some(Value::Null) | None => {
                        has_null = true;
                        break;
                    }
                    Some(v) => v.encode_order_key_into(&mut key_buf),
                }
            }
            if has_null {
                // NULL keys never match anything; leave this row out of the
                // table. build_matched stays false so outer joins emit it
                // as unmatched.
                continue;
            }
            if let Some(prev) = heads_bytes.get_mut(&key_buf) {
                // Prepend to the bucket chain: this row becomes the new
                // head, linked to the previous head.
                chain[i] = *prev;
                *prev = i as u32;
            } else {
                // Clone the buffer ONLY when inserting a new key.
                heads_bytes.insert(key_buf.clone(), i as u32);
            }
        }
    }

    // ---- PROBE phase ----
    let (probe_rows, probe_key_indices, probe_is_left): (&Vec<Row>, Vec<usize>, bool) = if build_left {
        let idxs: Vec<usize> = eq_pairs.iter().map(|(_, r)| *r).collect();
        (&right_res.rows, idxs, false)
    } else {
        let idxs: Vec<usize> = eq_pairs.iter().map(|(l, _)| *l).collect();
        (&left_res.rows, idxs, true)
    };

    // Projection bookkeeping: output width and per-output-source mapping.
    // proj_indices[i] = combined-row index of output column i.
    let proj_indices: Option<&Vec<usize>> = proj.as_ref().map(|(idxs, _)| idxs);
    let out_width = match proj_indices {
        Some(idxs) => idxs.len(),
        None => n_left + n_right,
    };
    let mut out_rows: Vec<Row> = Vec::with_capacity(probe_rows.len());
    let mut build_matched = vec![false; build_rows.len()];

    // NULL row templates for unmatched emission (LEFT/RIGHT/FULL).
    let nulls_right: Vec<Value> = vec![Value::Null; n_right];
    let nulls_left: Vec<Value> = vec![Value::Null; n_left];

    let residual: Option<&Expr> = if pure_equi { None } else { condition.as_ref() };

    // Helper: emit a joined (probe_row, build_row) pair, either fused to
    // the projected columns or as a full [left, right] combined row.
    #[inline]
    fn emit_row(
        out_rows: &mut Vec<Row>,
        proj_indices: Option<&Vec<usize>>,
        probe_row: &Row,
        build_row: &Row,
        probe_is_left: bool,
        n_left: usize,
        out_width: usize,
    ) {
        match proj_indices {
            Some(idxs) => {
                let mut out = Vec::with_capacity(idxs.len());
                for &p in idxs {
                    // combined index p: [0, n_left) = left side, else right.
                    let v = if p < n_left {
                        if probe_is_left {
                            &probe_row[p]
                        } else {
                            &build_row[p]
                        }
                    } else if probe_is_left {
                        &build_row[p - n_left]
                    } else {
                        &probe_row[p - n_left]
                    };
                    out.push(v.clone());
                }
                out_rows.push(out);
            }
            None => {
                let mut combined: Row = Vec::with_capacity(out_width);
                if probe_is_left {
                    combined.extend_from_slice(probe_row);
                    combined.extend_from_slice(build_row);
                } else {
                    combined.extend_from_slice(build_row);
                    combined.extend_from_slice(probe_row);
                }
                out_rows.push(combined);
            }
        }
    }

    for probe_row in probe_rows.iter() {
        // Build the probe key. NULL key -> no match possible.
        let mut probe_null = false;
        let mut u64_probe_key: u64 = 0;
        if u64_mode {
            let ci = probe_key_indices[0];
            match probe_row.get(ci) {
                Some(Value::Null) | None => probe_null = true,
                // Text/Blob probe keys can never equal the all-numeric
                // build keys (different storage classes never compare
                // equal), so they are non-matches, not lookups.
                Some(Value::Integer(iv)) => {
                    u64_probe_key = crate::types::value::double_order_key(*iv as f64)
                }
                Some(Value::Real(fv)) => {
                    u64_probe_key = crate::types::value::double_order_key(*fv)
                }
                Some(_) => probe_null = true,
            }
        } else {
            key_buf.clear();
            for &ci in &probe_key_indices {
                match probe_row.get(ci) {
                    Some(Value::Null) | None => {
                        probe_null = true;
                        break;
                    }
                    Some(v) => v.encode_order_key_into(&mut key_buf),
                }
            }
        }
        let mut matched = false;
        if !probe_null {
            let mut next = if u64_mode {
                heads_u64.get(&u64_probe_key).copied()
            } else {
                heads_bytes.get(&key_buf).copied()
            };
            while let Some(n) = next {
                if n == u32::MAX {
                    break;
                }
                let bi = n as usize;
                let build_row = &build_rows[bi];
                if residual.is_some() {
                    // Residual predicates need the FULL combined row.
                    let mut combined: Row = Vec::with_capacity(n_left + n_right);
                    if probe_is_left {
                        combined.extend_from_slice(probe_row);
                        combined.extend_from_slice(build_row);
                    } else {
                        combined.extend_from_slice(build_row);
                        combined.extend_from_slice(probe_row);
                    }
                    let ok = {
                        let v = eval_row(residual.unwrap(), &combined, &combined_cols, &params, &named_params)?;
                        v.is_truthy()
                    };
                    if ok {
                        emit_row(&mut out_rows, proj_indices, probe_row, build_row, probe_is_left, n_left, out_width);
                        matched = true;
                        build_matched[bi] = true;
                    }
                } else {
                    emit_row(&mut out_rows, proj_indices, probe_row, build_row, probe_is_left, n_left, out_width);
                    matched = true;
                    build_matched[bi] = true;
                }
                // Advance the chain (u32::MAX sentinel = end of bucket).
                next = match chain[bi] {
                    u32::MAX => None,
                    m => Some(m),
                };
            }
        }
        // Unmatched handling for LEFT/RIGHT/FULL joins.
        // If probe is left and the join preserves left (LEFT/FULL), emit [probe, NULLs].
        // If probe is right and the join preserves right (RIGHT/FULL), emit [NULLs, probe].
        if !matched {
            if probe_is_left && matches!(join_type, crate::sql::ast::JoinType::Left | crate::sql::ast::JoinType::Full) {
                match proj_indices {
                    Some(idxs) => {
                        let mut out = Vec::with_capacity(idxs.len());
                        for &p in idxs {
                            let v = if p < n_left {
                                // Left side is the probe (preserved) row.
                                &probe_row[p]
                            } else {
                                &Value::Null
                            };
                            out.push(v.clone());
                        }
                        out_rows.push(out);
                    }
                    None => {
                        let mut c: Row = Vec::with_capacity(out_width);
                        c.extend_from_slice(probe_row);
                        c.extend_from_slice(&nulls_right);
                        out_rows.push(c);
                    }
                }
            } else if !probe_is_left && matches!(join_type, crate::sql::ast::JoinType::Right | crate::sql::ast::JoinType::Full) {
                match proj_indices {
                    Some(idxs) => {
                        let mut out = Vec::with_capacity(idxs.len());
                        for &p in idxs {
                            let v = if p < n_left {
                                &Value::Null
                            } else {
                                // Right side is the probe (preserved) row.
                                &probe_row[p - n_left]
                            };
                            out.push(v.clone());
                        }
                        out_rows.push(out);
                    }
                    None => {
                        let mut c: Row = Vec::with_capacity(out_width);
                        c.extend_from_slice(&nulls_left);
                        c.extend_from_slice(probe_row);
                        out_rows.push(c);
                    }
                }
            }
            // For INNER/CROSS, unmatched probe rows are dropped.
        }
    }

    // Emit unmatched build-side rows for the outer-join case where the build
    // side is the preserved side (LEFT preserved by LEFT/FULL if build was left;
    // RIGHT preserved by RIGHT/FULL if build was right).
    if build_left && matches!(join_type, crate::sql::ast::JoinType::Left | crate::sql::ast::JoinType::Full) {
        for (bi, build_row) in build_rows.iter().enumerate() {
            if !build_matched[bi] {
                match proj_indices {
                    Some(idxs) => {
                        let mut out = Vec::with_capacity(idxs.len());
                        for &p in idxs {
                            let v = if p < n_left {
                                // Left side is the build (preserved) row.
                                &build_row[p]
                            } else {
                                &Value::Null
                            };
                            out.push(v.clone());
                        }
                        out_rows.push(out);
                    }
                    None => {
                        let mut c: Row = Vec::with_capacity(out_width);
                        c.extend_from_slice(build_row);
                        c.extend_from_slice(&nulls_right);
                        out_rows.push(c);
                    }
                }
            }
        }
    } else if !build_left && matches!(join_type, crate::sql::ast::JoinType::Right | crate::sql::ast::JoinType::Full) {
        for (bi, build_row) in build_rows.iter().enumerate() {
            if !build_matched[bi] {
                match proj_indices {
                    Some(idxs) => {
                        let mut out = Vec::with_capacity(idxs.len());
                        for &p in idxs {
                            let v = if p < n_left {
                                &Value::Null
                            } else {
                                // Right side is the build (preserved) row.
                                &build_row[p - n_left]
                            };
                            out.push(v.clone());
                        }
                        out_rows.push(out);
                    }
                    None => {
                        let mut c: Row = Vec::with_capacity(out_width);
                        c.extend_from_slice(&nulls_left);
                        c.extend_from_slice(build_row);
                        out_rows.push(c);
                    }
                }
            }
        }
    }

    let result = match &proj {
        Some((_, names)) => ExecResult { columns: names.clone().into(), rows: out_rows },
        None => ExecResult { columns: combined_cols.into(), rows: out_rows },
    };
    // When a projection was requested but fusion couldn't fire (residual
    // predicates, non-column projections, unresolvable names), apply the
    // projection normally on top of the full-width join output.
    match (projection, proj.is_some()) {
        (Some(cols), false) => apply_projection(result, cols, ctx),
        _ => Ok(result),
    }
}

/// If `cond` is a pure conjunction of `l.col = r.col` predicates (possibly
/// just one), returns the number of such equality leaves. Otherwise returns
/// None (the condition contains residual terms that must be evaluated per
/// candidate row). Conservative: any leaf that does not unambiguously
/// resolve to one column on each side disqualifies purity — the join then
/// keeps per-row residual evaluation (correct, slightly slower).
fn count_eq_leaves_and_purity(condition: &Option<Expr>, left_cols: &[String], right_cols: &[String]) -> Option<usize> {
    fn walk(e: &Expr, lc: &[String], rc: &[String]) -> Option<usize> {
        match e {
            Expr::Binary { op: BinaryOp::And, left, right } => {
                Some(walk(left, lc, rc)? + walk(right, lc, rc)?)
            }
            Expr::Binary { op: BinaryOp::Eq, left, right } => {
                let l_in_left = col_index(left, lc).is_some();
                let l_in_right = col_index(left, rc).is_some();
                let r_in_left = col_index(right, lc).is_some();
                let r_in_right = col_index(right, rc).is_some();
                let unambiguous_one_each =
                    ((l_in_left && !l_in_right) && (r_in_right && !r_in_left))
                        || ((l_in_right && !l_in_left) && (r_in_left && !r_in_right));
                if unambiguous_one_each { Some(1) } else { None }
            }
            _ => None,
        }
    }
    condition.as_ref().and_then(|c| walk(c, left_cols, right_cols))
}

/// Extract equi-join key column pairs from a join condition.
/// Returns a list of (left_col_index, right_col_index) pairs.
/// Handles a single `col = col` or an AND chain of equalities.
fn extract_equi_join_keys(
    condition: &Option<Expr>,
    left_cols: &[String],
    right_cols: &[String],
) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    if let Some(cond) = condition {
        collect_eq_pairs(cond, left_cols, right_cols, &mut pairs);
    }
    pairs
}

fn collect_eq_pairs(
    expr: &Expr,
    left_cols: &[String],
    right_cols: &[String],
    pairs: &mut Vec<(usize, usize)>,
) {
    match expr {
        Expr::Binary { op: BinaryOp::And, left, right } => {
            collect_eq_pairs(left, left_cols, right_cols, pairs);
            collect_eq_pairs(right, left_cols, right_cols, pairs);
        }
        Expr::Binary { op: BinaryOp::Eq, left, right } => {
            // Try left.col = right.col
            if let (Some(l_idx), Some(r_idx)) = (
                col_index(left, left_cols),
                col_index(right, right_cols),
            ) {
                pairs.push((l_idx, r_idx));
                return;
            }
            // Try right.col = left.col
            if let (Some(r_idx), Some(l_idx)) = (
                col_index(left, right_cols),
                col_index(right, left_cols),
            ) {
                pairs.push((l_idx, r_idx));
            }
        }
        _ => {}
    }
}

/// Find the index of a column reference in a column list.
/// Handles both qualified (`alias.col`) and unqualified (`col`) references.
///
/// **Qualifier semantics** (matches SQLite): when the expression carries a
/// table qualifier (e.g. `b.id`), we ONLY match columns whose prefix matches
/// that qualifier. This prevents `b.id` from spuriously matching `c.id` in a
/// 3-way join where both sides have a column named `id`.
fn col_index(expr: &Expr, cols: &[String]) -> Option<usize> {
    if let Expr::Column { table, name } = expr {
        match table {
            // Qualified reference: only consider columns whose table prefix
            // matches `table`.
            Some(t) => {
                for (i, c) in cols.iter().enumerate() {
                    // c is typically "table.column" — split off the prefix.
                    if let Some(pos) = c.rfind('.') {
                        let (prefix, col_name) = (&c[..pos], &c[pos + 1..]);
                        if prefix.eq_ignore_ascii_case(t)
                            && col_name.eq_ignore_ascii_case(name)
                        {
                            return Some(i);
                        }
                    }
                }
                None
            }
            // Unqualified: exact match first (handles names like "id" against
            // "id" — but our cols are usually "alias.id"). Then suffix match.
            None => {
                // Exact match on the whole column string.
                for (i, c) in cols.iter().enumerate() {
                    if c.eq_ignore_ascii_case(name) {
                        return Some(i);
                    }
                }
                // Suffix match (e.g. "u.id" matches "id"). On ambiguity, the
                // FIRST match wins — SQLite's behaviour is similar.
                for (i, c) in cols.iter().enumerate() {
                    if let Some(pos) = c.rfind('.') {
                        if c[pos + 1..].eq_ignore_ascii_case(name) {
                            return Some(i);
                        }
                    }
                }
                None
            }
        }
    } else {
        None
    }
}

// ============================================================================
// Index nested-loop join
// ============================================================================

/// Index nested-loop join: for each outer row, look up matching inner rows
/// via the inner table's secondary index. This is the optimal plan for
/// `JOIN inner ON outer.k = inner.k` when `inner.k` is indexed and the
/// outer side is highly selective (e.g. filtered by `WHERE outer.k = ?`).
///
/// Unlike `exec_hash_join`, this never materializes the full inner table —
/// it only fetches the inner rows that actually match an outer row. For
/// the canonical case `SELECT ... FROM users u JOIN orders o ON u.id =
/// o.user_id WHERE u.id = ?` with `idx_orders_user` on `orders(user_id)`,
/// this means ~10 inner-row lookups instead of decoding all 10k orders.
fn exec_index_nested_loop_join(
    ctx: &mut ExecContext<'_>,
    outer_plan: &Plan,
    inner_table: Arc<Table>,
    inner_alias: Option<String>,
    inner_index: Arc<crate::schema::Index>,
    outer_key_col: usize,
    projection: Option<&[crate::planner::plan::ProjectExpr]>,
) -> Result<ExecResult> {
    let outer_res = execute(outer_plan, ctx)?;
    let n_inner_cols = inner_table.n_columns();

    // Output columns: outer.cols ++ inner.cols (with inner prefix from alias).
    let inner_prefix = inner_alias.as_deref().unwrap_or(&inner_table.name);
    let mut combined_cols: Vec<String> = outer_res.columns.to_vec();
    combined_cols.extend(
        inner_table.columns.iter().map(|c| format!("{}.{}", inner_prefix, c.name)),
    );

    // ---- FUSED PROJECTION resolution ----
    // When this join sits under a Project of bare column references, the
    // join emits ONLY those columns directly: no full-width combined row,
    // no second cloning pass, and text values are cloned once (into the
    // output) instead of twice (combined row, then projection).
    let mut fused: Option<(Vec<usize>, Arc<[String]>)> = None;
    if let Some(columns) = projection {
        if !columns.is_empty()
            && columns.iter().all(|c| {
                matches!(&c.expr, Expr::Column { name, .. } if name != "*")
            })
        {
            let mut indices = Vec::with_capacity(columns.len());
            let mut names = Vec::with_capacity(columns.len());
            let mut all_ok = true;
            for c in columns {
                if let Expr::Column { table, name } = &c.expr {
                    match resolve_column_index(&combined_cols, table.as_deref(), name) {
                        Some(i) => {
                            indices.push(i);
                            names.push(match &c.alias {
                                Some(a) => a.clone(),
                                None => expr_display_name(&c.expr),
                            });
                        }
                        None => {
                            all_ok = false;
                            break;
                        }
                    }
                } else {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                fused = Some((indices, names.into()));
            }
        }
    }

    let mut out_rows: Vec<Row> = Vec::new();
    let inner_root = ctx.table_root(&inner_table);
    // Hoist the index root lookup out of the per-row loop: resolving it
    // per row cost a `to_ascii_lowercase()` String allocation + hash per
    // outer row. Index roots only move on B+tree splits; re-read only if
    // something actually changed roots mid-join (an UPDATE source writing
    // the same table).
    let mut inner_index_root = ctx.index_root(&inner_index);
    let outer_width = outer_res.columns.len();
    let total_width = outer_width + n_inner_cols;
    let mut key_buf: Vec<u8> = Vec::with_capacity(16);
    // Dedup set, allocated lazily ONLY when a probe returns multiple
    // rowids (defensive against index corruption; single-rowid probes —
    // the overwhelmingly common case — skip the HashSet allocation).
    let mut seen: Option<std::collections::HashSet<i64>> = None;

    for outer_row in &outer_res.rows {
        // Extract the join key from the outer row.
        let key_value = match outer_row.get(outer_key_col) {
            Some(v) => v.clone(),
            None => continue, // NULL join key — no matches in INNER join.
        };

        // Encode the key for index lookup (order-preserving form), reusing
        // the buffer across rows.
        key_buf.clear();
        key_value.encode_order_key_into(&mut key_buf);

        // Look up matching rowids in the index B+tree.
        let mut index_bt = Btree::new(ctx.pager, inner_index_root, true);
        let rowids = index_bt.lookup_index(&key_buf)?;
        if index_bt.root != inner_index_root {
            inner_index_root = index_bt.root;
        }

        // Fetch each matching row from the inner table, decoding directly
        // under the page lock (no intermediate payload Vec copy).
        let dedup_needed = rowids.len() > 1;
        for rowid in rowids {
            if dedup_needed {
                let set = seen.get_or_insert_with(std::collections::HashSet::new);
                if !set.insert(rowid) {
                    continue;
                }
            }
            let mut table_bt = Btree::new(ctx.pager, inner_root, false);
            if let Some(inner_row) = table_bt.lookup_table_with(rowid, |payload| {
                decode_row(payload, n_inner_cols, rowid, inner_table.rowid_alias)
            })? {
                match &fused {
                    Some((indices, _)) => {
                        // Emit ONLY the projected columns.
                        let mut out: Row = Vec::with_capacity(indices.len());
                        for &i in indices {
                            let v = if i < outer_width {
                                &outer_row[i]
                            } else {
                                &inner_row[i - outer_width]
                            };
                            out.push(v.clone());
                        }
                        out_rows.push(out);
                    }
                    None => {
                        // Single allocation for the combined row (was: clone the
                        // outer row into a fresh Vec, then grow it with extend —
                        // two allocations and two copies per output row).
                        let mut combined: Row = Vec::with_capacity(total_width);
                        combined.extend_from_slice(outer_row);
                        combined.extend_from_slice(&inner_row);
                        out_rows.push(combined);
                    }
                }
            }
        }
    }

    let out_columns: Arc<[String]> = match &fused {
        Some((_, names)) => names.clone(),
        None => combined_cols.into(),
    };
    Ok(ExecResult { columns: out_columns, rows: out_rows })
}

// ============================================================================
// Distinct
// ============================================================================

fn exec_distinct(ctx: &mut ExecContext<'_>, input: &Plan) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let mut seen: Vec<Row> = Vec::new();
    inner.rows.retain(|r| {
        if seen.contains(r) {
            false
        } else {
            seen.push(r.clone());
            true
        }
    });
    Ok(inner)
}

// ============================================================================
// Set operations
// ============================================================================

fn exec_union(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan, all: bool) -> Result<ExecResult> {
    let l = execute(left, ctx)?;
    let r = execute(right, ctx)?;
    let columns = l.columns.clone();
    let mut rows = l.rows;
    if all {
        rows.extend(r.rows);
    } else {
        let mut seen: Vec<Row> = Vec::new();
        for row in rows.iter().chain(r.rows.iter()) {
            if !seen.contains(row) {
                seen.push(row.clone());
            }
        }
        rows = seen;
    }
    Ok(ExecResult { columns, rows })
}

fn exec_intersect(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan) -> Result<ExecResult> {
    let l = execute(left, ctx)?;
    let r = execute(right, ctx)?;
    let columns = l.columns.clone();
    let mut seen: Vec<Row> = Vec::new();
    for row in &l.rows {
        if r.rows.contains(row) && !seen.contains(row) {
            seen.push(row.clone());
        }
    }
    Ok(ExecResult { columns, rows: seen })
}

fn exec_except(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan) -> Result<ExecResult> {
    let l = execute(left, ctx)?;
    let r = execute(right, ctx)?;
    let columns = l.columns.clone();
    let mut seen: Vec<Row> = Vec::new();
    for row in &l.rows {
        if !r.rows.contains(row) && !seen.contains(row) {
            seen.push(row.clone());
        }
    }
    Ok(ExecResult { columns, rows: seen })
}

// ============================================================================
// RowidLookup
// ============================================================================

fn exec_rowid_lookup(ctx: &mut ExecContext<'_>, table: Arc<Table>, rowid_expr: &Expr) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let rowid = evaluate(rowid_expr, &eval_ctx)?.as_integer();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let row = match bt.lookup_table(rowid)? {
        LookupResult::Found(payload) => decode_row(&payload, table.n_columns(), rowid, table.rowid_alias)?,
        LookupResult::NotFound => return Ok(ExecResult {
            columns: table.col_names.clone(),
            rows: Vec::new(),
        }),
    };
    Ok(ExecResult {
        columns: table.col_names.clone(),
        rows: vec![row],
    })
}

// ============================================================================
// RowidRange (WHERE id BETWEEN ? AND ?, id > ?, id < ?, id >= ?, id <= ?)
// ============================================================================

fn exec_rowid_range(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    start_expr: Option<&Expr>,
    end_expr: Option<&Expr>,
    residual: Option<&Expr>,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);

    // Evaluate the bounds. None means unbounded (-∞ or +∞).
    let start: i64 = match start_expr {
        Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
        None => i64::MIN,
    };
    let end: i64 = match end_expr {
        Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
        None => i64::MAX,
    };

    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let n_cols = table.n_columns();
    let mut rows: Vec<Row> = Vec::new();
    // `scan_table_range` is inclusive on both ends. For strict `>` / `<`
    // conjuncts, the planner kept a residual predicate that re-checks the
    // strict comparison; we apply it here so we don't accidentally include
    // the boundary.
    // Use borrowed scan — skip per-row Cell::decode Vec<u8> allocation.
    // decode_row itself still allocates a Vec<Value> per row (unavoidable
    // without restructuring the API to return iterators), but the payload
    // borrow eliminates one allocation per row.
    let rowid_alias = table.rowid_alias;
    bt.scan_table_range_borrowed(start, end, |rowid, payload| {
        if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
            rows.push(row);
        }
        true
    })?;

    // Apply residual predicate (strict < / > bounds, or additional filters).
    // Cached plain column names — one refcount bump, no per-query clones.
    let columns: Arc<[String]> = table.col_names.clone();
    if let Some(res) = residual {
        let params: &[Value] = &ctx.params;
        let named_params = &ctx.named_params;
        rows.retain(|row| {
            match eval_row(res, row, &columns, params, named_params) {
                Ok(v) => v.is_truthy(),
                Err(_) => false,
            }
        });
    }

    Ok(ExecResult { columns, rows })
}

/// Identity position table: `positions[i] == i` for every column index
/// up to MAX_TABLE_COLUMNS. `process_update_row` evaluates compiled
/// residual predicates against the FULL table-order row buffer, so column
/// i of the table is at position i of the slice. A lazily-built static
/// (const-friendly) avoids re-allocating a `Vec<usize>` per statement —
/// the old path built `(0..n_cols).collect()` per call.
const IDENTITY_POSITIONS: &[usize] = &{
    let mut a = [0usize; 1024];
    let mut i = 0;
    while i < 1024 {
        a[i] = i;
        i += 1;
    }
    a
};

/// Resolve a projection list against a base table: returns
/// `(project, out_cols)` where `project == None` means identity (SELECT *,
/// decode the full row in table order) and `Some(indices)` means decode only
/// those table column indices. Returns None unless EVERY projection expr is
/// a bare column of the table (or the single "*" pseudo-column).
fn bare_column_projection(columns: &[ProjectExpr], table: &Table) -> Option<(Option<Vec<usize>>, Arc<[String]>)> {
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

/// RowidRange with the projection FUSED into the scan: each row decodes
/// only the projected columns (selective decode) — no full-row decode, no
/// second cloning pass. `project == None` decodes the full row and moves it.
fn exec_rowid_range_projected(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    start_expr: Option<&Expr>,
    end_expr: Option<&Expr>,
    project: Option<&[usize]>,
    out_cols: Arc<[String]>,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let start: i64 = match start_expr {
        Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
        None => i64::MIN,
    };
    let end: i64 = match end_expr {
        Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
        None => i64::MAX,
    };
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let n_cols = table.n_columns();
    let rowid_alias = table.rowid_alias;
    let mut rows: Vec<Row> = Vec::new();
    bt.scan_table_range_borrowed(start, end, |rowid, payload| {
        match project {
            Some(idxs) => {
                let mut row = Vec::with_capacity(idxs.len());
                if decode_row_selective(payload, n_cols, idxs, rowid, rowid_alias, &mut row).is_ok() {
                    rows.push(row);
                }
            }
            None => {
                if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
                    rows.push(row);
                }
            }
        }
        true
    })?;
    Ok(ExecResult { columns: out_cols, rows })
}

/// RowidLookup with the projection fused (mirror of the api.rs fast path
/// for statements that go through the general executor).
fn exec_rowid_lookup_projected(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    rowid_expr: &Expr,
    project: Option<&[usize]>,
    out_cols: Arc<[String]>,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let rowid = evaluate(rowid_expr, &eval_ctx)?.as_integer();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let n_cols = table.n_columns();
    let rowid_alias = table.rowid_alias;
    match bt.lookup_table(rowid)? {
        LookupResult::Found(payload) => {
            let row = match project {
                Some(idxs) => {
                    let mut out = Vec::with_capacity(idxs.len());
                    decode_row_selective(&payload, n_cols, idxs, rowid, rowid_alias, &mut out)?;
                    out
                }
                None => decode_row(&payload, n_cols, rowid, rowid_alias)?,
            };
            Ok(ExecResult { columns: out_cols, rows: vec![row] })
        }
        LookupResult::NotFound => Ok(ExecResult { columns: out_cols, rows: Vec::new() }),
    }
}

// ============================================================================
// IndexLookup (WHERE indexed_col = ?)
// ============================================================================

fn exec_index_lookup(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    index: Arc<crate::schema::Index>,
    key_exprs: &[Expr],
) -> Result<ExecResult> {
    // Evaluate the key expressions to get the lookup values.
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let key_values: Vec<Value> = key_exprs.iter()
        .map(|e| evaluate(e, &eval_ctx))
        .collect::<Result<_>>()?;

    // Encode the key: concatenate the order-preserving encoded form of each
    // indexed column value (must match encode_index_key's encoding).
    let mut key_bytes = Vec::new();
    for v in &key_values {
        v.encode_order_key_into(&mut key_bytes);
    }

    // Look up matching rowids in the index (override-aware root).
    let index_root = ctx.index_root(&index);
    let mut index_bt = Btree::new(ctx.pager, index_root, true);
    let rowids = index_bt.lookup_index(&key_bytes)?;

    // Fetch each row by rowid from the table B+tree. Use the
    // override-aware root: the catalog's Arc<Table> holds the root from
    // CREATE TABLE time, but splits may have moved the actual root
    // (tracked in ctx.root_overrides). Using the stale root made rows
    // beyond the first subtree invisible to indexed lookups after the
    // table had grown.
    let table_root = ctx.table_root(&table);
    let mut rows = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        let mut table_bt = Btree::new(ctx.pager, table_root, false);
        if let LookupResult::Found(payload) = table_bt.lookup_table(rowid)? {
            rows.push(decode_row(&payload, table.n_columns(), rowid, table.rowid_alias)?);
        }
    }

    Ok(ExecResult {
        columns: table.columns.iter().map(|c| c.name.clone()).collect(),
        rows,
    })
}

/// Compare an index entry key against a bound's encoded value, considering
/// only the FIRST indexed column's worth of bytes (the bound constrains the
/// leading column; composite keys with a matching prefix compare Equal).
fn index_key_prefix_cmp(cell_key: &[u8], bound: &[u8]) -> std::cmp::Ordering {
    let n = cell_key.len().min(bound.len());
    match cell_key[..n].cmp(&bound[..n]) {
        std::cmp::Ordering::Equal if cell_key.len() >= bound.len() => std::cmp::Ordering::Equal,
        other => other,
    }
}

/// Execute an IndexRange: scan the index B+tree between the (encoded) bounds,
/// collect the matching rowids, fetch the rows by rowid, and apply any
/// residual predicate. Rows come back in index order (ascending by the
/// indexed value), matching SQLite's index-scan output order.
#[allow(clippy::type_complexity)]
fn exec_index_range(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    index: Arc<crate::schema::Index>,
    start: Option<&(Expr, bool)>,
    end: Option<&(Expr, bool)>,
    residual: Option<&Expr>,
) -> Result<ExecResult> {
    // Evaluate the bound expressions (they are constants / parameters).
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let start_key: Option<(Vec<u8>, bool)> = match start {
        Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
        None => None,
    };
    let end_key: Option<(Vec<u8>, bool)> = match end {
        Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
        None => None,
    };

    // Scan the index from the start bound.
    let scan_start: Vec<u8> = start_key.as_ref().map(|(k, _)| k.clone()).unwrap_or_default();
    let mut rowids: Vec<i64> = Vec::new();
    {
        let index_root = ctx.index_root(&index);
        let mut index_bt = Btree::new(ctx.pager, index_root, true);
        index_bt.scan_index_from(&scan_start, |rowid, cell_key| {
            // Exclusive lower bound: skip entries whose leading column
            // equals the bound (they share the bound's key prefix).
            if let Some((k, false)) = &start_key {
                if cell_key.starts_with(k) {
                    return true;
                }
            }
            // Upper bound: stop past it.
            if let Some((k, inc)) = &end_key {
                match index_key_prefix_cmp(cell_key, k) {
                    std::cmp::Ordering::Greater => return false,
                    std::cmp::Ordering::Equal if !*inc => return false,
                    _ => {}
                }
            }
            rowids.push(rowid);
            true
        })?;
    }

    // Fetch the rows by rowid and apply the residual predicate.
    // Cached qualified names — one refcount bump instead of N `format!()`s.
    let columns: Arc<[String]> = if alias.as_deref().unwrap_or(&table.name) == table.name {
        table.qualified_col_names.clone()
    } else {
        let prefix = alias.as_deref().unwrap_or(&table.name);
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect::<Vec<String>>()
            .into()
    };
    let plain_names: Arc<[String]> = table.col_names.clone();
    let table_root = ctx.table_root(&table);
    let mut rows = Vec::with_capacity(rowids.len());

    // Fetch the rows by rowid. Two strategies:
    //
    // - RANDOM LOOKUPS (one B+tree descent per rowid) when the selection is
    //   a small fraction of the table.
    // - MERGE SCAN when the selection is large: sort the rowids and scan
    //   the table B+tree ONCE in rowid order, merge-matching against the
    //   sorted rowid list. A random lookup costs ~300 ns (full descent +
    //   binary search); a sequential scan costs ~60-80 ns per visited row.
    //   Selecting 5000 of 10000 rows: 1.5 ms of descents vs ~0.7 ms of
    //   sequential scan — the crossover is around 20-25% of the table.
    //   `max_rowid` (cached) approximates the row count.
    let max_rowid_hint = ctx.get_or_scan_max_rowid(&table).unwrap_or(0);
    let use_merge_scan = max_rowid_hint > 0
        && (rowids.len() as i64) * 4 > max_rowid_hint
        && residual.is_none(); // residual needs full rows in index order? No —
    // (residual is fine with merge scan too, but the output ORDER changes:
    // merge scan emits rows in ROWID order, random lookups emit in INDEX
    // order. Keep order stability only for the no-residual case... actually
    // both are unordered bags for SQL without ORDER BY; residual is safe.
    // We keep the residual restriction for simplicity of reasoning about
    // filter semantics on partially-indexed predicates.)

    if use_merge_scan {
        // Preserve the observable emission order (index order) even though
        // rows are FETCHED in rowid order: remember each rowid's original
        // position, then place decoded rows into a position-indexed slot.
        let index_order: Vec<i64> = rowids.clone();
        let mut position: std::collections::HashMap<i64, usize, crate::storage::pager::PageIdHashBuild> =
            std::collections::HashMap::with_capacity_and_hasher(
                rowids.len(),
                crate::storage::pager::PageIdHashBuild,
            );
        for (pos, rid) in index_order.iter().enumerate() {
            position.insert(*rid, pos);
        }
        let mut placed: Vec<Option<Row>> = vec![None; index_order.len()];
        rowids.sort_unstable();
        rowids.dedup();
        let mut ri = 0usize;
        let n_cols = table.n_columns();
        let params = ctx.params.clone();
        let named_params = ctx.named_params.clone();
        let residual_pred = residual;
        let mut bt = Btree::new(ctx.pager, table_root, false);
        bt.scan_table_borrowed(|rowid, payload| {
            // Advance the merge cursor.
            while ri < rowids.len() && rowids[ri] < rowid {
                ri += 1;
            }
            if ri >= rowids.len() {
                return false; // all matches emitted — stop the scan early
            }
            if rowids[ri] != rowid {
                return true; // not a match — keep scanning
            }
            ri += 1;
            if let Ok(row) = decode_row(payload, n_cols, rowid, table.rowid_alias) {
                let keep = match residual_pred {
                    Some(pred) => match eval_row(pred, &row, &plain_names, &params, &named_params) {
                        Ok(v) => v.is_truthy(),
                        Err(_) => false,
                    },
                    None => true,
                };
                if keep {
                    if let Some(&pos) = position.get(&rowid) {
                        placed[pos] = Some(row);
                    }
                }
            }
            true
        })?;
        for slot in placed {
            if let Some(row) = slot {
                rows.push(row);
            }
        }
    } else {
        for rowid in rowids {
            let mut table_bt = Btree::new(ctx.pager, table_root, false);
            if let LookupResult::Found(payload) = table_bt.lookup_table(rowid)? {
                let row = decode_row(&payload, table.n_columns(), rowid, table.rowid_alias)?;
                if let Some(pred) = residual {
                    let v = eval_row(pred, &row, &plain_names, &ctx.params, &ctx.named_params)?;
                    if !v.is_truthy() {
                        continue;
                    }
                }
                rows.push(row);
            }
        }
    }

    Ok(ExecResult { columns, rows })
}

// ============================================================================
// INSERT
// ============================================================================

/// Per-statement index maintenance state for the INSERT hot paths.
///
/// Replaces the old `Vec<(Arc<Index>, u32)>` tuple list with a struct that
/// pre-resolves each index's column positions ONCE per statement (the old
/// `encode_index_key` ran `table.find_column()` — a case-insensitive scan
/// over the table's columns — for every indexed column of every row),
/// carries a reusable key-encoding buffer (no per-row `Vec<u8>` alloc),
/// and holds a validated APPEND HINT for the index B+tree so ascending
/// bulk loads skip the root-to-leaf descent + binary search per entry.
pub(crate) struct IndexMaintState {
    pub idx: Arc<crate::schema::Index>,
    pub root: u32,
    /// Resolved column positions (`usize::MAX` = column dropped/renamed;
    /// contributes nothing to the key, mirroring `encode_index_key`).
    pub cols: Vec<usize>,
    /// Pinned right-most index leaf from the previous append in this
    /// statement (see `insert_index_append_hinted`).
    pub hint: Option<PageId>,
    /// Reusable order-key encoding buffer.
    pub key_buf: Vec<u8>,
}

impl IndexMaintState {
    /// Encode `row`'s index key into `buf` (cleared first) using the
    /// pre-resolved column positions — zero allocations, no name lookups.
    #[inline]
    pub fn encode_key(&mut self, row: &[Value]) -> &[u8] {
        self.key_buf.clear();
        for &pos in &self.cols {
            if let Some(v) = row.get(pos) {
                v.encode_order_key_into(&mut self.key_buf);
            }
        }
        &self.key_buf
    }
}

/// Build the per-statement index maintenance states (resolved columns,
/// current roots) for every index on `table`.
pub(crate) fn make_index_states(
    ctx: &ExecContext<'_>,
    indexes: &[Arc<crate::schema::Index>],
    table: &Table,
) -> Vec<IndexMaintState> {
    indexes
        .iter()
        .map(|idx| {
            let cols: Vec<usize> = idx
                .columns
                .iter()
                .map(|c| table.find_column(&c.name).unwrap_or(usize::MAX))
                .collect();
            IndexMaintState {
                idx: idx.clone(),
                root: ctx.index_root(idx),
                cols,
                hint: None,
                key_buf: Vec::with_capacity(16),
            }
        })
        .collect()
}

fn exec_insert(ctx: &mut ExecContext<'_>, table: Arc<Table>, source: &Plan, columns: Option<Vec<usize>>, on_conflict: ConflictResolution, upsert: Option<&crate::sql::ast::UpsertClause>, returning: Option<&[crate::sql::ast::ResultColumn]>) -> Result<ExecResult> {
    let target_indices: Vec<usize> = columns.unwrap_or_else(|| (0..table.n_columns()).collect());
    // Track the current root page — it may change if the B+tree splits.
    let mut current_root = ctx.table_root(&table);
    let mut max_rowid = ctx.get_or_scan_max_rowid(&table)?;
    let mut inserted = 0i64;

    // Look up indexes on this table once, up front.
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    // Track current root for each index too (seeded from the override-aware
    // roots — the catalog snapshot may be stale after earlier splits).
    let mut index_states = make_index_states(ctx, &indexes, &table);

    // Pre-compute the lower-cased table name ONCE — set_table_root and
    // set_max_rowid both call to_ascii_lowercase() per call, which
    // allocates a fresh String per row. For a 1k-row INSERT batch, that's
    // 2k wasted String allocations.
    let table_name_lc = table.name.to_ascii_lowercase();

    // Reusable row + payload buffers. Hoisted outside the loop to avoid
    // a per-row Vec allocation. full_row is sized to table.n_columns() and
    // filled with NULL at the start of each iteration.
    let n_cols = table.n_columns();
    let mut full_row: Vec<Value> = vec![Value::Null; n_cols];
    let mut payload_buf: Vec<u8> = Vec::with_capacity(n_cols * 8);
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();

    // Column names — needed only for CHECK constraints or RETURNING
    // (NOT NULL checks are purely positional, so the common case of a
    // table with a NOT NULL PK but no CHECKs skips the per-statement
    // Vec<String> allocation entirely).
    let needs_col_names = !table.check_exprs.is_empty() || returning.is_some();
    let col_names: Vec<String> = if needs_col_names {
        table.columns.iter().map(|c| c.name.clone()).collect()
    } else {
        Vec::new()
    };

    // RETURNING output buffer.
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();

    // Fast path: source is a Plan::Values. Skip exec_values' column-name
    // String allocations (which INSERT doesn't need) and the intermediate
    // Vec<Vec<Value>> allocation. For a 1k-statement INSERT batch (1 row
    // per statement), this saves 1k Vec<String> + 2k String allocations
    // (column1/column2) + 1k Vec<Vec<Value>> overheads.
    if let Plan::Values { rows: expr_rows } = source {
        for exprs in expr_rows {
            // Reset the row buffer to all-NULLs without releasing capacity.
            for v in full_row.iter_mut() {
                *v = Value::Null;
            }
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            for (i, expr) in exprs.iter().enumerate() {
                if i < target_indices.len() {
                    let col_idx = target_indices[i];
                    let val = evaluate(expr, &eval_ctx)?;
                    let affinity = table.columns[col_idx].affinity;
                    full_row[col_idx] = affinity.coerce(val);
                }
            }
            // Apply column defaults.
            for (i, col) in table.columns.iter().enumerate() {
                if full_row[i].is_null() && col.default.is_some() {
                    if let Some(default_expr) = &col.default {
                        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
                        full_row[i] = evaluate(default_expr, &eval_ctx)?;
                    }
                }
            }

            // Assign the rowid BEFORE constraint enforcement so that a NULL
            // rowid-alias (auto-assign) doesn't trip NOT NULL (SQLite treats
            // NULL on INTEGER PRIMARY KEY as "allocate a new rowid").
            let mut rowid_autogen = false;
            if let Some(idx) = table.rowid_alias {
                if full_row[idx].is_null() {
                    max_rowid += 1;
                    full_row[idx] = Value::Integer(max_rowid);
                    rowid_autogen = true;
                }
            }

            // NOT NULL + CHECK constraints. Always enforced: the NOT NULL
            // loop is purely positional (no names needed); col_names is
            // only consulted for CHECK expressions (empty when absent).
            enforce_row_constraints(&table, &full_row, &col_names, &ctx.params, &ctx.named_params)?;
            // FOREIGN KEY (child side) — enforced only when the pragma is on.
            enforce_child_fks(ctx, &table, &full_row)?;

            // BEFORE INSERT triggers (NEW = the row about to be inserted,
            // rowid assigned + constraints enforced). An error aborts the
            // statement before the row is written.
            if crate::executor::triggers::has_triggers_for(ctx, &table, &crate::sql::ast::TriggerEvent::Insert) {
                crate::executor::triggers::fire_triggers(
                    ctx,
                    &table,
                    &crate::sql::ast::TriggerEvent::Insert,
                    crate::sql::ast::TriggerWhen::Before,
                    Some(&full_row),
                    None,
                    &table.col_names,
                )?;
                // Triggers may execute arbitrary SQL against this table
                // through the generic path (which doesn't know about our
                // append hints) — the pinned leaves may have split or been
                // freed. Invalidate every hint; they re-pin on next use.
                for st in index_states.iter_mut() {
                    st.hint = None;
                }
                ctx.table_append_hint = None;
            }

            let outcome = exec_insert_one_row(
                ctx, &table, &table_name_lc, &mut current_root, &mut max_rowid,
                &mut full_row, &mut payload_buf, &mut index_states, on_conflict, upsert, rowid_autogen,
            )?;
            let trigger_fired = matches!(outcome, InsertOutcome::Inserted | InsertOutcome::UpdatedExisting);
            match outcome {
                InsertOutcome::Inserted => {
                    inserted += 1;
                    if let Some(ret) = returning {
                        returning_rows.push(project_returning_row(ret, &full_row, &col_names, &ctx.params, &ctx.named_params)?);
                    }
                }
                InsertOutcome::UpdatedExisting => {
                    inserted += 1;
                    if let Some(ret) = returning {
                        returning_rows.push(project_returning_row(ret, &full_row, &col_names, &ctx.params, &ctx.named_params)?);
                    }
                }
                InsertOutcome::Skipped => {}
            }
            // AFTER INSERT triggers (skip the catalog lookup entirely when
            // the table has none).
            if trigger_fired
                && crate::executor::triggers::has_triggers_for(ctx, &table, &crate::sql::ast::TriggerEvent::Insert)
            {
                crate::executor::triggers::fire_triggers(
                    ctx,
                    &table,
                    &crate::sql::ast::TriggerEvent::Insert,
                    crate::sql::ast::TriggerWhen::After,
                    Some(&full_row),
                    None,
                    &table.col_names,
                )?;
                // Same hint invalidation as BEFORE triggers.
                for st in index_states.iter_mut() {
                    st.hint = None;
                }
                ctx.table_append_hint = None;
            }
        }
        // Write back any index-root moves (splits) to the context so the
        // NEXT statement descends from the current root — the catalog's
        // Arc<Index> still holds the CREATE-time root.
        for st in index_states.iter() {
            if ctx.index_root(&st.idx) != st.root {
                ctx.set_index_root(&st.idx.name, st.root);
            }
        }
        if !ctx.in_transaction && !ctx.deferred_flush {
            ctx.pager.flush()?;
        }
        return Ok(finish_insert_result(inserted, returning, &col_names, returning_rows));
    }

    // Slow path: source is something else (subquery, etc.) — go through
    // the generic execute() path.
    let source_res = execute(source, ctx)?;
    for row in &source_res.rows {
        // Reset the row buffer to all-NULLs without releasing capacity.
        // `Value::Null` is a no-op to drop, so this is just a memset.
        for v in full_row.iter_mut() {
            *v = Value::Null;
        }
        for (i, val) in row.iter().enumerate() {
            if i < target_indices.len() {
                let col_idx = target_indices[i];
                let affinity = table.columns[col_idx].affinity;
                full_row[col_idx] = affinity.coerce(val.clone());
            }
        }

        // Apply column defaults.
        for (i, col) in table.columns.iter().enumerate() {
            if full_row[i].is_null() && col.default.is_some() {
                let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
                if let Some(default_expr) = &col.default {
                    full_row[i] = evaluate(default_expr, &eval_ctx)?;
                }
            }
        }

        // Assign the rowid BEFORE constraint enforcement (see fast path).
        let mut rowid_autogen = false;
        if let Some(idx) = table.rowid_alias {
            if full_row[idx].is_null() {
                max_rowid += 1;
                full_row[idx] = Value::Integer(max_rowid);
                rowid_autogen = true;
            }
        }

        // NOT NULL + CHECK constraints (see fast path).
        enforce_row_constraints(&table, &full_row, &col_names, &ctx.params, &ctx.named_params)?;
        enforce_child_fks(ctx, &table, &full_row)?;

        let outcome = exec_insert_one_row(
            ctx, &table, &table_name_lc, &mut current_root, &mut max_rowid,
            &mut full_row, &mut payload_buf, &mut index_states, on_conflict, upsert, rowid_autogen,
        )?;
        match outcome {
            InsertOutcome::Inserted | InsertOutcome::UpdatedExisting => {
                inserted += 1;
                if let Some(ret) = returning {
                    returning_rows.push(project_returning_row(ret, &full_row, &col_names, &ctx.params, &ctx.named_params)?);
                }
            }
            InsertOutcome::Skipped => {}
        }
    }
        // Write back any index-root moves (splits) to the context so the
        // NEXT statement descends from the current root — the catalog's
        // Arc<Index> still holds the CREATE-time root.
        for st in index_states.iter() {
            if ctx.index_root(&st.idx) != st.root {
                ctx.set_index_root(&st.idx.name, st.root);
            }
        }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    Ok(finish_insert_result(inserted, returning, &col_names, returning_rows))
}

/// Build the final ExecResult for an INSERT (with or without RETURNING).
/// Shared column names for the non-RETURNING INSERT result shape
/// `["inserted"]`. Built once — previously every INSERT statement allocated
/// a fresh String + Vec for this, immediately discarded by the execute()
/// path (which ignores the result). ~4 allocations per statement saved.
static INSERTED_COLS: std::sync::OnceLock<Arc<[String]>> = std::sync::OnceLock::new();

fn finish_insert_result(
    inserted: i64,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
    col_names: &[String],
    returning_rows: Vec<Vec<Value>>,
) -> ExecResult {
    if let Some(ret) = returning {
        ExecResult {
            columns: returning_column_names(ret, col_names).into(),
            rows: returning_rows,
        }
    } else {
        let cols = INSERTED_COLS.get_or_init(|| Arc::from(vec!["inserted".to_string()]));
        ExecResult {
            columns: Arc::clone(cols),
            rows: vec![vec![Value::Integer(inserted)]],
        }
    }
}

/// What happened to one row in an INSERT statement.
#[derive(Clone, Copy, PartialEq)]
enum InsertOutcome {
    /// A brand-new row was inserted.
    Inserted,
    /// UPSERT DO UPDATE modified an existing row.
    UpdatedExisting,
    /// The row was skipped (OR IGNORE / DO NOTHING).
    Skipped,
}

/// Insert a single row into the table B+tree + maintain indexes.
/// Returns the `InsertOutcome` describing what happened (inserted /
/// upsert-updated / skipped).
///
/// Extracted from exec_insert's loop body so the fast-path (Plan::Values)
/// and slow-path (generic source) can both call it.
///
/// On UPSERT DO UPDATE, `full_row` is REPLACED with the merged (updated)
/// row so the caller can project RETURNING from it.
/// Fast-path single-row INSERT for the statement shape
/// `INSERT INTO t (c1, c2, ...) VALUES (literal, literal, ...)`.
///
/// Called from `Database::execute` when a lightweight scanner (no tokenizer,
/// no AST, no Plan — see `api::try_fast_insert_parse`) recognizes the
/// statement. Semantics are IDENTICAL to the general path: affinity
/// coercion, NOT NULL enforcement, rowid auto-assignment, UNIQUE index
/// maintenance, conflict handling (default ABORT) — because it funnels
/// into the very same `exec_insert_one_row`. The saving is the entire
/// parse -> plan -> cache machinery (~1.3 us/statement), which dominates
/// single-row OLTP inserts.
///
/// `supplied` is a list of (column_index, value) pairs (already
/// affinity-coerced by the caller). Returns the number of affected rows
/// (0 or 1).
/// Fast-path INSERT for one or more rows of LITERAL values (the byte-level
/// scanner path in api.rs). Semantics identical to exec_insert's Values
/// loop — same affinity coercion, same NOT NULL/CHECK enforcement, same
/// exec_insert_one_row (UNIQUE-index maintenance, conflict resolution,
/// rowid semantics) — but:
///   * values are already `Value`s (no evaluate() per literal),
///   * the target column indices, index roots, max-rowid, current root,
///     row buffer, and payload buffer are resolved ONCE for the whole
///     batch instead of per row,
///   * no AST, Plan, or statement cache.
/// `col_indices` empty = all columns in declared order.
pub fn fast_insert_literal_rows(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    col_indices: &[usize],
    rows: Vec<Vec<Value>>,
) -> Result<i64> {
    let mut current_root = ctx.table_root(table);
    let mut max_rowid = ctx.get_or_scan_max_rowid(table)?;
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let mut index_states = make_index_states(ctx, &indexes, table);
    let table_name_lc = table.name.to_ascii_lowercase();
    let n_cols = table.n_columns();
    let mut full_row: Vec<Value> = vec![Value::Null; n_cols];
    let mut payload_buf: Vec<u8> = Vec::with_capacity(n_cols * 8);
    // Column names — only needed for CHECK constraints (NOT NULL is positional).
    let col_names: Vec<String> = if table.check_exprs.is_empty() {
        Vec::new()
    } else {
        table.columns.iter().map(|c| c.name.clone()).collect()
    };
    let mut inserted = 0i64;

    for row in rows {
        // Reset the row buffer without releasing capacity.
        for v in full_row.iter_mut() {
            *v = Value::Null;
        }
        if col_indices.is_empty() {
            // All columns in declared order.
            if row.len() != n_cols {
                return Err(Error::semantic(format!(
                    "table {} has {} columns but {} values were supplied",
                    table.name, n_cols, row.len()
                )));
            }
            for (i, v) in row.into_iter().enumerate() {
                full_row[i] = table.columns[i].affinity.coerce(v);
            }
        } else {
            if row.len() != col_indices.len() {
                return Err(Error::semantic(format!(
                    "{} VALUES for {} columns",
                    row.len(),
                    col_indices.len()
                )));
            }
            for (v, &col_idx) in row.into_iter().zip(col_indices.iter()) {
                full_row[col_idx] = table.columns[col_idx].affinity.coerce(v);
            }
        }

        // Rowid pre-assignment so a NULL rowid-alias doesn't trip NOT NULL
        // (mirrors exec_insert).
        let mut rowid_autogen = false;
        if let Some(idx) = table.rowid_alias {
            if full_row[idx].is_null() {
                max_rowid += 1;
                full_row[idx] = Value::Integer(max_rowid);
                rowid_autogen = true;
            }
        }

        enforce_row_constraints(table, &full_row, &col_names, &ctx.params, &ctx.named_params)?;
        enforce_child_fks(ctx, table, &full_row)?;

        // BEFORE INSERT triggers.
        if crate::executor::triggers::has_triggers_for(ctx, table, &crate::sql::ast::TriggerEvent::Insert) {
            crate::executor::triggers::fire_triggers(
                ctx,
                table,
                &crate::sql::ast::TriggerEvent::Insert,
                crate::sql::ast::TriggerWhen::Before,
                Some(&full_row),
                None,
                &table.col_names,
            )?;
            // Triggers may mutate this table through the generic path —
            // invalidate all append hints (they re-pin on next use).
            for st in index_states.iter_mut() {
                st.hint = None;
            }
            ctx.table_append_hint = None;
        }

        let outcome = exec_insert_one_row(
            ctx,
            table,
            &table_name_lc,
            &mut current_root,
            &mut max_rowid,
            &mut full_row,
            &mut payload_buf,
            &mut index_states,
            crate::sql::ast::ConflictResolution::Abort,
            None,
            rowid_autogen,
        )?;
        let ok = matches!(outcome, InsertOutcome::Inserted | InsertOutcome::UpdatedExisting);
        if ok {
            inserted += 1;
        }
        // AFTER INSERT triggers.
        if ok
            && crate::executor::triggers::has_triggers_for(ctx, table, &crate::sql::ast::TriggerEvent::Insert)
        {
            crate::executor::triggers::fire_triggers(
                ctx,
                table,
                &crate::sql::ast::TriggerEvent::Insert,
                crate::sql::ast::TriggerWhen::After,
                Some(&full_row),
                None,
                &table.col_names,
            )?;
            // Same hint invalidation as BEFORE triggers.
            for st in index_states.iter_mut() {
                st.hint = None;
            }
            ctx.table_append_hint = None;
        }
    }

    // Write back any index-root moves (splits).
    for st in index_states.iter() {
        if ctx.index_root(&st.idx) != st.root {
            ctx.set_index_root(&st.idx.name, st.root);
        }
    }
    Ok(inserted)
}

pub fn fast_insert_single_row(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    supplied: Vec<(usize, Value)>,
) -> Result<i64> {
    // Look up indexes on this table once (same as exec_insert).
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let mut index_states = make_index_states(ctx, &indexes, table);

    let table_name_lc = table.name.to_ascii_lowercase();
    let mut current_root = ctx.table_root(table);
    let mut max_rowid = ctx.get_or_scan_max_rowid(table)?;

    let n_cols = table.n_columns();
    let mut full_row: Vec<Value> = vec![Value::Null; n_cols];
    let mut payload_buf: Vec<u8> = Vec::with_capacity(n_cols * 8);

    for (idx, v) in supplied {
        full_row[idx] = v;
    }

    // Rowid pre-assignment so a NULL rowid-alias doesn't trip NOT NULL
    // (mirrors exec_insert).
    let mut rowid_autogen = false;
    if let Some(idx) = table.rowid_alias {
        if full_row[idx].is_null() {
            max_rowid += 1;
            full_row[idx] = Value::Integer(max_rowid);
            rowid_autogen = true;
        }
    }

    // NOT NULL / CHECK enforcement (positional; CHECKs use col_names).
    enforce_row_constraints(
        table,
        &full_row,
        &table.col_names,
        &ctx.params,
        &ctx.named_params,
    )?;
    enforce_child_fks(ctx, table, &full_row)?;

    let outcome = exec_insert_one_row(
        ctx,
        table,
        &table_name_lc,
        &mut current_root,
        &mut max_rowid,
        &mut full_row,
        &mut payload_buf,
        &mut index_states,
        crate::sql::ast::ConflictResolution::Abort,
        None,
        rowid_autogen,
    )?;
    // Write back index roots that moved (splits).
    for st in index_states.iter() {
        if ctx.index_root(&st.idx) != st.root {
            ctx.set_index_root(&st.idx.name, st.root);
        }
    }
    match outcome {
        InsertOutcome::Inserted | InsertOutcome::UpdatedExisting => Ok(1),
        InsertOutcome::Skipped => Ok(0),
    }
}

fn exec_insert_one_row(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    table_name_lc: &str,
    current_root: &mut u32,
    max_rowid: &mut i64,
    full_row: &mut Vec<Value>,
    payload_buf: &mut Vec<u8>,
    index_states: &mut Vec<IndexMaintState>,
    on_conflict: ConflictResolution,
    upsert: Option<&crate::sql::ast::UpsertClause>,
    rowid_autogen_hint: bool,
) -> Result<InsertOutcome> {
    // Compute rowid. `rowid_was_autogenerated` tracks whether WE assigned
    // the rowid (vs. the user providing it explicitly). This is used below
    // to skip the redundant `lookup_table` check when we KNOW the rowid is
    // new, and to take the BTREE_APPEND fast path (right-most descent +
    // leaf append without a binary search) — the single biggest win for
    // bulk sequential inserts.
    //
    // The rowid may have been pre-assigned by the caller (for constraint
    // enforcement); `rowid_autogen_hint` says whether that happened.
    let rowid_was_autogenerated;
    let rowid = if let Some(idx) = table.rowid_alias {
        match &full_row[idx] {
            Value::Integer(i) => {
                rowid_was_autogenerated = rowid_autogen_hint;
                *i
            }
            Value::Null => {
                *max_rowid += 1;
                full_row[idx] = Value::Integer(*max_rowid);
                rowid_was_autogenerated = true;
                *max_rowid
            }
            _ => return Err(Error::semantic("rowid alias column must be an integer or NULL")),
        }
    } else {
        *max_rowid += 1;
        rowid_was_autogenerated = true;
        *max_rowid
    };

    // Determine which constraint the UPSERT target refers to:
    //  - `Some(i)` — the unique index at position i
    //  - `RowidPk` — the INTEGER PRIMARY KEY (rowid)
    //  - `Any`     — empty target list (any uniqueness constraint)
    enum UpsertTarget {
        Any,
        RowidPk,
        Index(usize),
    }
    let upsert_target = match upsert {
        Some(u) if u.target.is_empty() => UpsertTarget::Any,
        Some(u) => {
            let idx_pos = index_states.iter().position(|st| {
                st.idx.unique
                    && st.idx.columns.len() == u.target.len()
                    && u.target.iter().all(|t| {
                        st.idx.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&t.name))
                    })
            });
            match idx_pos {
                Some(i) => UpsertTarget::Index(i),
                None => {
                    // Single-column target on the rowid alias (INTEGER PK)?
                    if u.target.len() == 1 {
                        if let Some(col_idx) = table.find_column(&u.target[0].name) {
                            if table.rowid_alias == Some(col_idx) {
                                UpsertTarget::RowidPk
                            } else {
                                return Err(Error::semantic(
                                    "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
                                ));
                            }
                        } else {
                            return Err(Error::semantic(
                                "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
                            ));
                        }
                    } else {
                        return Err(Error::semantic(
                            "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
                        ));
                    }
                }
            }
        }
        None => UpsertTarget::Any,
    };

    // UNIQUE-index constraint check: before touching the table btree, look
    // up the new row's key in every UNIQUE index on this table. If any
    // index already contains the key, the row is a duplicate and we apply
    // the configured conflict resolution (IGNORE → skip, REPLACE → delete
    // the conflicting row first, ABORT/FAIL/ROLLBACK → error,
    // UPSERT → DO NOTHING / DO UPDATE).
    let mut conflict_rowid: Option<i64> = None;
    let mut conflict_on_target = false;
    for i in 0..index_states.len() {
        if !index_states[i].idx.unique {
            continue;
        }
        // Any REPLACE/UPSERT path below mutates the index trees, so the
        // append hint may go stale — drop it (it re-pins on the next
        // plain insert).
        index_states[i].hint = None;
        let key_bytes = index_states[i].encode_key(full_row).to_vec();
        let idx_root = index_states[i].root;
        let mut ibt = Btree::new(ctx.pager, idx_root, true);
        let matches = ibt.lookup_index(&key_bytes)?;
        if !matches.is_empty() {
            conflict_rowid = Some(matches[0]);
            conflict_on_target = matches!(
                upsert_target,
                UpsertTarget::Any | UpsertTarget::Index(_)
            ) && (matches!(upsert_target, UpsertTarget::Any) || {
                if let UpsertTarget::Index(t) = &upsert_target { *t == i } else { false }
            });
            break;
        }
    }
    if let Some(existing_rowid) = conflict_rowid {
        // UPSERT path: conflicts on the targeted index take the upsert
        // action; conflicts on OTHER constraints are plain errors.
        if conflict_on_target {
            if let Some(u) = upsert {
                return exec_upsert_row(
                    ctx, table, table_name_lc, current_root, full_row, payload_buf,
                    index_states, existing_rowid, u,
                );
            }
        }
        match on_conflict {
            ConflictResolution::Ignore => return Ok(InsertOutcome::Skipped),
            ConflictResolution::Replace => {
                // Delete the conflicting row from the table and from
                // all indexes (we'll re-insert the new row below).
                let mut bt = Btree::new(ctx.pager, *current_root, false);
                let old_payload_opt = match bt.lookup_table(existing_rowid)? {
                    LookupResult::Found(p) => Some(p),
                    LookupResult::NotFound => None,
                };
                bt.delete_table(existing_rowid)?;
                *current_root = bt.root;
                ctx.set_table_root_lc(table_name_lc, *current_root);
                if let Some(old_payload) = old_payload_opt {
                    if let Ok(old_row) = decode_row(&old_payload, table.n_columns(), existing_rowid, table.rowid_alias) {
                        for st in index_states.iter_mut() {
                            let old_key = encode_index_key(&st.idx, table, &old_row);
                            let mut ibt = Btree::new(ctx.pager, st.root, true);
                            ibt.delete_index(&old_key, existing_rowid)?;
                            st.root = ibt.root;
                        }
                    }
                }
            }
            _ => return Err(Error::semantic(format!(
                "UNIQUE constraint failed: {}",
                table.name
            ))),
        }
    }

    // Reuse the hoisted payload_buf. encode_row_aliased_into clears it first
    // and elides the rowid-alias column to a 1-byte marker (its value lives
    // in the B+tree cell key).
    encode_row_aliased_into(full_row, table.rowid_alias, payload_buf);
    let payload: &[u8] = payload_buf;
    let old_payload_opt;
    {
        let mut bt = Btree::new(ctx.pager, *current_root, false);
        // Optimization: when the rowid was auto-generated (we just
        // assigned max_rowid+1 ourselves), it CANNOT collide with an
        // existing row — the table's rowids are a strict subset of
        // integers up to max_rowid, and max_rowid+1 is by definition not
        // in that set. So we can skip the lookup_table call entirely
        // (saving ~50% of exec_insert's per-row time on multi-row
        // VALUES batches) and go straight to insert_table.
        //
        // We still need the slow path when the user explicitly provided
        // a rowid (it might already exist), or when the conflict
        // resolution is REPLACE (we need to know whether to delete an
        // existing row first).
        if rowid_was_autogenerated && on_conflict != ConflictResolution::Replace {
            old_payload_opt = None;
            // BTREE_APPEND fast path: for sequential auto-rowids,
            // skip the binary search per insert. Falls back to the
            // normal path automatically if the leaf is full or the
            // rowid is not actually an append.
            //
            // The APPEND HINT (statement-scoped) additionally skips the
            // root-to-leaf descent per row: bulk VALUES batches pin the
            // right-most leaf and append straight into it until a split
            // re-pins. Validated on every use.
            let table_key = Arc::as_ptr(table) as usize;
            let hint = ctx
                .table_append_hint
                .take()
                .filter(|(k, _)| *k == table_key)
                .map(|(_, leaf)| leaf);
            let new_hint = bt.insert_table_append_hinted(rowid, payload, hint)?;
            if let Some(leaf) = new_hint {
                ctx.table_append_hint = Some((table_key, leaf));
            } else {
                ctx.table_append_hint = None;
            }
        } else {
            // Single lookup_table call (was 2 before — redundant when
            // the rowid existed and we needed the old payload for the
            // REPLACE index cleanup path).
            let lookup = bt.lookup_table(rowid)?;
            match lookup {
                LookupResult::Found(existing_payload) => {
                    // Rowid conflict. UPSERT applies when the target is
                    // empty (any constraint) or the rowid PK itself.
                    if let Some(u) = upsert {
                        if matches!(upsert_target, UpsertTarget::Any | UpsertTarget::RowidPk) {
                            drop_payload(existing_payload);
                            return exec_upsert_row(
                                ctx, table, table_name_lc, current_root, full_row, payload_buf,
                                index_states, rowid, u,
                            );
                        }
                    }
                    match on_conflict {
                        ConflictResolution::Replace => {
                            old_payload_opt = Some(existing_payload);
                            bt.delete_table(rowid)?;
                            bt.insert_table(rowid, payload)?;
                        }
                        ConflictResolution::Ignore => return Ok(InsertOutcome::Skipped),
                        _ => return Err(Error::semantic(format!("UNIQUE constraint failed: rowid={}", rowid))),
                    }
                }
                LookupResult::NotFound => {
                    old_payload_opt = None;
                    bt.insert_table(rowid, payload)?;
                }
            }
        }
        // Track the (possibly new) root page.
        *current_root = bt.root;
        ctx.set_table_root_lc(table_name_lc, *current_root);
    }
    // On conflict: delete the old row's index entries first.
    if let Some(old_payload) = old_payload_opt {
        if !index_states.is_empty() {
            if let Ok(old_row) = decode_row(&old_payload, table.n_columns(), rowid, table.rowid_alias) {
                for st in index_states.iter_mut() {
                    let old_key = encode_index_key(&st.idx, table, &old_row);
                    let mut ibt = Btree::new(ctx.pager, st.root, true);
                    ibt.delete_index(&old_key, rowid)?;
                    st.root = ibt.root;
                }
            }
        }
    }
    // Maintain indexes: insert an entry for each index on this table.
    // The per-index APPEND HINT skips the root-to-leaf descent + binary
    // search on ascending bulk loads (validated per use; falls back to
    // the full insert path automatically).
    for st in index_states.iter_mut() {
        // Copy the root/hint out first: `encode_key` holds a mutable
        // borrow of `st` for as long as `key_bytes` is live.
        let root = st.root;
        let hint = st.hint.take();
        let key_bytes = st.encode_key(full_row);
        let mut ibt = Btree::new(ctx.pager, root, true);
        let new_hint = ibt.insert_index_append_hinted(key_bytes, rowid, hint)?;
        st.hint = new_hint;
        st.root = ibt.root;
    }
    ctx.last_insert_rowid = rowid;
    ctx.changes += 1;
    ctx.set_max_rowid_lc(table_name_lc, rowid);
    Ok(InsertOutcome::Inserted)
}

/// Consume an unused payload (helper to make the control flow above clear).
#[inline]
fn drop_payload(_p: Vec<u8>) {}

/// Execute the UPSERT action (`ON CONFLICT ... DO NOTHING / DO UPDATE SET`).
///
/// Reads the existing row, evaluates the SET assignments with unqualified
/// column refs bound to the EXISTING row and `excluded.<col>` refs bound to
/// the proposed (new) row, applies the optional WHERE guard, rewrites the
/// row in the B+tree, and maintains indexes.
///
/// On success, `full_row` is replaced with the merged row (for RETURNING).
fn exec_upsert_row(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    table_name_lc: &str,
    current_root: &mut u32,
    full_row: &mut Vec<Value>,
    payload_buf: &mut Vec<u8>,
    index_states: &mut Vec<IndexMaintState>,
    existing_rowid: i64,
    upsert: &crate::sql::ast::UpsertClause,
) -> Result<InsertOutcome> {
    match &upsert.action {
        crate::sql::ast::UpsertAction::DoNothing => {
            return Ok(InsertOutcome::Skipped);
        }
        crate::sql::ast::UpsertAction::DoUpdate { set, where_clause } => {
            // Read the existing row.
            let mut bt = Btree::new(ctx.pager, *current_root, false);
            let old_payload = match bt.lookup_table(existing_rowid)? {
                LookupResult::Found(p) => p,
                LookupResult::NotFound => {
                    // Shouldn't happen (index said it exists) — treat as insert.
                    return Ok(InsertOutcome::Skipped);
                }
            };
            let n_cols = table.n_columns();
            let old_row = match decode_row(&old_payload, n_cols, existing_rowid, table.rowid_alias) {
                Ok(r) => r,
                Err(_) => return Ok(InsertOutcome::Skipped),
            };

            // Build a combined evaluation context: unqualified refs → the
            // existing row; `excluded.<col>` → the proposed row.
            let mut comb_names: Vec<String> = Vec::with_capacity(n_cols * 2);
            let mut comb_row: Vec<Value> = Vec::with_capacity(n_cols * 2);
            for (i, c) in table.columns.iter().enumerate() {
                comb_names.push(c.name.clone());
                comb_row.push(old_row.get(i).cloned().unwrap_or(Value::Null));
            }
            for (i, c) in table.columns.iter().enumerate() {
                comb_names.push(format!("excluded.{}", c.name));
                comb_row.push(full_row.get(i).cloned().unwrap_or(Value::Null));
            }

            // Apply SET assignments onto the old row.
            let mut new_row = old_row.clone();
            for (col_name, expr) in set {
                let col_idx = table
                    .find_column(col_name)
                    .ok_or_else(|| Error::semantic(format!("no such column: {}", col_name)))?;
                let v = eval_row(expr, &comb_row, &comb_names, &ctx.params, &ctx.named_params)?;
                let aff = table.columns[col_idx].affinity;
                new_row[col_idx] = aff.coerce(v);
            }

            // WHERE guard: if false, the conflict is left in place (no-op).
            // Column refs bind to the PRE-update row (SQLite semantics).
            if let Some(w) = where_clause {
                let v = eval_row(w, &comb_row, &comb_names, &ctx.params, &ctx.named_params)?;
                if !v.is_truthy() {
                    return Ok(InsertOutcome::Skipped);
                }
            }

            // Enforce NOT NULL + CHECK on the merged row.
            let plain_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
            enforce_row_constraints(table, &new_row, &plain_names, &ctx.params, &ctx.named_params)?;

            // Rowid alias must stay consistent (it's the B+tree key).
            if let Some(idx) = table.rowid_alias {
                new_row[idx] = Value::Integer(existing_rowid);
            }

            // Rewrite the row: in-place when the payload size is unchanged,
            // otherwise delete + insert.
            payload_buf.clear();
            encode_row_aliased_into(&new_row, table.rowid_alias, payload_buf);
            {
                let mut bt = Btree::new(ctx.pager, *current_root, false);
                let did_in_place = bt.update_table(existing_rowid, payload_buf).unwrap_or(false);
                if !did_in_place {
                    bt.delete_table(existing_rowid)?;
                    bt.insert_table(existing_rowid, payload_buf)?;
                }
                *current_root = bt.root;
                ctx.set_table_root_lc(table_name_lc, *current_root);
            }

            // Index maintenance: update entries whose key changed.
            for st in index_states.iter_mut() {
                let old_key = encode_index_key(&st.idx, table, &old_row);
                let new_key = encode_index_key(&st.idx, table, &new_row);
                if old_key == new_key {
                    continue;
                }
                // The tree shape changes here — drop any append hint.
                st.hint = None;
                let mut ibt = Btree::new(ctx.pager, st.root, true);
                ibt.delete_index(&old_key, existing_rowid)?;
                st.root = ibt.root;
                let mut ibt = Btree::new(ctx.pager, st.root, true);
                ibt.insert_index(&new_key, existing_rowid)?;
                st.root = ibt.root;
            }

            // Replace full_row with the merged row for RETURNING.
            *full_row = new_row;
            ctx.changes += 1;
            ctx.last_insert_rowid = existing_rowid;
            Ok(InsertOutcome::UpdatedExisting)
        }
    }
}

/// Encode the index key for a row, given the table's column layout.
/// Uses the ORDER-PRESERVING key encoding (see `Value::encode_order_key_into`)
/// so that the index B+tree's byte order matches SQL value ordering —
/// required for range scans and binary-search equality lookups.
pub(crate) fn encode_index_key(index: &crate::schema::Index, table: &Table, row: &[Value]) -> Vec<u8> {
    let mut key_bytes = Vec::new();
    for col in &index.columns {
        if let Some(pos) = table.find_column(&col.name) {
            if let Some(v) = row.get(pos) {
                v.encode_order_key_into(&mut key_bytes);
            }
        }
    }
    key_bytes
}

/// Insert an entry into an index for a given row.
fn insert_index_entry(ctx: &mut ExecContext<'_>, index: &crate::schema::Index, table: &Table, row: &[Value], rowid: i64) -> Result<()> {
    let key_bytes = encode_index_key(index, table, row);
    // Override-aware root: an earlier split may have moved this index's
    // root since the catalog snapshot was taken.
    let root = ctx.index_root(index);
    let mut bt = Btree::new(ctx.pager, root, true);
    bt.insert_index(&key_bytes, rowid)?;
    if bt.root != root {
        ctx.set_index_root(&index.name, bt.root);
    }
    Ok(())
}

/// Delete an entry from an index for a given row.
fn delete_index_entry(ctx: &mut ExecContext<'_>, index: &crate::schema::Index, table: &Table, row: &[Value], rowid: i64) -> Result<()> {
    let key_bytes = encode_index_key(index, table, row);
    let root = ctx.index_root(index);
    let mut bt = Btree::new(ctx.pager, root, true);
    bt.delete_index(&key_bytes, rowid)?;
    if bt.root != root {
        ctx.set_index_root(&index.name, bt.root);
    }
    Ok(())
}

// Note: these functions are kept for reference but the INSERT executor now
// tracks root pages inline. UPDATE and DELETE still use these, which is safe
// because they use the catalog's root_page (which may be stale after splits
// within the same statement — a known limitation to fix later).

fn find_max_rowid(pager: &Pager, root: u32) -> Result<i64> {
    let mut bt = Btree::new(pager, root, false);
    let mut max = 0i64;
    bt.scan_table(|rowid, _| {
        if rowid > max {
            max = rowid;
        }
        true
    })?;
    Ok(max)
}

// ============================================================================
// UPDATE
// ============================================================================

fn exec_update(ctx: &mut ExecContext<'_>, table: Arc<Table>, source: &Plan, assignments: &[(usize, Expr)], returning: Option<&[crate::sql::ast::ResultColumn]>) -> Result<ExecResult> {
    // Streaming UPDATE fast path: when the source is a bare `Scan`, a
    // `Filter { Scan, predicate }`, or a `RowidRange`, iterate the B+tree
    // directly with a reusable row buffer. This avoids the materialize-all
    // cost (`Vec<Vec<Value>>` of N rows × N columns of allocations) for
    // the common `UPDATE t SET col = ... WHERE pred` case.
    //
    // The non-streaming path materializes all matching rows into a Vec<Row>
    // and then iterates them. For a 10k-row UPDATE, that's 10k Vec<Value>
    // allocations + 10k+ Value allocations (one per Text/Blob column per
    // row). The streaming path does ONE Vec<Value> allocation (reused
    // across rows) and ONE Vec<u8> allocation for the encoded payload
    // (also reused). Cuts ~80% of the allocations on a 10k-row UPDATE,
    // closing the 23× UPDATE-range gap vs SQLite.
    if let Some(result) = try_streaming_update(ctx, &table, source, assignments, returning)? {
        // Autocommit flush (transactional execution defers this to COMMIT;
        // deferred_flush leaves it to the threshold/requery logic). The
        // flush used to live at the end of the general path — the fast
        // path's early return skipped it, losing autocommit UPDATEs on
        // disk (in-memory reads stayed correct until eviction/reopen).
        if !ctx.in_transaction && !ctx.deferred_flush {
            ctx.pager.flush()?;
        }
        return Ok(result);
    }

    let source_res = execute(source, ctx)?;
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let mut updated = 0i64;

    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();

    for row in &source_res.rows {
        let rowid = if let Some(idx) = table.rowid_alias {
            row[idx].as_integer()
        } else {
            return Err(Error::Unsupported("UPDATE on a table without INTEGER PRIMARY KEY"));
        };

        let mut new_row = row.clone();
        for (col_idx, expr) in assignments {
            new_row[*col_idx] = eval_row(expr, row, &col_names, &ctx.params, &ctx.named_params)?;
            let aff = table.columns[*col_idx].affinity;
            new_row[*col_idx] = aff.coerce(new_row[*col_idx].clone());
        }
        // NOT NULL + CHECK constraints on the updated row.
        enforce_row_constraints(&table, &new_row, &col_names, &ctx.params, &ctx.named_params)?;
        let payload = encode_row_aliased(&new_row, table.rowid_alias);
        let root = ctx.table_root(&table);
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            // Fast path: if the new payload is the same length as the
            // existing cell payload (the common case for UPDATEs that don't
            // change column types — e.g. `score = score + 1.0` on a REAL
            // column), overwrite the payload bytes in place. This avoids
            // the delete+insert dance (two B+tree traversals + two leaf
            // modifications per row) and is the single biggest UPDATE-perf
            // win.
            //
            // When the payload size changes (e.g. TEXT column gets longer),
            // we fall back to delete + insert.
            let did_in_place = bt.update_table(rowid, &payload).unwrap_or(false);
            if !did_in_place {
                bt.delete_table(rowid)?;
                bt.insert_table(rowid, &payload)?;
            }
            new_root = bt.root;
        }
        ctx.set_table_root(&table.name, new_root);
        // Maintain indexes: for each index on this table, compare the old
        // and new indexed column values. If they're unchanged, skip the
        // delete+insert on that index entirely (the index entry is still
        // valid). This is the common case for `UPDATE t SET score = ...`
        // when the index is on a different column (e.g. `idx_val` on `val`,
        // and the UPDATE doesn't touch `val`).
        for idx in &indexes {
            // Compute the old and new index keys.
            let old_key = encode_index_key(idx, &table, row);
            let new_key = encode_index_key(idx, &table, &new_row);
            if old_key == new_key {
                // No change to this index's key — skip maintenance.
                continue;
            }
            let _ = delete_index_entry(ctx, idx, &table, row, rowid);
            let _ = insert_index_entry(ctx, idx, &table, &new_row, rowid);
        }
        ctx.changes += 1;
        updated += 1;
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(ret, &new_row, &col_names, &ctx.params, &ctx.named_params)?);
        }
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        });
    }
    Ok(ExecResult {
        columns: Arc::from(vec!["updated".to_string()]),
        rows: vec![vec![Value::Integer(updated)]],
    })
}

/// Streaming UPDATE fast path: scan the B+tree directly and update each
/// matching row in place, avoiding the `Vec<Vec<Value>>` materialization
/// cost of the non-streaming path.
///
/// Recognized source shapes (mirrors the streaming-aggregate fast path):
///  - `Plan::Scan`                              — `UPDATE t SET ...`
///  - `Plan::Filter { Scan, predicate }`       — `UPDATE t SET ... WHERE pred`
///  - `Plan::RowidRange`                        — `UPDATE t SET ... WHERE id BETWEEN ? AND ?`
///
/// For each leaf cell visited:
///  1. Decode the row into a reusable `Vec<Value>` buffer (no per-row alloc).
///  2. Apply the filter predicate (if any). Skip if false.
///  3. Apply the SET assignments to produce a `new_row`.
///  4. Encode the new row into a reusable `Vec<u8>` buffer.
///  5. Update in place via `Btree::update_table(rowid, &payload)` if the
///     payload size is unchanged; fall back to delete+insert if it changed.
///  6. Maintain indexes: compare old vs new index keys; only update the
///     index if the key changed.
///
/// Returns `Ok(Some(result))` if the streaming path handled the UPDATE;
/// `Ok(None)` if the source shape wasn't recognized and the caller should
/// fall back to the non-streaming path.
fn try_streaming_update(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    source: &Plan,
    assignments: &[(usize, Expr)],
    returning: Option<&[crate::sql::ast::ResultColumn]>,
) -> Result<Option<ExecResult>> {
    // Detect the source shape and extract (table, filter_predicate, range, rowid).
    enum StreamingSource<'a> {
        Scan { table: &'a Arc<Table>, filter: Option<&'a Expr> },
        RowidRange { table: &'a Arc<Table>, start: Option<&'a Expr>, end: Option<&'a Expr>, residual: Option<&'a Expr> },
        RowidLookup { table: &'a Arc<Table>, rowid: &'a Expr },
        IndexRange {
            table: &'a Arc<Table>,
            index: &'a Arc<crate::schema::Index>,
            start: Option<&'a (Expr, bool)>,
            end: Option<&'a (Expr, bool)>,
            residual: Option<&'a Expr>,
        },
    }
    let src = match source {
        Plan::Scan { table: t, .. } => StreamingSource::Scan { table: t, filter: None },
        Plan::Filter { input, predicate } => {
            if let Plan::Scan { table: t, .. } = input.as_ref() {
                StreamingSource::Scan { table: t, filter: Some(predicate) }
            } else {
                return Ok(None);
            }
        }
        Plan::RowidRange { table: t, start, end, residual, .. } => {
            StreamingSource::RowidRange { table: t, start: start.as_ref(), end: end.as_ref(), residual: residual.as_ref() }
        }
        Plan::RowidLookup { table: t, rowid, .. } => {
            StreamingSource::RowidLookup { table: t, rowid }
        }
        Plan::IndexRange { table: t, index, start, end, residual, .. } => {
            StreamingSource::IndexRange { table: t, index, start: start.as_ref(), end: end.as_ref(), residual: residual.as_ref() }
        }
        _ => return Ok(None),
    };

    // The source table must match the UPDATE's target table (otherwise
    // we'd be updating rows from a different table, which isn't what
    // this fast path is for).
    // `*t` copies the `&Arc<Table>` field out of the match-ergonomics
    // double reference (`.clone()` on a `&&Arc` only clones the reference).
    let src_table: &Arc<Table> = match &src {
        StreamingSource::Scan { table: t, .. } => *t,
        StreamingSource::RowidRange { table: t, .. } => *t,
        StreamingSource::RowidLookup { table: t, .. } => *t,
        StreamingSource::IndexRange { table: t, .. } => *t,
    };
    if !src_table.name.eq_ignore_ascii_case(&table.name) {
        return Ok(None);
    }

    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();
    // Reuse the cached Arc<[String]> column names — rebuilding a Vec of
    // cloned Strings was one alloc + N String clones per UPDATE statement.
    let col_names: std::sync::Arc<[String]> = table.col_names.clone();
    let col_names: &[String] = &col_names;
    let n_cols = table.n_columns();
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let root = ctx.table_root(table);

    // Evaluate rowid-range bounds (if RowidRange source).
    let (range_start, range_end) = match &src {
        StreamingSource::RowidRange { start, end, .. } => {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
            let s = match start { Some(e) => evaluate(e, &eval_ctx)?.as_integer(), None => i64::MIN };
            let e = match end { Some(e) => evaluate(e, &eval_ctx)?.as_integer(), None => i64::MAX };
            (s, e)
        }
        _ => (i64::MIN, i64::MAX),
    };
    let residual_pred = match &src {
        StreamingSource::Scan { filter, .. } => *filter,
        StreamingSource::RowidRange { residual, .. } => *residual,
        StreamingSource::RowidLookup { .. } => None,
        StreamingSource::IndexRange { residual, .. } => *residual,
    };
    // Compile the residual predicate ONCE per statement (mirrors
    // exec_filter / the aggregate streaming scan): positional evaluation
    // against the full table-order row is ~5-15 ns/row vs the ~60-120
    // ns/row AST walk + name-resolution of eval_row. `UPDATE t SET ...
    // WHERE val > ?` over 10k rows saves ~0.5-1 ms per statement.
    let compiled_residual: Option<crate::executor::predicate::CompiledPredicate> =
        residual_pred.and_then(|p| {
            crate::executor::predicate::compile_predicate(p, table, &table.name)
        });

    // For RowidLookup, evaluate the rowid expression now.
    let lookup_rowid: Option<i64> = match &src {
        StreamingSource::RowidLookup { rowid, .. } => {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
            Some(evaluate(rowid, &eval_ctx)?.as_integer())
        }
        _ => None,
    };

    // Reusable buffers — allocated once, reused per row.
    let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
    let mut new_row: Vec<Value> = Vec::with_capacity(n_cols);
    let mut payload_buf: Vec<u8> = Vec::with_capacity(256);

    // Compile SET expressions positionally ONCE per statement: `score =
    // score + 1.0` costs an AST walk + name resolution per row on the
    // general path (~80-120 ns); compiled evaluation is ~5-15 ns.
    let compiled_assignments: Vec<Option<crate::executor::predicate::CompiledExpr>> =
        assignments
            .iter()
            .map(|(_, e)| {
                crate::executor::predicate::compile_expr(e, col_names, params.len())
            })
            .collect();
    let compiled_ref: Option<&[Option<crate::executor::predicate::CompiledExpr>]> =
        if compiled_assignments.iter().any(|c| c.is_some()) {
            Some(&compiled_assignments)
        } else {
            None
        };

    // Pre-compute which indexes might be touched by the SET assignments
    // BEFORE the scan — when any index column is assigned, the OLD payload
    // must be stashed during the scan phase so phase 2 can compute the old
    // index keys WITHOUT re-fetching the row (one full B+tree descent +
    // decode per row). The lookup-based sources (RowidLookup / IndexRange)
    // already hold the payload as an owned Vec — stashing it is free.
    let touched_indexes: Vec<&Arc<crate::schema::Index>> = indexes
        .iter()
        .filter(|idx| {
            idx.columns.iter().any(|c| {
                table
                    .find_column(&c.name)
                    .map(|col_idx| {
                        assignments.iter().any(|(a_idx, _)| *a_idx == col_idx)
                    })
                    .unwrap_or(false)
            })
        })
        .collect();
    // AFTER UPDATE triggers need the OLD row too — stash old payloads
    // whenever triggers exist, not just for index maintenance.
    let has_update_triggers = crate::executor::triggers::has_triggers_for(
        ctx,
        table,
        &crate::sql::ast::TriggerEvent::Update(vec![]),
    );
    let needs_old_payload = !touched_indexes.is_empty() || has_update_triggers;

    // Collect (rowid, new_payload_bytes, old_payload_bytes) tuples for the
    // update phase. We can't update inside the scan callback because the
    // scan holds the pager via the Btree, so we collect first, then update
    // after the scan completes.
    // (rowid, payload range in the arena, old-payload stash). The arena
    // replaces one Vec<u8> allocation PER ROW — a 5000-row UPDATE paid
    // 5000 alloc+copy pairs just to hand payloads to phase 2.
    let mut updates: Vec<(i64, std::ops::Range<usize>, Option<Vec<u8>>)> = Vec::new();
    let mut update_arena: Vec<u8> = Vec::new();
    // RETURNING: stash decoded new rows too (only when needed).
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    // First constraint error encountered during the scan (if any).
    let mut first_error: Option<crate::error::Error> = None;

    let mut bt = Btree::new(ctx.pager, root, false);
    if let Some(rowid) = lookup_rowid {
        // ---- SINGLE-ROW FAST PATH --------------------------------------
        // `UPDATE t SET ... WHERE id = ?` — the OLTP workhorse. When no
        // indexed column is assigned and no AFTER UPDATE triggers exist,
        // process and apply the row in ONE pass: fetch (leaf-hinted) →
        // decode → SET → constraints → encode → in-place patch. Skips the
        // `updates` / `order` / `sorted_updates` / `deferred` / `done_mask`
        // Vec machinery and its per-statement allocations entirely.
        if touched_indexes.is_empty()
            && !has_update_triggers
            && residual_pred.is_none()
        {
            let mut applied = false;
            let fetch = bt.lookup_table_with(rowid, |payload| {
                // Decode + SET + constraints + encode, all before the page
                // lock is released (payload borrowed, no copy).
                row_buf.clear();
                if decode_row_into(payload, n_cols, rowid, table.rowid_alias, &mut row_buf).is_err() {
                    return Ok(());
                }
                new_row.clear();
                new_row.extend_from_slice(&row_buf);
                for (col_idx, expr) in assignments {
                    let v = eval_row(expr, &row_buf, col_names, &params, &named_params)?;
                    let aff = table.columns[*col_idx].affinity;
                    new_row[*col_idx] = aff.coerce(v);
                }
                enforce_row_constraints(table, &new_row, col_names, &params, &named_params)?;
                enforce_child_fks(ctx, table, &new_row)?;
                payload_buf.clear();
                encode_row_aliased_into(&new_row, table.rowid_alias, &mut payload_buf);
                if let Some(ret) = returning {
                    returning_rows.push(project_returning_row(ret, &new_row, col_names, &params, &named_params)?);
                }
                applied = true;
                Ok(())
            })?;
            if fetch.is_some() && applied {
                // Patch in place (leaf-hinted); size changes fall back to
                // delete + insert.
                let did = bt.update_table(rowid, &payload_buf)?;
                if !did {
                    bt.delete_table(rowid)?;
                    bt.insert_table(rowid, &payload_buf)?;
                }
                if bt.root != root {
                    ctx.set_table_root(&table.name, bt.root);
                }
                return Ok(Some(ExecResult {
                    columns: returning_column_names(returning.unwrap_or(&[]), &table.col_names).into(),
                    rows: returning_rows,
                }));
            }
            if fetch.is_some() {
                // Row exists but was skipped (decode failure) — fall through
                // to the general path for identical semantics.
            } else {
                // Rowid absent: zero rows updated.
                return Ok(Some(ExecResult {
                    columns: returning_column_names(returning.unwrap_or(&[]), &table.col_names).into(),
                    rows: returning_rows,
                }));
            }
        }
        // RowidLookup source — fetch exactly one row by rowid.
        match bt.lookup_table(rowid)? {
            LookupResult::Found(payload) => {
                // `payload` is an owned Vec — hand it over for phase 2's
                // index maintenance (free stash, saves a re-fetch descent).
                let old_owned = if needs_old_payload { Some(payload.clone()) } else { None };
                if let Err(e) = process_update_row(
                    ctx, &payload, n_cols, rowid, &mut row_buf, &mut new_row, &mut payload_buf,
                    assignments, &col_names, &params, &named_params, table,
                    residual_pred, &mut updates, &mut update_arena, &mut returning_rows, returning, old_owned,
                    compiled_ref,
                    compiled_residual.as_ref(),
                ) {
                    first_error = Some(e);
                }
            }
            LookupResult::NotFound => {}
        }
        Ok::<(), crate::error::Error>(())
    } else if matches!(src, StreamingSource::RowidRange { .. }) {
        bt.scan_table_range_borrowed(range_start, range_end, |rowid, payload| {
            let old_owned = if needs_old_payload { Some(payload.to_vec()) } else { None };
            if let Err(e) = process_update_row(
                ctx, payload, n_cols, rowid, &mut row_buf, &mut new_row, &mut payload_buf,
                assignments, &col_names, &params, &named_params, table,
                residual_pred, &mut updates, &mut update_arena, &mut returning_rows, returning, old_owned,
                compiled_ref,
                compiled_residual.as_ref(),
            ) {
                first_error = Some(e);
                return false; // stop the scan
            }
            true
        })
    } else if let StreamingSource::IndexRange { index, start, end, .. } = &src {
        // IndexRange source: `UPDATE ... WHERE indexed_col > ?` (and
        // BETWEEN / < / >= variants). Phase 1a scans the index between the
        // encoded bounds collecting rowids; phase 1b fetches each row from
        // the table B+tree and processes it through the same streaming
        // path. Previously this shape fell through to the generic
        // exec_update, which materialized every matching row (Vec<Value>
        // per row) before updating.
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
        let start_key: Option<(Vec<u8>, bool)> = match start {
            Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
            None => None,
        };
        let end_key: Option<(Vec<u8>, bool)> = match end {
            Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
            None => None,
        };
        let scan_start: Vec<u8> = start_key.as_ref().map(|(k, _)| k.clone()).unwrap_or_default();
        let mut rowids: Vec<i64> = Vec::new();
        {
            let index_root = ctx.index_root(index);
            let mut index_bt = Btree::new(ctx.pager, index_root, true);
            index_bt.scan_index_from(&scan_start, |rowid, cell_key| {
                // Exclusive lower bound: skip entries matching the bound key.
                if let Some((k, false)) = &start_key {
                    if cell_key.starts_with(k) {
                        return true;
                    }
                }
                // Upper bound: stop past it.
                if let Some((k, inc)) = &end_key {
                    match index_key_prefix_cmp(cell_key, k) {
                        std::cmp::Ordering::Less => {}
                        std::cmp::Ordering::Equal if *inc => {}
                        _ => return false,
                    }
                }
                rowids.push(rowid);
                true
            })?;
        }
        // Phase 1b: fetch rows by rowid and process. All reads — the tree
        // isn't mutated until phase 2, so interleaving table lookups here
        // is safe.
        //
        // MERGE SCAN: when the selection is a large fraction of the table,
        // sort the rowids and walk the table B+tree ONCE (sequential leaf
        // reads, ~60-80 ns per visited row) instead of one random descent
        // (~300 ns) per rowid. Selecting 5000 of 10000 rows: ~1.5 ms of
        // descents becomes ~0.7 ms of scan. Payloads are borrowed from the
        // page during the scan, so the old-payload stash clones when needed.
        let max_rowid_hint = ctx.get_or_scan_max_rowid(table).unwrap_or(0);
        let use_merge = max_rowid_hint > 0 && (rowids.len() as i64) * 4 > max_rowid_hint;
        if use_merge {
            rowids.sort_unstable();
            rowids.dedup();
            let mut ri = 0usize;
            let mut err: Option<crate::error::Error> = None;
            bt.scan_table_borrowed(|rowid, payload| {
                while ri < rowids.len() && rowids[ri] < rowid {
                    ri += 1;
                }
                if ri >= rowids.len() {
                    return false; // all matches processed
                }
                if rowids[ri] != rowid {
                    return true;
                }
                ri += 1;
                let old_owned = if needs_old_payload { Some(payload.to_vec()) } else { None };
                if let Err(e) = process_update_row(
                    ctx, payload, n_cols, rowid, &mut row_buf, &mut new_row, &mut payload_buf,
                    assignments, &col_names, &params, &named_params, table,
                    residual_pred, &mut updates, &mut update_arena, &mut returning_rows, returning, old_owned,
                    compiled_ref,
                    compiled_residual.as_ref(),
                ) {
                    err = Some(e);
                    return false;
                }
                true
            })?;
            if let Some(e) = err {
                first_error = Some(e);
            }
        } else {
        for rowid in rowids {
            match bt.lookup_table(rowid)? {
                LookupResult::Found(payload) => {
                    // Owned payload — stash for phase 2's index maintenance
                    // (saves a re-fetch descent per row when the SET clause
                    // touches an indexed column).
                    let old_owned = if needs_old_payload { Some(payload.clone()) } else { None };
                    if let Err(e) = process_update_row(
                        ctx, &payload, n_cols, rowid, &mut row_buf, &mut new_row, &mut payload_buf,
                        assignments, &col_names, &params, &named_params, table,
                        residual_pred, &mut updates, &mut update_arena, &mut returning_rows, returning, old_owned,
                        compiled_ref,
                        compiled_residual.as_ref(),
                    ) {
                        first_error = Some(e);
                        break;
                    }
                }
                LookupResult::NotFound => {}
            }
        }
        }
        Ok::<(), crate::error::Error>(())
    } else {
        bt.scan_table_borrowed(|rowid, payload| {
            let old_owned = if needs_old_payload { Some(payload.to_vec()) } else { None };
            if let Err(e) = process_update_row(
                ctx, payload, n_cols, rowid, &mut row_buf, &mut new_row, &mut payload_buf,
                assignments, &col_names, &params, &named_params, table,
                residual_pred, &mut updates, &mut update_arena, &mut returning_rows, returning, old_owned,
                compiled_ref,
                compiled_residual.as_ref(),
            ) {
                first_error = Some(e);
                return false; // stop the scan
            }
            true
        })
    }?;
    let new_root = bt.root;
    ctx.set_table_root(&table.name, new_root);

    // Surface any constraint error BEFORE applying updates (statement
    // aborts atomically — the pager snapshot/rollback handles the rest).
    if let Some(e) = first_error {
        return Err(e);
    }

    // Phase 2: apply the updates. For each (rowid, new_payload, old_payload):
    //   1. The old payload was pre-stashed during the scan when an indexed
    //      column is being SET — no per-row re-fetch descent needed.
    //   2. If new payload size matches the existing size, overwrite in
    //      place — BULK when updates are rowid-sorted: one tree traversal
    //      patches every same-size payload with no per-row descent.
    //      Size-changed / missing rows fall back to per-row delete+insert.
    //   3. Maintain indexes: only update the index if the key changed.
    let mut updated = 0i64;
    let mut old_row_buf: Vec<Value> = Vec::with_capacity(n_cols);
    // One persistent Btree per touched index, reused across all rows.
    let mut index_bts: Vec<Btree<'_>> = Vec::with_capacity(touched_indexes.len());
    let mut index_roots: Vec<u32> = Vec::with_capacity(touched_indexes.len());
    for idx in &touched_indexes {
        let r = ctx.index_root(idx);
        index_roots.push(r);
        index_bts.push(Btree::new(ctx.pager, r, true));
    }
    // Sort by rowid (merge-scan sources already are; IndexRange sources
    // are in index order). Phase 2's per-row work is order-independent.
    let mut order: Vec<usize> = (0..updates.len()).collect();
    order.sort_unstable_by_key(|&i| updates[i].0);
    let mut deferred: Vec<usize> = Vec::new();
    let mut root = root;
    // Triggers force the per-row path: bulk-applied rows skip AFTER UPDATE
    // trigger firing.
    // Bulk in-place pass ONLY when no indexed column is being SET —
    // bulk-applied rows skip the per-row index maintenance, so any
    // touched index forces the per-row path (defer everything).
    if touched_indexes.is_empty() && !has_update_triggers {
        let sorted_updates: Vec<(i64, &[u8])> = order
            .iter()
            .map(|&i| {
                let r = updates[i].1.clone();
                (updates[i].0, &update_arena[r])
            })
            .collect();
        let mut bt = Btree::new(ctx.pager, root, false);
        bt.update_table_bulk(&sorted_updates, &mut deferred)?;
        if bt.root != root {
            root = bt.root;
            ctx.set_table_root(&table.name, root);
        }
    } else {
        deferred = (0..updates.len()).collect();
    }
    // Everything the bulk pass did NOT defer is already applied.
    let mut done_mask: Vec<bool> = vec![true; updates.len()];
    for &di in &deferred {
        done_mask[di] = false; // not applied yet
    }
    for &i in order.iter() {
        if done_mask[i] {
            continue;
        }
        let (rowid, new_payload_range, old_payload_stash) = &updates[i];
        let new_payload: &[u8] = &update_arena[new_payload_range.clone()];
        let old_payload_opt: Option<&[u8]> = old_payload_stash.as_deref();
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            let did_in_place = bt.update_table(*rowid, new_payload).unwrap_or(false);
            if !did_in_place {
                bt.delete_table(*rowid)?;
                bt.insert_table(*rowid, new_payload)?;
            }
            new_root = bt.root;
        }
        ctx.set_table_root(&table.name, new_root);
        // AFTER UPDATE triggers: NEW = the post-change row, OLD = the
        // pre-change row (decoded from the stash when present; otherwise
        // we can't reconstruct it — triggers on indexed-SET updates always
        // have the stash since needs_old_payload is true for them).
        if has_update_triggers {
            let old_row_v: Option<Vec<Value>> = old_payload_opt.and_then(|op| {
                decode_row(op, n_cols, *rowid, table.rowid_alias).ok()
            });
            let new_row_v = decode_row(new_payload, n_cols, *rowid, table.rowid_alias).ok();
            if let (Some(old_r), Some(new_r)) = (old_row_v, new_row_v) {
                let changed_cols: Vec<String> = assignments
                    .iter()
                    .map(|(idx, _)| table.columns[*idx].name.clone())
                    .collect();
                crate::executor::triggers::fire_triggers(
                    ctx,
                    &table,
                    &crate::sql::ast::TriggerEvent::Update(changed_cols),
                    crate::sql::ast::TriggerWhen::After,
                    Some(&new_r),
                    Some(&old_r),
                    &table.col_names,
                )?;
            }
        }
        // Index maintenance — only on indexes whose key actually changed.
        // The old/new keys are computed HERE and handed straight to the
        // B+tree (the previous code called delete_index_entry /
        // insert_index_entry wrappers, which re-encoded the keys from the
        // decoded rows a second time and created a fresh Btree per call).
        // Index B+trees are reused across the loop: their roots track
        // splits, and the pager keeps pages hot.
        if needs_old_payload {
            if let Some(old_payload) = old_payload_opt {
                old_row_buf.clear();
                if decode_row_into(old_payload, n_cols, *rowid, table.rowid_alias, &mut old_row_buf).is_err() {
                    continue;
                }
                new_row.clear();
                if decode_row_into(new_payload, n_cols, *rowid, table.rowid_alias, &mut new_row).is_err() {
                    continue;
                }
                for (ti, idx) in touched_indexes.iter().enumerate() {
                    let old_key = encode_index_key(idx, table, &old_row_buf);
                    let new_key = encode_index_key(idx, table, &new_row);
                    if old_key == new_key {
                        continue;
                    }
                    let ibt = &mut index_bts[ti];
                    if ibt.delete_index(&old_key, *rowid).is_ok() {
                        if ibt.insert_index(&new_key, *rowid).is_err() {
                            // Insert failed after a successful delete — the
                            // entry is gone; propagate a hard error.
                            return Err(Error::corruption(format!(
                                "index maintenance failed for {} (rowid {})",
                                idx.name, rowid
                            )));
                        }
                    }
                    if ibt.root != index_roots[ti] {
                        index_roots[ti] = ibt.root;
                        ctx.set_index_root(&idx.name, ibt.root);
                    }
                }
            }
        }
        ctx.changes += 1;
        updated += 1;
    }
    // Count the bulk-applied rows (they skipped the per-row loop).
    {
        let bulk_applied = updates.len().saturating_sub(deferred.len());
        updated += bulk_applied as i64;
        ctx.changes += bulk_applied as i64;
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(Some(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        }));
    }
    Ok(Some(ExecResult {
        columns: Arc::from(vec!["updated".to_string()]),
        rows: vec![vec![Value::Integer(updated)]],
    }))
}

/// Per-row processing for the streaming UPDATE: decode the payload into
/// `row_buf`, apply the filter predicate, build the new row in `new_row`,
/// enforce constraints, encode it into `payload_buf`, and push the
/// (rowid, payload) tuple into `updates`. Buffers are reused across rows.
#[allow(clippy::too_many_arguments)]
fn process_update_row(
    fk_ctx: &ExecContext<'_>,
    payload: &[u8],
    n_cols: usize,
    rowid: i64,
    row_buf: &mut Vec<Value>,
    new_row: &mut Vec<Value>,
    payload_buf: &mut Vec<u8>,
    assignments: &[(usize, Expr)],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
    table: &Arc<Table>,
    residual_pred: Option<&Expr>,
    updates: &mut Vec<(i64, std::ops::Range<usize>, Option<Vec<u8>>)>,
    update_arena: &mut Vec<u8>,
    returning_rows: &mut Vec<Vec<Value>>,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
    // Old row payload, owned (pre-stashed by lookup-based sources) or
    // cloned (scan-based sources). Only stashed when an indexed column is
    // being SET — phase 2 needs it for index maintenance without a
    // per-row re-fetch descent.
    old_payload_stash: Option<Vec<u8>>,
    // Pre-compiled positional SET expressions (None per assignment when
    // the shape isn't compilable — the general AST walk is used then).
    compiled: Option<&[Option<crate::executor::predicate::CompiledExpr>]>,
    // Pre-compiled positional residual predicate (None when the residual
    // doesn't compile — the general AST walk is used then). Identity
    // positions: row_buf holds the full table-order row.
    compiled_pred: Option<&crate::executor::predicate::CompiledPredicate>,
) -> Result<()> {
    row_buf.clear();
    if decode_row_into(payload, n_cols, rowid, table.rowid_alias, row_buf).is_err() {
        return Ok(());
    }
    // Rowid comes from the B+tree cell key (passed in by the caller).
    // Apply filter predicate (if any).
    if let Some(pred) = residual_pred {
        let keep = if let Some(cp) = compiled_pred {
            // Compiled positional evaluation: no AST walk, no name
            // lookup — the same ~5-15 ns/row cost the SELECT Filter path
            // enjoys (eval_row costs ~60-120 ns/row).
            let positions: &[usize] = IDENTITY_POSITIONS;
            cp.eval(row_buf, positions, params)
        } else {
            match eval_row(pred, row_buf, col_names, params, named_params) {
                Ok(v) => v.is_truthy(),
                Err(e) => return Err(e),
            }
        };
        if !keep {
            return Ok(());
        }
    }
    // Build the new row: copy old values, apply SET assignments.
    new_row.clear();
    new_row.extend_from_slice(row_buf);
    for (i, (col_idx, expr)) in assignments.iter().enumerate() {
        let v = if let Some(Some(cexpr)) = compiled.and_then(|c| c.get(i)) {
            // Compiled positional evaluation: no AST walk, no name lookup.
            cexpr.eval(row_buf, params)
        } else {
            eval_row(expr, row_buf, col_names, params, named_params)?
        };
        let aff = table.columns[*col_idx].affinity;
        new_row[*col_idx] = aff.coerce(v);
    }
    // NOT NULL + CHECK constraints on the updated row.
    enforce_row_constraints(table, new_row, col_names, params, named_params)?;
    // FOREIGN KEY (child side): the updated row's FK values must reference
    // an existing parent row (only when the pragma is on and the table has
    // FKs — two cheap checks otherwise).
    enforce_child_fks(fk_ctx, table, new_row)?;
    // Encode the new row.
    payload_buf.clear();
    encode_row_aliased_into(new_row, table.rowid_alias, payload_buf);
    // RETURNING: project now (the row is final).
    if let Some(ret) = returning {
        returning_rows.push(project_returning_row(ret, new_row, col_names, params, named_params)?);
    }
    // Stash the (rowid, new payload, old payload) for phase 2. The new
    // payload goes into the shared arena — no per-row allocation.
    let start = update_arena.len();
    update_arena.extend_from_slice(payload_buf);
    updates.push((rowid, start..update_arena.len(), old_payload_stash));
    Ok(())
}

// ============================================================================
// DELETE
// ============================================================================

/// Streaming DELETE: walk the source B+tree directly (or the index for
/// IndexRange sources), evaluate the residual predicate against a reused
/// decode buffer, and collect just the rowids to remove. Phase 2 deletes
/// each rowid via `delete_table_get_payload` (one descent, payload handed
/// back for index maintenance / RETURNING / triggers).
///
/// Besides the allocation savings (the generic path materializes every
/// matched row — one Vec<Value> + one String per TEXT column), this is
/// what makes `DELETE FROM t WHERE ...` work on tables **without** an
/// INTEGER PRIMARY KEY: the rowid comes from the B+tree cell key, not
/// from a row column (SQLite semantics — every table has a rowid).
///
/// Returns Ok(None) when the source shape isn't handled here (the caller
/// falls back to the generic materialize-all path).
fn try_streaming_delete(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    source: &Plan,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
) -> Result<Option<ExecResult>> {
    use crate::planner::plan::Plan;

    // The source table must be the DELETE target itself.
    let (src_table, residual_pred, range, index_range) = match source {
        Plan::Scan { table: t, predicate, .. } => (t, predicate.as_ref(), None, None),
        Plan::Filter { input, predicate } => {
            match input.as_ref() {
                Plan::Scan { table: t, predicate: None, .. } => (t, Some(predicate), None, None),
                _ => return Ok(None),
            }
        }
        Plan::RowidRange { table: t, start, end, residual, .. } => {
            (t, residual.as_ref(), Some((start.as_ref(), end.as_ref())), None)
        }
        Plan::IndexRange { table: t, index, start, end, residual, .. } => {
            (t, residual.as_ref(), None, Some((index, start.as_ref(), end.as_ref())))
        }
        _ => return Ok(None),
    };
    if !src_table.name.eq_ignore_ascii_case(&table.name) {
        return Ok(None);
    }

    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let n_cols = table.n_columns();
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let has_delete_triggers = crate::executor::triggers::has_triggers_for(
        ctx,
        table,
        &crate::sql::ast::TriggerEvent::Delete,
    );
    let need_row = !indexes.is_empty() || returning.is_some() || has_delete_triggers;
    let root = ctx.table_root(table);

    // ---- Phase 1: collect rowids to delete ----
    let mut rowids: Vec<i64> = Vec::new();
    let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
    let mut first_error: Option<crate::error::Error> = None;

    let mut bt = Btree::new(ctx.pager, root, false);
    if let Some((index, start, end)) = index_range {
        // IndexRange source: scan the index between the encoded bounds,
        // collecting rowids (bounds logic mirrors try_streaming_update).
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
        let start_key: Option<(Vec<u8>, bool)> = match start {
            Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
            None => None,
        };
        let end_key: Option<(Vec<u8>, bool)> = match end {
            Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
            None => None,
        };
        let scan_start: Vec<u8> = start_key.as_ref().map(|(k, _)| k.clone()).unwrap_or_default();
        let index_root = ctx.index_root(index);
        {
            let mut index_bt = Btree::new(ctx.pager, index_root, true);
            index_bt.scan_index_from(&scan_start, |rowid, cell_key| {
                if let Some((k, false)) = &start_key {
                    if cell_key.starts_with(k) {
                        return true; // exclusive lower bound
                    }
                }
                if let Some((k, inc)) = &end_key {
                    match index_key_prefix_cmp(cell_key, k) {
                        std::cmp::Ordering::Less => {}
                        std::cmp::Ordering::Equal if *inc => {}
                        _ => return false,
                    }
                }
                rowids.push(rowid);
                true
            })?;
        }
        // Fetch each candidate row to evaluate the residual predicate
        // (index bounds alone may over-select; the predicate decides).
        if residual_pred.is_some() || need_row {
            let mut matched: Vec<i64> = Vec::with_capacity(rowids.len());
            for rid in rowids.drain(..) {
                match bt.lookup_table(rid)? {
                    LookupResult::Found(payload) => {
                        row_buf.clear();
                        if decode_row_into(&payload, n_cols, rid, table.rowid_alias, &mut row_buf).is_err() {
                            continue;
                        }
                        if let Some(pred) = residual_pred {
                            match eval_row(pred, &row_buf, &col_names, &params, &named_params) {
                                Ok(v) if v.is_truthy() => {}
                                Ok(_) => continue,
                                Err(e) => {
                                    first_error = Some(e);
                                    break;
                                }
                            }
                        }
                        matched.push(rid);
                    }
                    LookupResult::NotFound => {} // row deleted concurrently — skip
                }
            }
            rowids = matched;
        }
    } else if let Some((start_expr, end_expr)) = range {
        // RowidRange source: walk the range, decode each payload for the
        // residual predicate (ranges are inclusive on both ends).
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
        let lo = match start_expr {
            Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
            None => i64::MIN,
        };
        let hi = match end_expr {
            Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
            None => i64::MAX,
        };
        bt.scan_table_range_borrowed(lo, hi, |rowid, payload| {
            if let Some(pred) = residual_pred {
                row_buf.clear();
                if decode_row_into(payload, n_cols, rowid, table.rowid_alias, &mut row_buf).is_err() {
                    return true; // skip undecodable rows, keep walking
                }
                match eval_row(pred, &row_buf, &col_names, &params, &named_params) {
                    Ok(v) if v.is_truthy() => {}
                    Ok(_) => return true,
                    Err(e) => {
                        first_error = Some(e);
                        return false;
                    }
                }
            }
            rowids.push(rowid);
            true
        })?;
    } else {
        // Full scan (optionally filtered): walk every cell.
        bt.scan_table_borrowed(|rowid, payload| {
            if let Some(pred) = residual_pred {
                row_buf.clear();
                if decode_row_into(payload, n_cols, rowid, table.rowid_alias, &mut row_buf).is_err() {
                    return true;
                }
                match eval_row(pred, &row_buf, &col_names, &params, &named_params) {
                    Ok(v) if v.is_truthy() => {}
                    Ok(_) => return true,
                    Err(e) => {
                        first_error = Some(e);
                        return false;
                    }
                }
            }
            rowids.push(rowid);
            true
        })?;
    }
    if let Some(e) = first_error {
        return Err(e);
    }
    // Keep max-rowid bookkeeping consistent (mirrors the generic path):
    // deleting the max rowid invalidates the cached next-rowid hint.
    let table_name_lc = table.name.to_ascii_lowercase();
    if let Some(&max_del) = rowids.iter().max() {
        ctx.invalidate_max_rowid_if_deleted(&table_name_lc, max_del);
    }

    // ---- Phase 2: delete + index maintenance + triggers + RETURNING ----
    let mut deleted: i64 = 0;
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    let mut new_root = root;
    for rid in rowids {
        // FOREIGN KEY (parent side): reject / cascade / set-null BEFORE the
        // row goes away. The old row is fetched first (needed for the key
        // comparison and for CASCADE/SET NULL rewrites of child rows).
        if ctx.pager.foreign_keys_enabled() {
            let mut bt = Btree::new(ctx.pager, new_root, false);
            if let LookupResult::Found(payload) = bt.lookup_table(rid)? {
                if let Ok(old_row) = decode_row(&payload, n_cols, rid, table.rowid_alias) {
                    enforce_parent_delete_fks(ctx, table, &old_row, rid, 0)?;
                }
            }
        }
        // One descent: find + delete, payload returned for maintenance.
        let payload = {
            let mut bt = Btree::new(ctx.pager, new_root, false);
            let p = bt.delete_table_get_payload(rid)?;
            new_root = bt.root;
            p
        };
        ctx.set_table_root_lc(&table_name_lc, new_root);
        let Some(payload) = payload else { continue };
        deleted += 1;
        ctx.changes += 1;
        if need_row {
            let row = decode_row(&payload, n_cols, rid, table.rowid_alias)?;
            if let Some(ret) = returning {
                returning_rows.push(project_returning_row(ret, &row, &col_names, &params, &named_params)?);
            }
            for idx in &indexes {
                delete_index_entry(ctx, idx, table, &row, rid)?;
            }
            if has_delete_triggers {
                crate::executor::triggers::fire_triggers(
                    ctx,
                    table,
                    &crate::sql::ast::TriggerEvent::Delete,
                    crate::sql::ast::TriggerWhen::After,
                    None,
                    Some(&row),
                    &table.col_names,
                )?;
            }
        }
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if returning.is_some() {
        return Ok(Some(ExecResult {
            columns: returning_column_names(returning.unwrap(), &col_names).into(),
            rows: returning_rows,
        }));
    }
    Ok(Some(ExecResult {
        columns: Arc::from(vec!["deleted".to_string()]),
        rows: vec![vec![Value::Integer(deleted)]],
    }))
}

fn exec_delete(ctx: &mut ExecContext<'_>, table: Arc<Table>, source: &Plan, returning: Option<&[crate::sql::ast::ResultColumn]>) -> Result<ExecResult> {
    // Streaming path first: handles Scan/Filter, RowidRange and IndexRange
    // sources without materializing matched rows, and — critically —
    // supports DELETE on tables without an INTEGER PRIMARY KEY (rowid
    // comes from the B+tree cell key). RowidLookup sources fall through
    // to the dedicated point-delete fast path below.
    if let Some(result) = try_streaming_delete(ctx, &table, source, returning)? {
        return Ok(result);
    }

    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let table_name_lc = table.name.to_ascii_lowercase();

    // ---- Fast path: DELETE ... WHERE id = ? (RowidLookup source) ----
    //
    // The generic path executes the source plan (which decodes every
    // matched row into a Vec<Value> — including a String allocation per
    // TEXT column) and then re-walks the B+tree to delete. When the source
    // is a RowidLookup on the same table, evaluate the rowid expression
    // directly and use `delete_table_get_payload`, which finds + deletes
    // in ONE descent and returns the old payload bytes. The row is decoded
    // ONLY when actually needed: RETURNING projections or index
    // maintenance.
    let col_names_fast: Option<Vec<String>> = if returning.is_some() {
        Some(table.columns.iter().map(|c| c.name.clone()).collect())
    } else {
        None
    };
    if let Plan::RowidLookup { table: src_table, rowid, .. } = source {
        if Arc::ptr_eq(src_table, &table) || src_table.name.eq_ignore_ascii_case(&table.name) {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            let rowid_val = evaluate(rowid, &eval_ctx)?.as_integer();
            let root = ctx.table_root(&table);
            // FOREIGN KEY (parent side): check BEFORE deleting — the row's
            // key values are needed for the child-reference scan and for
            // CASCADE / SET NULL rewrites.
            if ctx.pager.foreign_keys_enabled() {
                let mut bt = Btree::new(ctx.pager, root, false);
                if let LookupResult::Found(payload) = bt.lookup_table(rowid_val)? {
                    if let Ok(old_row) = decode_row(&payload, table.n_columns(), rowid_val, table.rowid_alias) {
                        enforce_parent_delete_fks(ctx, &table, &old_row, rowid_val, 0)?;
                    }
                }
            }
            let (new_root, old_payload) = {
                let mut bt = Btree::new(ctx.pager, root, false);
                let payload = bt.delete_table_get_payload(rowid_val)?;
                (bt.root, payload)
            };
            ctx.set_table_root_lc(&table_name_lc, new_root);
            let mut deleted = 0i64;
            let mut returning_rows: Vec<Vec<Value>> = Vec::new();
            if let Some(payload) = old_payload {
                deleted = 1;
                ctx.changes += 1;
                let need_row = !indexes.is_empty()
                    || returning.is_some()
                    || crate::executor::triggers::has_triggers_for(
                        ctx,
                        &table,
                        &crate::sql::ast::TriggerEvent::Delete,
                    );
                if need_row {
                    // Decode the old row only for index keys / RETURNING /
                    // triggers.
                    let row = decode_row(&payload, table.n_columns(), rowid_val, table.rowid_alias)?;
                    if let (Some(ret), Some(names)) = (returning, col_names_fast.as_deref()) {
                        returning_rows.push(project_returning_row(ret, &row, names, &ctx.params, &ctx.named_params)?);
                    }
                    for idx in &indexes {
                        delete_index_entry(ctx, idx, &table, &row, rowid_val)?;
                    }
                    // AFTER DELETE triggers.
                    if crate::executor::triggers::has_triggers_for(
                        ctx,
                        &table,
                        &crate::sql::ast::TriggerEvent::Delete,
                    ) {
                        crate::executor::triggers::fire_triggers(
                            ctx,
                            &table,
                            &crate::sql::ast::TriggerEvent::Delete,
                            crate::sql::ast::TriggerWhen::After,
                            None,
                            Some(&row),
                            &table.col_names,
                        )?;
                    }
                }
                // Keep the cached max-rowid consistent (see below).
                ctx.invalidate_max_rowid_if_deleted(&table_name_lc, rowid_val);
            }
            if !ctx.in_transaction && !ctx.deferred_flush {
                ctx.pager.flush()?;
            }
            if let Some(ret) = returning {
                let names = col_names_fast.as_deref().unwrap_or(&[]);
                return Ok(ExecResult {
                    columns: returning_column_names(ret, names).into(),
                    rows: returning_rows,
                });
            }
            return Ok(ExecResult {
                columns: Arc::from(vec!["deleted".to_string()]),
                rows: vec![vec![Value::Integer(deleted)]],
            });
        }
    }

    let source_res = execute(source, ctx)?;
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let mut deleted = 0i64;
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    let mut max_deleted: i64 = i64::MIN;
    for row in &source_res.rows {
        let rowid = if let Some(idx) = table.rowid_alias {
            row[idx].as_integer()
        } else {
            return Err(Error::Unsupported("DELETE on a table without INTEGER PRIMARY KEY"));
        };
        if rowid > max_deleted {
            max_deleted = rowid;
        }
        // RETURNING: project the pre-delete row.
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(ret, row, &col_names, &ctx.params, &ctx.named_params)?);
        }
        // FOREIGN KEY (parent side): reject / cascade / set-null before the
        // row goes away (only when the pragma is on and some table
        // references this one).
        enforce_parent_delete_fks(ctx, &table, row, rowid, 0)?;
        let root = ctx.table_root(&table);
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            bt.delete_table(rowid)?;
            new_root = bt.root;
        }
        ctx.set_table_root_lc(&table_name_lc, new_root);
        // Maintain indexes: delete the entry for this row.
        for idx in &indexes {
            delete_index_entry(ctx, idx, &table, row, rowid)?;
        }
        ctx.changes += 1;
        deleted += 1;
        // AFTER DELETE triggers.
        if crate::executor::triggers::has_triggers_for(
            ctx,
            &table,
            &crate::sql::ast::TriggerEvent::Delete,
        ) {
            crate::executor::triggers::fire_triggers(
                ctx,
                &table,
                &crate::sql::ast::TriggerEvent::Delete,
                crate::sql::ast::TriggerWhen::After,
                None,
                Some(row),
                &table.col_names,
            )?;
        }
    }
    // Keep the cached max-rowid consistent: if we deleted the current max
    // rowid, the cached value is stale. Invalidate it — the next INSERT
    // rescans once and picks up the true max (matching SQLite's
    // `next rowid = max(existing) + 1`, which REUSES rowids after the max
    // row is deleted). Without this, a DELETE-all followed by inserts
    // continued from the old max instead of restarting at 1.
    if max_deleted != i64::MIN {
        ctx.invalidate_max_rowid_if_deleted(&table_name_lc, max_deleted);
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        });
    }
    Ok(ExecResult {
        columns: Arc::from(vec!["deleted".to_string()]),
        rows: vec![vec![Value::Integer(deleted)]],
    })
}
