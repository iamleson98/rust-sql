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
pub(crate) mod explain;
pub mod expr;
pub mod json;
pub(crate) mod predicate;
pub(crate) mod triggers;
pub(crate) mod vtab_exec;

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
        static CORR: RefCell<CorrState> = const { RefCell::new(CorrState {
            ctx: std::ptr::null_mut(),
            depth: 0,
            outer: Vec::new(),
        }) };
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
            // Lifetime erasure for the thread-local: the pointee type is
            // identical, only the lifetime parameter widens to 'static
            // (the guard keeps the real borrow alive on the caller's
            // stack). `.cast()` is the method form of the same pointer
            // conversion.
            let erased = ctx.cast::<ExecContext<'static>>();
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
                let qual_lower =
                    format!("{}.{}", t.to_ascii_lowercase(), name.to_ascii_lowercase());
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
    unsafe fn exec_select(
        sel: &SelectStatement,
        ctx: *mut ExecContext<'static>,
    ) -> Result<ExecResult> {
        let ctx = unsafe { &mut *ctx };
        super::exec_select_statement(sel, ctx)
    }
}

pub(crate) use corr::Guard as CorrGuard;

/// Evaluator-facing wrapper: execute a correlated scalar subquery with the
/// given EvalContext as the outer scope.
pub(crate) fn corr_exec_scalar(sel: &SelectStatement, eval_ctx: &EvalContext<'_>) -> Result<Value> {
    corr::exec_scalar(
        sel,
        eval_ctx.row as *const [Value],
        eval_ctx.column_names as *const [String],
    )
}

/// Evaluator-facing wrapper: correlated EXISTS.
pub(crate) fn corr_exec_exists(sel: &SelectStatement, eval_ctx: &EvalContext<'_>) -> Result<Value> {
    corr::exec_exists(
        sel,
        eval_ctx.row as *const [Value],
        eval_ctx.column_names as *const [String],
    )
}

/// Evaluator-facing wrapper: correlated IN-subquery list.
pub(crate) fn corr_exec_in_list(
    sel: &SelectStatement,
    eval_ctx: &EvalContext<'_>,
) -> Result<Vec<Value>> {
    corr::exec_in_list(
        sel,
        eval_ctx.row as *const [Value],
        eval_ctx.column_names as *const [String],
    )
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
use crate::storage::row_codec::{
    decode_row, decode_row_into, decode_row_selective, decode_row_selective_wide,
    encode_row_aliased, encode_row_aliased_into,
};
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
    pub ctes: Option<HashMap<String, crate::types::CteMaterialization>>,
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
        self.shared
            .roots
            .get(&lc)
            .copied()
            .unwrap_or(table.root_page)
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
        self.shared
            .index_roots
            .get(&lc)
            .copied()
            .unwrap_or(index.root_page)
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
        // Monotonic: the max-rowid cache may only ever be RAISED. It feeds
        // `next_auto_rowid` (max + 1), which is only collision-free when it
        // is an upper bound of the rowids actually in the table. Lowering
        // it (e.g. after `INSERT INTO t (rowid, ...) VALUES (5, ...)` into
        // a table whose max is 100) would make the next auto-allocated
        // rowid collide with an existing row — SQLite keeps the true max
        // and so must we. DELETE of the max rowid invalidates the entry
        // entirely (see `invalidate_max_rowid_if_deleted`), which is the
        // only legitimate way the cached max shrinks.
        if let Some(v) = self.max_rowids.get_mut(table_name_lc) {
            if rowid > *v {
                *v = rowid;
                self.max_rowids_changed = true;
            }
            return;
        }
        if let Some(&shared_max) = self.shared.max_rowids.get(table_name_lc) {
            if rowid > shared_max {
                self.max_rowids.insert(table_name_lc.to_string(), rowid);
                self.max_rowids_changed = true;
            }
            // rowid <= shared_max: the shared snapshot is already an upper
            // bound — recording a lower local value would poison allocation.
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
        Self {
            columns: Arc::from(Vec::new()),
            rows: Vec::new(),
        }
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
            if let Plan::Join {
                left,
                right,
                join_type,
                condition,
                algorithm,
            } = &**input
            {
                if *algorithm == crate::planner::plan::JoinAlgorithm::Hash {
                    return exec_hash_join(ctx, left, right, *join_type, condition, Some(columns));
                }
            }
            // FUSED PATH: Project over a RowidRange / RowidLookup with
            // bare-column projections — decode ONLY the projected columns
            // per row (skipping e.g. the rowid marker and un-referenced
            // wide text columns), with no second cloning pass.
            if let Plan::RowidRange {
                table,
                alias: _,
                start,
                end,
                residual: None,
            } = &**input
            {
                if let Some((project, out_cols)) = bare_column_projection(columns, table) {
                    return exec_rowid_range_projected(
                        ctx,
                        table.clone(),
                        start.as_ref(),
                        end.as_ref(),
                        project.as_deref(),
                        out_cols,
                    );
                }
            }
            if let Plan::RowidLookup { table, rowid, .. } = &**input {
                if let Some((project, out_cols)) = bare_column_projection(columns, table) {
                    return exec_rowid_lookup_projected(
                        ctx,
                        table.clone(),
                        rowid,
                        project.as_deref(),
                        out_cols,
                    );
                }
            }
            // FUSED PATH: Project over an IndexLookup (WHERE indexed_col = ?)
            // — decode only the projected columns, reuse one B+tree handle
            // across the rowid batch (pinned root survives). Non-bare
            // projections fall through to the generic Project.
            if let Plan::IndexLookup {
                table,
                alias: _,
                index,
                key_exprs,
            } = &**input
            {
                if bare_column_projection(columns, table).is_some() {
                    return exec_index_lookup_projected(
                        ctx,
                        table.clone(),
                        index.clone(),
                        key_exprs,
                        Some(columns),
                    );
                }
            }
            // FUSED PATH: Project over an Index Nested-Loop Join — the join
            // emits only the projected columns (no full-width combined row,
            // no second cloning pass). Mirrors the Hash Join fusion.
            if let Plan::IndexNestedLoopJoin {
                outer,
                inner_table,
                inner_alias,
                inner_index,
                outer_key_col,
            } = &**input
            {
                return exec_index_nested_loop_join(
                    ctx,
                    outer,
                    inner_table.clone(),
                    inner_alias.clone(),
                    inner_index.clone(),
                    *outer_key_col,
                    Some(columns),
                );
            }
            exec_project(ctx, input, columns)
        }
        Plan::Sort { input, terms } => exec_sort(ctx, input, terms),
        Plan::Limit {
            input,
            count,
            offset,
        } => exec_limit(ctx, input, count, offset),
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => exec_aggregate(ctx, input, group_by, aggregates),
        Plan::Window { input, windows } => exec_window(ctx, input, windows),
        Plan::Join {
            left,
            right,
            join_type,
            condition,
            algorithm,
        } => {
            if std::env::var_os("RSQL_DBG_FUSED").is_some() {
                eprintln!(
                    "[dbg] execute: Plan::Join algo={:?} join={:?}",
                    algorithm, join_type
                );
            }
            if *algorithm == crate::planner::plan::JoinAlgorithm::Hash {
                exec_hash_join(ctx, left, right, *join_type, condition, None)
            } else {
                exec_join(ctx, left, right, *join_type, condition)
            }
        }
        Plan::IndexNestedLoopJoin {
            outer,
            inner_table,
            inner_alias,
            inner_index,
            outer_key_col,
        } => exec_index_nested_loop_join(
            ctx,
            outer,
            inner_table.clone(),
            inner_alias.clone(),
            inner_index.clone(),
            *outer_key_col,
            None,
        ),
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
        Plan::RowidIn {
            table,
            alias: _,
            values,
            residual,
        } => exec_rowid_in(ctx, table.clone(), values, residual.as_ref()),
        Plan::IndexIn {
            table,
            alias: _,
            index,
            key_exprs,
            residual,
        } => exec_index_in(
            ctx,
            table.clone(),
            index.clone(),
            key_exprs,
            residual.as_ref(),
        ),
        Plan::RowidRange {
            table,
            alias: _,
            start,
            end,
            residual,
        } => exec_rowid_range(
            ctx,
            table.clone(),
            start.as_ref(),
            end.as_ref(),
            residual.as_ref(),
        ),
        Plan::IndexLookup {
            table,
            alias: _,
            index,
            key_exprs,
        } => exec_index_lookup(ctx, table.clone(), index.clone(), key_exprs),
        Plan::IndexRange {
            table,
            alias,
            index,
            start,
            end,
            residual,
        } => exec_index_range(
            ctx,
            table.clone(),
            alias.clone(),
            index.clone(),
            start.as_ref(),
            end.as_ref(),
            residual.as_ref(),
        ),
        Plan::Insert {
            table,
            source,
            columns,
            on_conflict,
            upsert,
            returning,
        } => exec_insert(
            ctx,
            table.clone(),
            source,
            columns.clone(),
            *on_conflict,
            upsert.as_ref(),
            returning.as_deref(),
        ),
        Plan::Update {
            table,
            source,
            assignments,
            returning,
            or_conflict,
            from,
        } => exec_update(
            ctx,
            table.clone(),
            source,
            assignments,
            returning.as_deref(),
            *or_conflict,
            from.as_deref(),
        ),
        Plan::Delete {
            table,
            source,
            returning,
        } => exec_delete(ctx, table.clone(), source, returning.as_deref()),
    }
}

// Helper: evaluate an expression against a single row.
pub(crate) fn eval_row(
    expr: &Expr,
    row: &[Value],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<Value> {
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
            return Err(Error::constraint(format!(
                "NOT NULL constraint failed: {}.{}",
                table.name, col.name
            )));
        }
    }
    for expr in &table.check_exprs {
        let v = eval_row(expr, row, col_names, params, named_params)?;
        if !v.is_truthy() {
            return Err(Error::constraint(format!(
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
        let key_values: Vec<Value> = fk
            .columns
            .iter()
            .map(|&i| row.get(i).cloned().unwrap_or(Value::Null))
            .collect();
        // MATCH SIMPLE: any NULL in the key → constraint satisfied.
        if key_values.iter().any(|v| v.is_null()) {
            continue;
        }
        if !fk_parent_exists(ctx, fk, &key_values)? {
            // SQLite's runtime FK message is exactly this — no table or
            // column detail (see sqlite3.c fkMismatch / sqlite3FkCheck).
            return Err(Error::constraint("FOREIGN KEY constraint failed"));
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
            t.foreign_keys
                .iter()
                .any(|fk| fk.ref_table.eq_ignore_ascii_case(&parent.name))
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
            let children =
                fk_find_child_rows(ctx, &child, fk, &parent_cols, old_row, parent_rowid)?;
            if children.is_empty() {
                continue;
            }
            match fk.on_delete {
                ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                    // SQLite-exact: no row-count detail.
                    return Err(Error::constraint("FOREIGN KEY constraint failed"));
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

/// Extract the WHERE predicate from an UPDATE/DELETE source plan: the
/// `Filter { predicate }` wrapper, the pushed-down `Scan { predicate }`, or
/// None. Used by the virtual-table DML paths.
fn extract_source_predicate(source: &Plan) -> Option<Expr> {
    match source {
        Plan::Filter { predicate, .. } => Some(predicate.clone()),
        Plan::Scan {
            predicate: Some(p), ..
        } => Some(p.clone()),
        _ => None,
    }
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
        Expr::In {
            source: InSource::Subquery(_),
            ..
        } => true,
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
        Expr::Column { table, name } => Expr::Column {
            table: table.clone(),
            name: name.clone(),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(r!(left)),
            right: Box::new(r!(right)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(r!(expr)),
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(r!(expr)),
            low: Box::new(r!(low)),
            high: Box::new(r!(high)),
            negated: *negated,
        },
        Expr::In {
            expr,
            source,
            negated,
        } => {
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
            Expr::In {
                expr: Box::new(r!(expr)),
                source: new_source,
                negated: *negated,
            }
        }
        Expr::Like {
            op,
            expr,
            pattern,
            escape,
            negated,
        } => {
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
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(r!(expr)),
            negated: *negated,
        },
        Expr::Is {
            left,
            right,
            negated,
        } => Expr::Is {
            left: Box::new(r!(left)),
            right: Box::new(r!(right)),
            negated: *negated,
        },
        Expr::Function {
            name,
            distinct,
            args,
            filter,
            over,
        } => {
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
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
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
            Expr::Case {
                operand: new_operand,
                whens: new_whens,
                else_: new_else,
            }
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
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(r!(expr)),
            type_name: type_name.clone(),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(r!(expr)),
            collation: collation.clone(),
        },
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
            Plan::RowidIn {
                values, residual, ..
            } => {
                for v in values.iter() {
                    out.push(v);
                }
                if let Some(r) = residual.as_ref() {
                    out.push(r);
                }
            }
            Plan::IndexIn {
                key_exprs,
                residual,
                ..
            } => {
                for k in key_exprs.iter() {
                    out.push(k);
                }
                if let Some(r) = residual.as_ref() {
                    out.push(r);
                }
            }
            Plan::RowidRange {
                start,
                end,
                residual,
                ..
            } => {
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
            Plan::IndexRange {
                start,
                end,
                residual,
                ..
            } => {
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
            Plan::Limit {
                input,
                count,
                offset,
            } => {
                out.push(count);
                out.push(offset);
                exprs_in_plan(input, out);
            }
            Plan::Aggregate {
                input,
                group_by,
                aggregates,
            } => {
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
            Plan::Join {
                left,
                right,
                condition,
                ..
            } => {
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
            Plan::Update {
                source,
                assignments,
                ..
            } => {
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
        Plan::RowidIn {
            values, residual, ..
        } => {
            for v in values.iter_mut() {
                rewrite_expr_in_place(v, ctx)?;
            }
            if let Some(r) = residual.as_mut() {
                rewrite_expr_in_place(r, ctx)?;
            }
        }
        Plan::IndexIn {
            key_exprs,
            residual,
            ..
        } => {
            for k in key_exprs.iter_mut() {
                rewrite_expr_in_place(k, ctx)?;
            }
            if let Some(r) = residual.as_mut() {
                rewrite_expr_in_place(r, ctx)?;
            }
        }
        Plan::RowidRange {
            start,
            end,
            residual,
            ..
        } => {
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
        Plan::IndexRange {
            start,
            end,
            residual,
            ..
        } => {
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
        Plan::Limit {
            input,
            count,
            offset,
        } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            rewrite_expr_in_place(count, ctx)?;
            rewrite_expr_in_place(offset, ctx)?;
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
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
        Plan::Join {
            left,
            right,
            condition,
            ..
        } => {
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
        Plan::Update {
            source,
            assignments,
            ..
        } => {
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
                Ok(Expr::Literal(Value::Integer(if res.rows.is_empty() {
                    0
                } else {
                    1
                })))
            }
        }
        Expr::In {
            expr,
            source,
            negated,
        } => {
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
            Ok(Expr::In {
                expr,
                source: inner,
                negated,
            })
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

fn rewrite_table_expression_subqueries(
    te: &mut TableExpression,
    ctx: &mut ExecContext<'_>,
) -> Result<()> {
    match te {
        TableExpression::Table { .. } => {}
        TableExpression::Subquery { select, .. } => {
            rewrite_select_subqueries(select, ctx)?;
        }
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
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
) -> (
    std::collections::HashSet<String>,
    HashMap<String, std::sync::Arc<[String]>>,
) {
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
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
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
        Expr::Between {
            expr, low, high, ..
        } => {
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
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
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
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
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
        Expr::Raise {
            message: Some(m), ..
        } => collect_expr_refs(m, out),
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
        Expr::In {
            source: InSource::Subquery(sel),
            ..
        } => {
            out.push(sel);
            collect_nested_selects(sel, out);
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
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
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

fn exec_scan(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
) -> Result<ExecResult> {
    // Virtual table: drive the module's cursor protocol instead of a
    // B+tree scan. The pushed-down predicate (if any) is offered to the
    // module through best_index.
    if table.vtab.is_some() {
        return vtab_exec::exec_scan_vtab(ctx, &table, alias.as_ref(), None);
    }
    let mut rows = Vec::new();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let rowid_alias = table.rowid_alias;
    let n_cols = table.n_columns();
    bt.scan_table_borrowed(|rowid, payload| {
        // Fused inline serial decode — semantics identical to
        // `decode_row` (short rows pad NULL, alias column materializes
        // from the B+tree key, corrupt rows are skipped), but the tag
        // dispatch happens inline in the scan loop: no per-value
        // Result wrap, no re-entrant length walk, direct Value writes.
        let mut row: Vec<Value> = Vec::with_capacity(n_cols);
        let mut pos = 0usize;
        let mut col = 0usize;
        let mut ok = true;
        while col < n_cols {
            if Some(col) == rowid_alias {
                row.push(Value::Integer(rowid));
                pos += 1; // 0x09 marker (short rows: harmlessly past end)
                col += 1;
                continue;
            }
            if pos >= payload.len() {
                // Short row (ALTER TABLE ADD COLUMN): pad NULLs.
                while row.len() < n_cols {
                    row.push(Value::Null);
                }
                break;
            }
            let tag = payload[pos];
            let rest = &payload[pos + 1..];
            match tag {
                0x00 => {
                    row.push(Value::Null);
                    pos += 1;
                }
                0x01 => {
                    row.push(Value::Integer(0));
                    pos += 1;
                }
                0x02 => {
                    if rest.is_empty() {
                        ok = false;
                        break;
                    }
                    row.push(Value::Integer(rest[0] as i8 as i64));
                    pos += 2;
                }
                0x03 => {
                    if rest.len() < 2 {
                        ok = false;
                        break;
                    }
                    row.push(Value::Integer(i16::from_le_bytes([rest[0], rest[1]]) as i64));
                    pos += 3;
                }
                0x04 => {
                    if rest.len() < 4 {
                        ok = false;
                        break;
                    }
                    row.push(Value::Integer(
                        i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as i64,
                    ));
                    pos += 5;
                }
                0x05 => {
                    if rest.len() < 8 {
                        ok = false;
                        break;
                    }
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&rest[..8]);
                    row.push(Value::Integer(i64::from_le_bytes(b)));
                    pos += 9;
                }
                0x06 => {
                    if rest.len() < 8 {
                        ok = false;
                        break;
                    }
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&rest[..8]);
                    row.push(Value::Real(f64::from_le_bytes(b)));
                    pos += 9;
                }
                0x07 | 0x08 => match crate::types::value::decode_uvarint(rest) {
                    Ok((len, n)) => {
                        let len = len as usize;
                        if rest.len() < n + len {
                            ok = false;
                            break;
                        }
                        let body = &rest[n..n + len];
                        if tag == 0x07 {
                            match crate::types::text::Text::from_utf8(body) {
                                Ok(t) => row.push(Value::Text(t)),
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            }
                        } else {
                            row.push(Value::Blob(body.to_vec()));
                        }
                        pos += 1 + n + len;
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                },
                0x09 => {
                    // Alias marker at an unexpected position: NULL (same
                    // as Value::decode).
                    row.push(Value::Null);
                    pos += 1;
                }
                0x0A => match crate::types::value::decode_uvarint(rest) {
                    Ok((z, n)) => {
                        let i = ((z >> 1) as i64) ^ -((z & 1) as i64);
                        row.push(Value::Real(i as f64));
                        pos += 1 + n;
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                },
                _ => {
                    ok = false;
                    break;
                }
            }
            col += 1;
        }
        if ok && row.len() == n_cols {
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
    Ok(ExecResult { columns, rows })
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
            row.push(evaluate(
                e,
                &EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params),
            )?);
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
    // selective decode and nothing else (no Vec<Value>, no clone, no move).
    if let Plan::Scan { .. } = input {
        if let Some(res) = scan_filter_limit(ctx, input, Some(predicate), None)? {
            return Ok(res);
        }
    }

    let inner = execute(input, ctx)?;
    // Compiled-predicate eval on the materialized rows (identity positions
    // against the input's column order) — avoids the per-row AST walk and
    // name lookups of eval_row when the predicate shape is supported.
    if let Plan::Scan { table, alias, .. } = input {
        let prefix = alias.as_deref().unwrap_or(&table.name);
        if let Some(pred) = crate::executor::predicate::compile_predicate(predicate, table, prefix)
        {
            let n_cols = table.n_columns();
            let positions: Vec<usize> = (0..n_cols).collect();
            let params: &[Value] = &ctx.params;
            let mut rows = Vec::new();
            for row in inner.rows {
                if pred.eval(&row, &positions, params) {
                    rows.push(row);
                }
            }
            return Ok(ExecResult {
                columns: inner.columns,
                rows,
            });
        }
    }
    let mut rows = Vec::new();
    for row in inner.rows {
        let v = eval_row(
            predicate,
            &row,
            &inner.columns,
            &ctx.params,
            &ctx.named_params,
        )?;
        if v.is_truthy() {
            rows.push(row);
        }
    }
    Ok(ExecResult {
        columns: inner.columns,
        rows,
    })
}

/// FUSED Scan+Filter(+Limit) path: scan the table with a compiled
/// predicate, selectively decoding ONLY the predicate's columns for
/// non-matching rows, and — when `stop_after` is set — terminating the
/// scan as soon as enough rows have passed (LIMIT pushdown; the classic
/// `WHERE ... LIMIT k` shape stops at the k-th match instead of scanning
/// the whole table).
///
/// * `input` MUST be `Plan::Scan { index: None, predicate: None }`.
/// * `predicate` — the filter to apply (from the enclosing Filter node,
///   or the Scan's own pushed-down predicate). `None` accepts every row.
/// * Returns `Ok(None)` when the shape is unsupported (virtual table,
///   non-compilable predicate, index scan) so the caller falls back.
/// * `stop_after` — include offset: stop once this many rows PASS
///   (the caller then applies offset/truncation).
fn scan_filter_limit(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    predicate: Option<&Expr>,
    stop_after: Option<usize>,
) -> Result<Option<ExecResult>> {
    let Plan::Scan {
        table,
        alias,
        index: None,
        predicate: scan_pred,
    } = input
    else {
        return Ok(None);
    };
    // Effective predicate: the caller's (Filter's) predicate, the Scan's own
    // pushed-down predicate, or none — never both (that shape falls back).
    let predicate = match (predicate, scan_pred.as_ref()) {
        (Some(p), None) => Some(p),
        (None, Some(p)) => Some(p),
        (None, None) => None,
        (Some(_), Some(_)) => return Ok(None),
    };
    // Virtual table: pass the Filter's predicate into the vtab scan
    // (best_index sees it; unhandled conjuncts become the residual).
    if table.vtab.is_some() {
        if stop_after.is_none() {
            let res = vtab_exec::exec_scan_vtab(ctx, table, alias.as_ref(), predicate)?;
            return Ok(Some(res));
        }
        return Ok(None); // vtab LIMIT pushdown is the vtab's business
    }
    let prefix = alias.as_deref().unwrap_or(&table.name);
    let pred = match predicate {
        Some(p) => match crate::executor::predicate::compile_predicate(p, table, prefix) {
            Some(c) => Some(c),
            None => return Ok(None),
        },
        None => None, // accept every row
    };
    let n_cols = table.n_columns();
    let root = ctx.table_root(table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let rowid_alias = table.rowid_alias;
    let params: &[Value] = &ctx.params;
    // Selective decode for the predicate: decode ONLY the columns the
    // predicate references while scanning; decode the full row just for
    // the (usually few) rows that PASS. For a selective filter over a wide
    // table this skips the dominant per-row decode cost of every
    // non-matching row.
    let mut wanted: Vec<usize> = Vec::new();
    if let Some(p) = &pred {
        crate::executor::predicate::compiled_columns(p, &mut wanted);
    }
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
    let mut rows: Vec<Vec<Value>> = Vec::new();
    match (&pred, !wanted.is_empty()) {
        (Some(pred), true) => {
            wanted.sort_unstable();
            wanted.dedup();
            let mut positions = vec![usize::MAX; n_cols];
            for (pos, &c) in wanted.iter().enumerate() {
                positions[c] = pos;
            }
            let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
            let stop = stop_after;
            bt.scan_table_borrowed(|rowid, payload| {
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
                if !pred.eval(&sel_buf, &positions, params) {
                    return true; // non-matching row: cost = selective decode only
                }
                // Matching row: materialize the full row for output.
                if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
                    rows.push(row);
                }
                // LIMIT pushdown: stop the scan once enough rows have passed.
                if let Some(stop) = stop {
                    if rows.len() >= stop {
                        return false;
                    }
                }
                true
            })?;
        }
        (Some(pred), false) => {
            // Degenerate predicate (constant): full decode as before.
            let positions: Vec<usize> = (0..n_cols).collect();
            let stop = stop_after;
            bt.scan_table_borrowed(|rowid, payload| {
                if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
                    if pred.eval(&row, &positions, params) {
                        rows.push(row);
                    }
                }
                if let Some(stop) = stop {
                    if rows.len() >= stop {
                        return false;
                    }
                }
                true
            })?;
        }
        (None, _) => {
            // No predicate: accept every row (bare Scan + LIMIT).
            let stop = stop_after;
            bt.scan_table_borrowed(|rowid, payload| {
                if let Ok(row) = decode_row(payload, n_cols, rowid, rowid_alias) {
                    rows.push(row);
                }
                if let Some(stop) = stop {
                    if rows.len() >= stop {
                        return false;
                    }
                }
                true
            })?;
        }
    }
    Ok(Some(ExecResult { columns, rows }))
}

// ============================================================================
// Project
// ============================================================================

fn exec_project(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    columns: &[ProjectExpr],
) -> Result<ExecResult> {
    let inner = execute(input, ctx)?;
    apply_projection(inner, columns, ctx)
}

/// Apply a projection to an ALREADY-EXECUTED input. Split out of
/// `exec_project` so the fused hash-join path can fall back to normal
/// projection semantics when its fusion preconditions don't hold.
fn apply_projection(
    inner: ExecResult,
    columns: &[ProjectExpr],
    ctx: &ExecContext<'_>,
) -> Result<ExecResult> {
    // Compute output columns, expanding `*` and `table.*` to the underlying
    // input column names.
    let mut out_columns: Vec<String> = Vec::new();
    let mut star_expansions: Vec<Vec<String>> = Vec::new(); // for each column, the list of expanded names (or empty)
    for c in columns {
        if let Expr::Column { name, .. } = &c.expr {
            if name == "*" {
                // Expand to all input columns.
                let expanded: Vec<String> = inner
                    .columns
                    .iter()
                    .map(|c| {
                        if let Some(pos) = c.rfind('.') {
                            c[pos + 1..].to_string()
                        } else {
                            c.clone()
                        }
                    })
                    .collect();
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
        && columns
            .iter()
            .all(|c| !matches!(&c.expr, Expr::Column { name, .. } if name == "*"));

    // ---- IDENTITY FAST PATH ----
    // `SELECT *`, `SELECT t.*`-style star projections, or any projection
    // that selects every input column in order produce rows identical to
    // the input rows. Move them instead of cloning every value into fresh
    // Vecs — for a 1000-row scan that's 1000 allocations + 2000+ Value
    // clones eliminated. Only the column NAMES can differ (aliases / star
    // unqualification); the values are byte-identical.
    let single_star =
        columns.len() == 1 && matches!(&columns[0].expr, Expr::Column { name, .. } if name == "*");
    let identity_projection = single_star
        || (all_resolved
            && resolved.len() == inner.columns.len()
            && resolved.iter().enumerate().all(|(i, r)| r == &Some(i)));
    if identity_projection {
        return Ok(ExecResult {
            columns: out_columns.into(),
            rows: inner.rows,
        });
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
                out.push(eval_row(
                    &c.expr,
                    row,
                    &inner.columns,
                    &ctx.params,
                    &ctx.named_params,
                )?);
            }
            out_rows.push(out);
        }
    }
    Ok(ExecResult {
        columns: out_columns.into(),
        rows: out_rows,
    })
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
/// Two-side column resolution over a JOIN's combined schema WITHOUT
/// materializing the combined `Vec<String>` (which cost a `to_vec` clone
/// of the outer names + one `format!` per inner column, per query, in the
/// join fusion path). Semantics replicate `resolve_column_index` applied
/// to `outer_cols ++ ["prefix.col" for each inner col]`: three passes
/// (qualified exact, unqualified exact, suffix), outer entries before
/// inner entries WITHIN each pass. Inner entries are virtual
/// "inner_prefix.col" strings — compared structurally, never built.
#[allow(clippy::too_many_arguments)]
fn resolve_column_index_two_sides(
    outer_cols: &[String],
    inner_prefix: &str,
    inner_cols: &[crate::schema::Column],
    table: Option<&str>,
    name: &str,
) -> Option<usize> {
    let n_outer = outer_cols.len();
    // Pass 1: qualified exact "t.c".
    if let Some(t) = table {
        for (i, n) in outer_cols.iter().enumerate() {
            if n.len() == t.len() + 1 + name.len()
                && n.as_bytes().get(t.len()) == Some(&b'.')
                && n[..t.len()].eq_ignore_ascii_case(t)
                && n[t.len() + 1..].eq_ignore_ascii_case(name)
            {
                return Some(i);
            }
        }
        // Inner virtual entries: "prefix.col".
        for (j, c) in inner_cols.iter().enumerate() {
            let cname = c.name.as_str();
            if inner_prefix.len() == t.len()
                && cname.len() == name.len()
                && inner_prefix.eq_ignore_ascii_case(t)
                && cname.eq_ignore_ascii_case(name)
            {
                return Some(n_outer + j);
            }
        }
        // Fall through to unqualified resolution.
    }
    // Pass 2: exact unqualified match against the full (virtual) entry.
    for (i, n) in outer_cols.iter().enumerate() {
        if n.eq_ignore_ascii_case(name) {
            return Some(i);
        }
    }
    for (j, c) in inner_cols.iter().enumerate() {
        let cname = c.name.as_str();
        if name.len() == inner_prefix.len() + 1 + cname.len()
            && name.as_bytes().get(inner_prefix.len()) == Some(&b'.')
            && name[..inner_prefix.len()].eq_ignore_ascii_case(inner_prefix)
            && name[inner_prefix.len() + 1..].eq_ignore_ascii_case(cname)
        {
            return Some(n_outer + j);
        }
    }
    // Pass 3: suffix match (after the last dot).
    for (i, n) in outer_cols.iter().enumerate() {
        if let Some(pos) = n.rfind('.') {
            if n[pos + 1..].eq_ignore_ascii_case(name) {
                return Some(i);
            }
        }
    }
    // Inner virtual "prefix.col": suffix = col name (prefix has no dot in
    // any realistic schema).
    for (j, c) in inner_cols.iter().enumerate() {
        if c.name.eq_ignore_ascii_case(name) {
            return Some(n_outer + j);
        }
    }
    None
}

fn resolve_column_index<T: AsRef<str>>(
    col_names: &[T],
    table: Option<&str>,
    name: &str,
) -> Option<usize> {
    // Allocation-free: the old version built `to_ascii_lowercase()` copies
    // of the qualifier, the name, AND every candidate column name — 3+
    // heap Strings per resolution, paid per projected column per query in
    // the join fusion paths. Generic over `AsRef<str>` so callers can pass
    // `&[String]` OR a borrowed `Vec<&str>` combined list (the fused join
    // avoids cloning every column String per query).
    if let Some(t) = table {
        // Qualified exact match: "table.column" (case-insensitive on both
        // components, dot at exactly `t.len()`). Runs over the WHOLE list
        // before any unqualified fallback — the pass ORDER is what makes
        // `b.id` resolve to the b side rather than a same-named `a.id`
        // via the suffix pass.
        for (i, n) in col_names.iter().enumerate() {
            let n = n.as_ref();
            if n.len() == t.len() + 1 + name.len()
                && n.as_bytes().get(t.len()) == Some(&b'.')
                && n[..t.len()].eq_ignore_ascii_case(t)
                && n[t.len() + 1..].eq_ignore_ascii_case(name)
            {
                return Some(i);
            }
        }
        // Fall through to unqualified resolution (mirrors lookup()).
    }
    // Exact match first.
    for (i, n) in col_names.iter().enumerate() {
        if n.as_ref().eq_ignore_ascii_case(name) {
            return Some(i);
        }
    }
    // Qualified match by suffix (e.g. "u.id" matches ref "id").
    for (i, n) in col_names.iter().enumerate() {
        let n = n.as_ref();
        if let Some(pos) = n.rfind('.') {
            if n[pos + 1..].eq_ignore_ascii_case(name) {
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
/// SQLite-style output-column naming (alias or expression text), shared
/// with the C ABI compatibility layer for `sqlite3_column_name` at
/// PREPARE time (before the first step materializes the result).
pub fn expr_display_name(e: &Expr) -> String {
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
        Expr::Function {
            name,
            distinct,
            args,
            ..
        } => {
            let rendered: Vec<String> = args
                .iter()
                .map(|a| match a {
                    Expr::Column { name, .. } if name == "*" => "*".to_string(),
                    Expr::Column { name, .. } => name.clone(),
                    Expr::Literal(v) => format!("{}", v),
                    _ => "?".to_string(),
                })
                .collect();
            if rendered.is_empty() {
                format!("{}()", name)
            } else if *distinct {
                format!("{}(DISTINCT {})", name, rendered.join(", "))
            } else {
                format!("{}({})", name, rendered.join(", "))
            }
        }
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

    // Ordinal validation up front: `ORDER BY <positive int K>` means the
    // K-th OUTPUT column (SQLite semantics). A comparator closure cannot
    // return Err, so out-of-range ordinals must be rejected before the
    // sort begins. (Negative or zero literals are constants — SQLite
    // treats them as no-op sort terms, not errors.)
    //
    // This is the execution-time half of ordinal resolution; the planner
    // resolves ordinals against explicit projections, and this path serves
    // star projections, compound (UNION/EXCEPT/INTERSECT) bodies and
    // subqueries — every shape where the sort's input width equals the
    // SELECT's output width.
    let row_width = inner.rows.first().map_or(0, |r| r.len());
    if row_width > 0 {
        for term in terms {
            if let Expr::Literal(Value::Integer(k)) = &term.expr {
                if *k >= 1 && (*k as usize) > row_width {
                    return Err(Error::semantic(format!(
                        "{}st ORDER BY term out of range ({} output columns)",
                        k, row_width
                    )));
                }
            }
        }
    }

    inner.rows.sort_by(|a, b| {
        for term in terms {
            let va = sort_key(&term.expr, a, &columns, params, named_params);
            let vb = sort_key(&term.expr, b, &columns, params, named_params);
            // ORDER BY ... COLLATE name: compare text through the named
            // collation (SQL semantics: collations affect only text pairs).
            let ord = if let Expr::Collate { collation, .. } = &term.expr {
                crate::plugin::lookup_collation(collation)
                    .map(|c| crate::plugin::compare_collated(&va, &vb, c.as_ref()))
                    .unwrap_or_else(|| va.cmp(&vb))
            } else {
                va.cmp(&vb)
            };
            let ord = if term.order == Order::Desc {
                ord.reverse()
            } else {
                ord
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(inner)
}

/// Evaluate one ORDER BY term for one row. Handles the two key shapes:
/// - `Expr::Literal(Integer(k))` with k >= 1 — the ordinal form: the k-th
///   column of the row itself (`ORDER BY 1, 2`).
/// - anything else — ordinary expression evaluation against the row.
///
/// Ordinals were range-validated by `exec_sort` before sorting, so a
/// missing index here degrades to NULL rather than panicking.
#[inline]
fn sort_key(
    expr: &Expr,
    row: &[Value],
    columns: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Value {
    if let Expr::Literal(Value::Integer(k)) = expr {
        if *k >= 1 {
            return row.get(*k as usize - 1).cloned().unwrap_or(Value::Null);
        }
    }
    eval_row(expr, row, columns, params, named_params).unwrap_or(Value::Null)
}

/// Bounded top-N selection for `Limit(Sort(x))` (see exec_limit's fusion).
/// `keep = offset + count` rows survive; the output window is
/// `[offset .. keep]` of the selected prefix, matching the general path's
/// sort-then-slice semantics exactly.
fn exec_topn(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    term: &OrderTerm,
    keep: usize,
    offset: usize,
) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let columns = &inner.columns;

    // Ordinal validation (mirrors exec_sort).
    let row_width = inner.rows.first().map_or(0, |r| r.len());
    if row_width > 0 {
        if let Expr::Literal(Value::Integer(k)) = &term.expr {
            if *k >= 1 && (*k as usize) > row_width {
                return Err(Error::semantic(format!(
                    "{}st ORDER BY term out of range ({} output columns)",
                    k, row_width
                )));
            }
        }
    }

    if keep == 0 {
        inner.rows.clear();
        return Ok(inner);
    }
    if keep >= inner.rows.len() {
        // The bound covers everything: a plain sort is the same result and
        // avoids the selection machinery.
        let rows = &mut inner.rows;
        let term_expr = &term.expr;
        let cols = &inner.columns;
        let params: &[Value] = &ctx.params;
        let named = &ctx.named_params;
        rows.sort_by(|a, b| {
            let va = sort_key(term_expr, a, cols, params, named);
            let vb = sort_key(term_expr, b, cols, params, named);
            let ord = if let Expr::Collate { collation, .. } = term_expr {
                crate::plugin::lookup_collation(collation)
                    .map(|c| crate::plugin::compare_collated(&va, &vb, c.as_ref()))
                    .unwrap_or_else(|| va.cmp(&vb))
            } else {
                va.cmp(&vb)
            };
            if term.order == Order::Desc {
                ord.reverse()
            } else {
                ord
            }
        });
        if offset > 0 {
            let drop = offset.min(inner.rows.len());
            inner.rows.drain(..drop);
        }
        return Ok(inner);
    }

    // One key per row, extracted ONCE (the full sort evaluates the key
    // expression twice per comparison — O(n log n) evals; this is O(n)).
    // Bare-column / ordinal terms resolve to a row index up front — no
    // per-row name lookup. Ambiguous bare names (more than one suffix
    // match) keep the general eval so key semantics can never diverge
    // from the full sort.
    let row_width = inner.rows.first().map_or(0, |r| r.len());
    let key_idx: Option<usize> = match &term.expr {
        Expr::Literal(Value::Integer(k)) if *k >= 1 && (*k as usize) <= row_width => {
            Some(*k as usize - 1)
        }
        Expr::Column { table: None, name } => {
            let hits: Vec<usize> = columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.rsplit('.').next() == Some(name.as_str()))
                .map(|(i, _)| i)
                .collect();
            if hits.len() == 1 {
                Some(hits[0])
            } else {
                None
            }
        }
        Expr::Column {
            table: Some(t),
            name,
        } => {
            let qualified = format!("{t}.{name}");
            columns.iter().position(|c| *c == qualified)
        }
        _ => None,
    };
    let keys: Vec<Value> = if let Some(i) = key_idx {
        inner
            .rows
            .iter()
            .map(|r| r.get(i).cloned().unwrap_or(Value::Null))
            .collect()
    } else {
        inner
            .rows
            .iter()
            .map(|r| sort_key(&term.expr, r, columns, params, named_params))
            .collect()
    };
    let desc = term.order == Order::Desc;
    let coll = match &term.expr {
        Expr::Collate { collation, .. } => crate::plugin::lookup_collation(collation),
        _ => None,
    };
    // Ties break by INPUT ROW ORDER (index ascending) — deterministic and
    // matching SQLite's stable sorter for scan-sourced rows, so equal keys
    // keep the rowid order on both ASC and DESC terms.
    let cmp_idx = |a: &u32, b: &u32| -> std::cmp::Ordering {
        let (ka, kb) = (&keys[*a as usize], &keys[*b as usize]);
        let ord = match coll.as_deref() {
            Some(c) => crate::plugin::compare_collated(ka, kb, c),
            None => ka.cmp(kb),
        };
        let ord = if desc { ord.reverse() } else { ord };
        ord.then(a.cmp(b))
    };
    let n = inner.rows.len();
    let mut idx: Vec<u32> = (0..n as u32).collect();
    // Partial selection: after `select_nth_unstable_by(keep - 1, ...)`, the
    // element at index keep-1 is the keep-th smallest and idx[..keep] holds
    // exactly the `keep` smallest keys (unordered; the returned head alone
    // would exclude the pivot — the classic off-by-one).
    idx.select_nth_unstable_by(keep - 1, cmp_idx);
    idx[..keep].sort_unstable_by(cmp_idx);
    let window: Vec<Row> = idx[offset.min(keep)..keep]
        .iter()
        .map(|&i| inner.rows[i as usize].clone())
        .collect();
    inner.rows = window;
    Ok(inner)
}

// ============================================================================
// Limit
// =========================================================================

///  —
/// the shapes  and  fuse into the
/// bounded top-N selection.
type TopnTarget<'a> = Option<(&'a Plan, &'a Vec<OrderTerm>, Option<&'a Vec<ProjectExpr>>)>;

fn exec_limit(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    count: &Expr,
    offset: &Expr,
) -> Result<ExecResult> {
    // TOP-N FUSION: `Limit(Sort(x))` with literal bounds and a SINGLE sort
    // term keeps only the top (offset + count) rows via bounded partial
    // selection — O(n) key extraction + O(n) selection + O(k log k) sort —
    // instead of materializing and fully sorting the input (SQLite's
    // sorter applies the same LIMIT bound). Multi-term sorts,
    // non-literal bounds, or a bound beyond the cap keep the general path.
    // Shape A: Limit(Sort(x)). Shape B: Limit(Project(Sort(x))) — the
    // planner inserts Sort BELOW Project so ORDER BY can reference
    // unprojected columns; the projection is 1:1, so fusing through it
    // (top-N first, then project the survivors) yields identical rows.
    // (sort input, terms, optional 1:1 projection to apply after top-N)
    let topn_target: TopnTarget<'_> = match input {
        Plan::Sort {
            input: sort_input,
            terms,
        } if terms.len() == 1 => Some((sort_input, terms, None)),
        Plan::Project {
            input: inner,
            columns,
        } => match inner.as_ref() {
            Plan::Sort {
                input: sort_input,
                terms,
            } if terms.len() == 1 => Some((sort_input, terms, Some(columns))),
            _ => None,
        },
        _ => None,
    };
    if let Some((sort_input, terms, projection)) = topn_target {
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
        if let (Ok(count_val), Ok(offset_val)) =
            (evaluate(count, &eval_ctx), evaluate(offset, &eval_ctx))
        {
            let count_i = count_val.as_integer();
            let offset_i = offset_val.as_integer().max(0);
            if count_i >= 0 && offset_i <= (i64::MAX / 4) && count_i <= (1 << 20) {
                let total = count_i.saturating_add(offset_i) as usize;
                let res = exec_topn(ctx, sort_input, &terms[0], total, offset_i as usize)?;
                if let Some(columns) = projection {
                    return apply_projection(res, columns, ctx);
                }
                return Ok(res);
            }
        }
    }
    // LIMIT PUSHDOWN: for `Limit(Filter(Scan))` and `Limit(Scan)`, stop the
    // scan as soon as `offset + count` rows have passed the filter instead
    // of materializing the whole table and truncating — the classic
    // `WHERE ... LIMIT k` top-k shape. Only valid without a Sort between
    // them (ordering is the input's natural scan order, which is exactly
    // what the fallback would produce too).
    {
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
        if let Ok(count_val) = evaluate(count, &eval_ctx) {
            if let Ok(offset_val) = evaluate(offset, &eval_ctx) {
                let count_i = count_val.as_integer();
                let offset_i = offset_val.as_integer().max(0);
                if count_i >= 0 && offset_i <= (i64::MAX / 2) {
                    let total = count_i.saturating_add(offset_i);
                    if total >= 0 {
                        let stop = (total as usize).min(1 << 40);
                        // Shapes that allow pushing the limit into the scan:
                        // Limit(Filter(Scan)), Limit(Scan), and the common
                        // SELECT shape Limit(Project(Filter(Scan))) /
                        // Limit(Project(Scan)) — the projection is 1:1, so
                        // limiting before projecting yields identical rows.
                        let pushed: Option<ExecResult> = match input {
                            Plan::Filter {
                                input: inner,
                                predicate,
                            } => scan_filter_limit(ctx, inner, Some(predicate), Some(stop))?,
                            Plan::Scan { index: None, .. } => {
                                scan_filter_limit(ctx, input, None, Some(stop))?
                            }
                            Plan::Project {
                                input: inner,
                                columns,
                            } => match inner.as_ref() {
                                Plan::Filter {
                                    input: scan,
                                    predicate,
                                } => scan_filter_limit(ctx, scan, Some(predicate), Some(stop))?
                                    .map(|res| apply_projection(res, columns, ctx))
                                    .transpose()?,
                                Plan::Scan { index: None, .. } => {
                                    scan_filter_limit(ctx, inner, None, Some(stop))?
                                        .map(|res| apply_projection(res, columns, ctx))
                                        .transpose()?
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        if let Some(mut res) = pushed {
                            let skip = offset_i as usize;
                            if skip >= res.rows.len() {
                                res.rows.clear();
                            } else {
                                res.rows.drain(0..skip);
                                if count_i >= 0 {
                                    let c = count_i as usize;
                                    if res.rows.len() > c {
                                        res.rows.truncate(c);
                                    }
                                }
                            }
                            return Ok(res);
                        }
                    }
                }
            }
        }
    }
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
#[derive(Default)]
struct HashGrouper {
    buckets: HashMap<u64, Vec<usize>, crate::api::FxHashBuild>,
    /// (key values, per-aggregate states) in first-seen order.
    groups: Vec<(Vec<Value>, Vec<AggState>)>,
}

impl HashGrouper {
    /// Find or create the group for `key`, returning its index.
    /// `key` is typically a reusable scratch buffer — its contents are
    /// cloned only when a NEW group is created.
    fn intern(&mut self, key: &[Value]) -> usize {
        // FxHash-style multiply-rotate hasher: ~5-10 ns for the typical
        // 1-2 value key vs ~25-40 ns for SipHash (the previous
        // `DefaultHasher`). The hash quality is more than sufficient for
        // group interning — collisions cost one wasted values_sql_equal.
        let mut hasher = crate::api::FxHasher::default();
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

    /// Single-key variant: skips the key-slice machinery entirely (no
    /// `key_buf` Vec, no length header, one value hash + one direct
    /// comparison). This is the overwhelmingly common `GROUP BY <expr>`
    /// shape.
    fn intern_one(&mut self, key: &Value) -> usize {
        let mut hasher = crate::api::FxHasher::default();
        hash_sql_value(key, &mut hasher);
        let h = hasher.finish();
        if let Some(bucket) = self.buckets.get(&h) {
            for &gi in bucket {
                let (existing, _) = &self.groups[gi];
                if existing.len() == 1 && crate::types::values_sql_equal(&existing[0], key) {
                    return gi;
                }
            }
        }
        let gi = self.groups.len();
        self.groups.push((vec![key.clone()], Vec::new()));
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
            sum_is_int: true, // Optimistic: assume int until we see a Real.
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
    let columns: Vec<String> = table
        .columns
        .iter()
        .map(|c| format!("{}.{}", prefix, c.name))
        .collect();

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
                let matches = ref_t
                    .as_ref()
                    .map(|t| {
                        // SQL scoping: an alias REPLACES the table name —
                        // `t.col` must NOT bind to a `FROM t t2` instance
                        // (otherwise a correlated reference to an outer
                        // un-aliased `t` is silently captured by the inner
                        // alias and compared against itself).
                        if prefix == table.name {
                            t == &table.name || t == prefix
                        } else {
                            t == prefix
                        }
                    })
                    .unwrap_or(true);
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
                let matches = ref_t
                    .as_ref()
                    .map(|t| {
                        // SQL scoping: an alias REPLACES the table name —
                        // `t.col` must NOT bind to a `FROM t t2` instance
                        // (otherwise a correlated reference to an outer
                        // un-aliased `t` is silently captured by the inner
                        // alias and compared against itself).
                        if prefix == table.name {
                            t == &table.name || t == prefix
                        } else {
                            t == prefix
                        }
                    })
                    .unwrap_or(true);
                if matches {
                    table.find_column(name)
                } else {
                    None
                }
            }
            _ => None, // COUNT(*) or a non-column arg
        })
        .collect();
    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();

    let all_resolved =
        key_col_indices.iter().all(|x| x.is_some()) && agg_col_indices.iter().all(|x| x.is_some());
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
        // Per-agg decode positions + COUNT(*) flags resolved ONCE (was a
        // `wanted.iter().position()` scan + Value clone per agg per row).
        let agg_pos_gb: Vec<Option<usize>> = agg_col_indices
            .iter()
            .map(|widx| widx.and_then(|c| wanted.iter().position(|x| *x == c)))
            .collect();
        let agg_count_star_gb: Vec<bool> = aggregates.iter().map(|a| a.arg.is_none()).collect();
        let key_pos_gb: Vec<usize> = key_col_indices
            .iter()
            .map(|k| {
                k.and_then(|c| wanted.iter().position(|x| *x == c))
                    .unwrap_or(usize::MAX)
            })
            .collect();
        let rowid_alias = table.rowid_alias;
        bt.scan_table_borrowed(|rowid, payload| {
            if decode_row_selective(payload, n_cols, &wanted, rowid, rowid_alias, &mut sel_buf)
                .is_err()
            {
                return true; // skip corrupt rows
            }
            // Build the group key from the decoded slice (positions
            // precomputed above — direct index, no per-row search).
            key_buf.clear();
            for &pos in key_pos_gb.iter() {
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
            for i in 0..aggregates.len() {
                let arg_val: &Value = if agg_count_star_gb[i] {
                    &COUNT_STAR_ARG // COUNT(*): constant placeholder, no evaluation
                } else {
                    match agg_pos_gb[i] {
                        Some(pos) if pos < sel_buf.len() => &sel_buf[pos],
                        _ => &Value::Null,
                    }
                };
                update_agg_state(
                    &mut grouper.groups[gi].1[i],
                    agg_funcs[i],
                    arg_val,
                    aggregates[i].distinct,
                );
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

        // ---- Compiled-expression fast path ------------------------------
        // GROUP BY keys and aggregate args that are ARITHMETIC over
        // columns/literals/params (`GROUP BY val / 100`, `SUM(val + 1)`)
        // compile once into positional trees (`CompiledExpr`) — ~5-15
        // ns/row instead of the ~60-120 ns `eval_row` AST walk with its
        // per-row name resolution. When EVERY key and arg compiles, the
        // decode can also be SELECTIVE (only the referenced columns),
        // through a full-width buffer so identity indexing keeps working.
        let params_len = params.len();
        let compiled_keys: Vec<Option<crate::executor::predicate::CompiledExpr>> = group_by
            .iter()
            .map(|e| crate::executor::predicate::compile_expr_scoped(e, &table, prefix, params_len))
            .collect();
        let compiled_args: Vec<Option<crate::executor::predicate::CompiledExpr>> = aggregates
            .iter()
            .enumerate()
            .map(|(i, agg)| match &agg.arg {
                Some(arg) => {
                    // Bare columns keep their zero-cost direct index path.
                    if agg_col_indices[i].is_some() {
                        None
                    } else {
                        crate::executor::predicate::compile_expr_scoped(
                            arg, &table, prefix, params_len,
                        )
                    }
                }
                None => None, // COUNT(*): no argument to evaluate
            })
            .collect();
        let keys_all_compile =
            compiled_keys.iter().all(|k| k.is_some()) && compiled_keys.len() == group_by.len();
        let args_all_compile = aggregates.iter().enumerate().all(|(i, agg)| {
            agg.arg.is_none() || agg_col_indices[i].is_some() || compiled_args[i].is_some()
        });

        let single_key = group_by.len() == 1;
        let rowid_alias = table.rowid_alias;

        if keys_all_compile && args_all_compile {
            // Referenced columns = union of compiled key/arg columns +
            // bare-column indices, ascending + deduped.
            let mut wanted: Vec<usize> = Vec::with_capacity(n_cols);
            for k in compiled_keys.iter().flatten() {
                crate::executor::predicate::compiled_expr_columns(k, &mut wanted);
            }
            for (i, _agg) in aggregates.iter().enumerate() {
                if let Some(idx) = agg_col_indices[i] {
                    wanted.push(idx);
                } else if let Some(c) = &compiled_args[i] {
                    crate::executor::predicate::compiled_expr_columns(c, &mut wanted);
                }
            }
            wanted.sort_unstable();
            wanted.dedup();

            let mut wide: Vec<Value> = vec![Value::Null; n_cols];
            let mut owned_key: Value = Value::Null;
            bt.scan_table_borrowed(|rowid, payload| {
                if decode_row_selective_wide(
                    payload,
                    n_cols,
                    &wanted,
                    rowid,
                    rowid_alias,
                    &mut wide,
                )
                .is_err()
                {
                    return true; // skip corrupt rows
                }
                if let Some(pred) = filter_predicate {
                    let keep = if let Some(cp) = &compiled_filter {
                        cp.eval(&wide, &identity, params)
                    } else {
                        match eval_row(pred, &wide, &columns, params, named_params) {
                            Ok(v) => v.is_truthy(),
                            Err(_) => false,
                        }
                    };
                    if !keep {
                        return true;
                    }
                }
                // Group key: single-value fast path or full key Vec.
                let gi = if single_key {
                    let kv = match &compiled_keys[0] {
                        Some(c) => c.eval(&wide, params),
                        None => wide[key_col_indices[0].unwrap()].clone(),
                    };
                    owned_key = kv;
                    grouper.intern_one(&owned_key)
                } else {
                    key_buf.clear();
                    for (i, _g) in group_by.iter().enumerate() {
                        let kv = match &compiled_keys[i] {
                            Some(c) => c.eval(&wide, params),
                            None => wide[key_col_indices[i].unwrap()].clone(),
                        };
                        key_buf.push(kv);
                    }
                    grouper.intern(&key_buf)
                };
                if grouper.groups[gi].1.is_empty() {
                    grouper.groups[gi].1 = (0..n_aggs).map(|_| AggState::default()).collect();
                }
                for (i, agg) in aggregates.iter().enumerate() {
                    if agg.arg.is_none() {
                        // COUNT(*): constant placeholder, no evaluation.
                        update_agg_state(
                            &mut grouper.groups[gi].1[i],
                            agg_funcs[i],
                            &COUNT_STAR_ARG,
                            false,
                        );
                        continue;
                    }
                    let arg_val = match (agg_col_indices[i], &compiled_args[i]) {
                        (Some(idx), _) => wide[idx].clone(),
                        (None, Some(c)) => c.eval(&wide, params),
                        (None, None) => Value::Null,
                    };
                    update_agg_state(
                        &mut grouper.groups[gi].1[i],
                        agg_funcs[i],
                        &arg_val,
                        agg.distinct,
                    );
                }
                true
            })?;
            return finish_group_result(grouper, group_by, aggregates);
        }

        let identity_ref: &[usize] = &identity;
        bt.scan_table_borrowed(|rowid, payload| {
            row_buf.clear();
            if decode_row_into(payload, n_cols, rowid, rowid_alias, &mut row_buf).is_err() {
                return true; // skip corrupt rows
            }
            // Apply the filter predicate inline (if any).
            if let Some(pred) = filter_predicate {
                let keep = if let Some(cp) = &compiled_filter {
                    cp.eval(&row_buf, identity_ref, params)
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
                    (Some(arg), None) => {
                        match eval_row(arg, &row_buf, &columns, params, named_params) {
                            Ok(v) => v,
                            Err(_) => Value::Null,
                        }
                    }
                    (None, _) => Value::Integer(1), // COUNT(*)
                };
                update_agg_state(
                    &mut grouper.groups[gi].1[i],
                    agg_funcs[i],
                    &arg_val,
                    agg.distinct,
                );
            }
            true
        })?;
    }

    finish_group_result(grouper, group_by, aggregates)
}

/// Emit one output row per group, in first-seen order (matches the
/// previous implementation's `group_order` behavior). Shared by the
/// compiled-expression fast path and the general path.
fn finish_group_result(
    grouper: HashGrouper,
    group_by: &[Expr],
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
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
            Expr::Column {
                table: Some(t),
                name,
            } => format!("{}.{}", t, name),
            _ => format!("col{}", i + 1),
        };
        out_cols.push(name);
    }
    for (i, _agg) in aggregates.iter().enumerate() {
        out_cols.push(format!("__agg_{}", i));
    }

    Ok(ExecResult {
        columns: out_cols.into(),
        rows: out_rows,
    })
}

/// COUNT(*) placeholder argument: update_agg_state's non-NULL integer,
/// shared so the fast path doesn't build a fresh Value per row.
static COUNT_STAR_ARG: Value = Value::Integer(1);

// Vectorized fast path for `SELECT <aggregates> FROM t [WHERE pred]`
// (no GROUP BY). Key optimizations vs the generic streaming-scan path:
//
// 1. **No HashMap of groups** — only one group, so we accumulate directly
//    into a `Vec<AggState>`. Saves the per-row String key formatting
//    (which was ~200 ns/row on a 4-column table — 4× Debug-formatted
//    Values + a join("|") allocation).
//
// 2. **Column index resolution upfront** — if every aggregate's arg is a
//    bare `Expr::Column`, we resolve the column index ONCE before the
//    scan, and during the scan we read `row_buf[idx]` directly (a Vec
//    index, ~1 ns) instead of calling `eval_row` (which does name lookup
//    + type coercion + Result wrapping, ~100 ns/row).
//
// 3. **No per-row column-name formatting** — we only build the column-name
//    Vec if we actually need it (filter predicate or non-Column aggregate
//    args). For `SELECT SUM(x), COUNT(*) FROM t` (no predicate), we never
//    build it.
//
// Together these optimizations cut the per-row overhead by ~10x for the
// common OLAP case, bringing aggregate scan within ~2x of SQLite (from
// the previous ~6x gap).

// ---------------------------------------------------------------------------
// Fused typed-aggregate machine (no GROUP BY)
// ---------------------------------------------------------------------------
// The generic paths decode each row into `Value`s and dispatch through
// `update_agg_state`'s enum per aggregate (~14 ns/aggregate/row). For the
// hot OLAP shape — bare-column COUNT/SUM/MIN/MAX/AVG/TOTAL with an
// optional `col <op> numeric-literal` filter — this machine decodes the
// row's serial types INLINE (no Vec<Value>, no per-value Result, no
// marker/perm machinery) and accumulates directly in typed slots.
// Falls back to the generic paths on any shape/type it cannot reproduce
// EXACTLY (distinct, text/blob values, NaN, params, non-column args).

/// A numeric-only cell value (Copy — stack resident).
#[derive(Clone, Copy)]
enum NumVal {
    Null,
    I(i64),
    F(f64),
}

impl NumVal {
    #[inline]
    fn as_f64(self) -> f64 {
        match self {
            NumVal::I(i) => i as f64,
            NumVal::F(f) => f,
            NumVal::Null => 0.0,
        }
    }
}

#[derive(Clone, Copy)]
enum FusedOp {
    CountStar,
    CountCol,
    Sum,
    Total,
    Avg,
    Min,
    Max,
}

#[derive(Clone, Copy)]
enum FusedCmp {
    Eq,
    Neq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

struct FusedFilter {
    col: usize,
    lit: NumVal,
    cmp: FusedCmp,
}

struct FusedAggSpec {
    op: FusedOp,
    /// Decoded-slot position of the argument (None for COUNT(*)).
    slot: Option<usize>,
}

/// Maximum distinct argument/filter columns the fused machine handles
/// (stack array). Wider shapes fall back to the generic paths.
const FUSED_MAX_COLS: usize = 8;

/// Resolve a bare column reference against `table` with the same
/// alias-scoping rules the generic paths use.
fn fused_resolve_col(expr: &Expr, table: &Table, prefix: &str) -> Option<usize> {
    if let Expr::Column { table: ref_t, name } = expr {
        let matches_t = ref_t
            .as_ref()
            .map(|t| {
                if prefix == table.name {
                    t == &table.name || t == prefix
                } else {
                    t == prefix
                }
            })
            .unwrap_or(true);
        if matches_t {
            table.find_column(name)
        } else {
            None
        }
    } else {
        None
    }
}

/// Extract `col <op> numeric-literal` (either operand order) from a
/// predicate. `None` = not a supported simple numeric comparison.
fn fused_filter_of(pred: &Expr, table: &Table, prefix: &str) -> Option<FusedFilter> {
    if let Expr::Binary { op, left, right } = pred {
        let cmp = match op {
            BinaryOp::Eq => FusedCmp::Eq,
            BinaryOp::NotEq => FusedCmp::Neq,
            BinaryOp::Lt => FusedCmp::Lt,
            BinaryOp::LtEq => FusedCmp::LtEq,
            BinaryOp::Gt => FusedCmp::Gt,
            BinaryOp::GtEq => FusedCmp::GtEq,
            _ => return None,
        };
        // Literal side must be numeric.
        let lit_of = |e: &Expr| -> Option<NumVal> {
            if let Expr::Literal(v) = e {
                match v {
                    Value::Integer(i) => Some(NumVal::I(*i)),
                    Value::Real(f) => Some(NumVal::F(*f)),
                    _ => None,
                }
            } else {
                None
            }
        };
        if let (Some(col), Some(lit)) = (fused_resolve_col(left, table, prefix), lit_of(right)) {
            return Some(FusedFilter { col, lit, cmp });
        }
        if let (Some(col), Some(lit)) = (fused_resolve_col(right, table, prefix), lit_of(left)) {
            // Operand order swapped: mirror the comparison.
            let cmp = match cmp {
                FusedCmp::Lt => FusedCmp::Gt,
                FusedCmp::LtEq => FusedCmp::GtEq,
                FusedCmp::Gt => FusedCmp::Lt,
                FusedCmp::GtEq => FusedCmp::LtEq,
                other => other,
            };
            return Some(FusedFilter { col, lit, cmp });
        }
    }
    None
}

/// Total encoded size of the value starting at `pos` (tag included), or
/// None when truncated/unknown.
#[inline]
fn fused_value_size(buf: &[u8], pos: usize) -> Option<usize> {
    let tag = *buf.get(pos)?;
    Some(match tag {
        0x00 | 0x01 | 0x09 => 1,
        0x02 => 2,
        0x03 => 3,
        0x04 => 5,
        0x05 | 0x06 => 9,
        0x07 | 0x08 | 0x0A => {
            // uvarint length (or zigzag payload for 0x0A) after the tag.
            let mut i = pos + 1;
            let mut shift = 0u32;
            let mut len = 0u64;
            while i < buf.len() {
                let b = buf[i];
                i += 1;
                if shift >= 64 {
                    return None;
                }
                len |= ((b & 0x7f) as u64) << shift;
                shift += 7;
                if b & 0x80 == 0 {
                    let total = (i - pos) as u64 + if tag == 0x0A { 0 } else { len };
                    return usize::try_from(total)
                        .ok()
                        .filter(|&t| pos + t <= buf.len());
                }
            }
            return None;
        }
        _ => return None,
    })
}

/// Decode the value at `pos` as a numeric cell (tag included in the size).
#[inline]
fn fused_decode_num(buf: &[u8], pos: usize) -> Option<(NumVal, usize)> {
    let tag = *buf.get(pos)?;
    match tag {
        0x00 => Some((NumVal::Null, 1)),
        0x01 => Some((NumVal::I(0), 1)),
        0x02 => Some((NumVal::I(*buf.get(pos + 1)? as i8 as i64), 2)),
        0x03 => {
            let b = buf.get(pos + 1..pos + 3)?;
            Some((NumVal::I(i16::from_le_bytes([b[0], b[1]]) as i64), 3))
        }
        0x04 => {
            let b = buf.get(pos + 1..pos + 5)?;
            Some((
                NumVal::I(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64),
                5,
            ))
        }
        0x05 => {
            let b = buf.get(pos + 1..pos + 9)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            Some((NumVal::I(i64::from_le_bytes(a)), 9))
        }
        0x06 => {
            let b = buf.get(pos + 1..pos + 9)?;
            let mut a = [0u8; 8];
            a.copy_from_slice(b);
            Some((NumVal::F(f64::from_le_bytes(a)), 9))
        }
        0x0A => {
            let (z, n) = crate::types::value::decode_uvarint(&buf[pos + 1..]).ok()?;
            let i = ((z >> 1) as i64) ^ -((z & 1) as i64);
            Some((NumVal::F(i as f64), 1 + n))
        }
        // Text / Blob / marker / unknown: the fused machine bails.
        _ => None,
    }
}

/// Try the fused machine. Returns Ok(None) when the shape (or a runtime
/// value) is unsupported — the caller then uses the generic paths.
fn try_fused_aggregate(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    alias: Option<&str>,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Result<Option<ExecResult>> {
    if aggregates.is_empty() {
        return Ok(None);
    }
    let prefix = alias.unwrap_or(&table.name);

    // ---- Shape checks -------------------------------------------------
    let filter = match filter_predicate {
        None => None,
        Some(p) => match fused_filter_of(p, table, prefix) {
            Some(f) => Some(f),
            None => return Ok(None),
        },
    };

    let mut specs: Vec<FusedAggSpec> = Vec::with_capacity(aggregates.len());
    // (col, slot) pairs; slots assigned after dedup.
    let mut wanted_cols: Vec<usize> = Vec::new();
    if let Some(f) = &filter {
        wanted_cols.push(f.col);
    }
    for agg in aggregates {
        if agg.distinct {
            return Ok(None);
        }
        let op = match AggFunc::from_name(&agg.func) {
            AggFunc::Count => match &agg.arg {
                None => FusedOp::CountStar,
                Some(_) => FusedOp::CountCol,
            },
            AggFunc::Sum => FusedOp::Sum,
            AggFunc::Total => FusedOp::Total,
            AggFunc::Avg => FusedOp::Avg,
            AggFunc::Min => FusedOp::Min,
            AggFunc::Max => FusedOp::Max,
            _ => return Ok(None),
        };
        let col = match &agg.arg {
            None => None,
            Some(arg) => match fused_resolve_col(arg, table, prefix) {
                Some(c) => Some(c),
                None => return Ok(None),
            },
        };
        if let Some(c) = col {
            if !wanted_cols.contains(&c) {
                wanted_cols.push(c);
            }
        }
        specs.push(FusedAggSpec { op, slot: None });
    }
    if wanted_cols.len() > FUSED_MAX_COLS {
        return Ok(None);
    }
    wanted_cols.sort_unstable();
    wanted_cols.dedup();
    // Assign slots (position of each column in the decode buffer).
    let slot_of = |col: usize| wanted_cols.iter().position(|&c| c == col);
    for (agg, spec) in aggregates.iter().zip(specs.iter_mut()) {
        if let Some(arg) = &agg.arg {
            if let Some(c) = fused_resolve_col(arg, table, prefix) {
                spec.slot = slot_of(c);
            }
        }
    }
    let filter_slot = filter.as_ref().and_then(|f| slot_of(f.col));

    // ---- Typed accumulators -------------------------------------------
    #[derive(Clone)]
    struct FusedAcc {
        count: u64,
        i_sum: i64,
        sum: f64,
        sum_is_int: bool,
        seen: bool,
        min: Option<NumVal>,
        max: Option<NumVal>,
    }
    let mut accs: Vec<FusedAcc> = specs
        .iter()
        .map(|_| FusedAcc {
            count: 0,
            i_sum: 0,
            sum: 0.0,
            sum_is_int: true,
            seen: false,
            min: None,
            max: None,
        })
        .collect();

    // Column -> slot lookup table (usize::MAX = not wanted).
    let n_cols = table.n_columns();
    let mut col_slot = vec![usize::MAX; n_cols];
    for (slot, &col) in wanted_cols.iter().enumerate() {
        col_slot[col] = slot;
    }
    let rowid_alias = table.rowid_alias;

    let mut vals = [NumVal::Null; FUSED_MAX_COLS];
    let mut bailed = false;

    let root = ctx.table_root(table);
    let mut bt = Btree::new(ctx.pager, root, false);
    bt.scan_table_borrowed(|rowid, payload| {
        // ---- inline serial walk: decode ONLY wanted numeric columns ----
        let mut pos = 0usize;
        let mut col = 0usize;
        let mut filled = 0usize; // count of slots written this row
        while pos < payload.len() && col < n_cols {
            let slot = col_slot[col];
            if slot == usize::MAX {
                // Not wanted: skip the whole encoded value.
                match fused_value_size(payload, pos) {
                    Some(sz) => {
                        pos += sz;
                        col += 1;
                        continue;
                    }
                    None => {
                        bailed = true;
                        return false;
                    }
                }
            }
            if Some(col) == rowid_alias {
                // Rowid-alias column: value is the B+tree key.
                vals[slot] = NumVal::I(rowid);
                filled += 1;
                pos += 1; // the 0x09 marker byte (or absent — short rows)
                col += 1;
                continue;
            }
            match fused_decode_num(payload, pos) {
                Some((v, sz)) => {
                    if let NumVal::F(f) = v {
                        if f.is_nan() {
                            // NaN ordering semantics live in Value::Ord —
                            // reproduce them through the generic path.
                            bailed = true;
                            return false;
                        }
                    }
                    vals[slot] = v;
                    filled += 1;
                    pos += sz;
                    col += 1;
                }
                None => {
                    // Text/Blob/truncated: the generic path handles these
                    // (with text coercion semantics) — bail.
                    bailed = true;
                    return false;
                }
            }
        }
        // Short rows (ALTER TABLE ADD COLUMN): remaining wanted cols are NULL.
        let _ = filled;

        // ---- filter ----
        if let (Some(f), Some(fslot)) = (&filter, filter_slot) {
            let v = vals[fslot];
            let pass = match (v, f.lit) {
                (NumVal::I(a), NumVal::I(b)) => match f.cmp {
                    FusedCmp::Eq => a == b,
                    FusedCmp::Neq => a != b,
                    FusedCmp::Lt => a < b,
                    FusedCmp::LtEq => a <= b,
                    FusedCmp::Gt => a > b,
                    FusedCmp::GtEq => a >= b,
                },
                (NumVal::Null, _) | (_, NumVal::Null) => {
                    // NULL filter semantics (SQL three-valued logic →
                    // excluded): numeric compare with 0.0 would be WRONG.
                    // The generic compiled predicate handles NULL; bail.
                    bailed = true;
                    false
                }
                (a, b) => {
                    let (x, y) = (a.as_f64(), b.as_f64());
                    match f.cmp {
                        FusedCmp::Eq => x == y,
                        FusedCmp::Neq => x != y,
                        FusedCmp::Lt => x < y,
                        FusedCmp::LtEq => x <= y,
                        FusedCmp::Gt => x > y,
                        FusedCmp::GtEq => x >= y,
                    }
                }
            };
            if !pass {
                return true; // filtered out; nothing to accumulate
            }
        }

        // ---- typed accumulation (mirrors update_agg_state exactly) ----
        for (i, spec) in specs.iter().enumerate() {
            let v = match spec.slot {
                Some(s) => vals[s],
                None => NumVal::I(1), // COUNT(*) placeholder (non-null)
            };
            let acc = &mut accs[i];
            let non_null = !matches!(v, NumVal::Null);
            if non_null {
                acc.seen = true;
            }
            match spec.op {
                FusedOp::CountStar | FusedOp::CountCol => {
                    if non_null {
                        acc.count += 1;
                    }
                }
                FusedOp::Sum | FusedOp::Total => {
                    if non_null {
                        match v {
                            NumVal::F(f) => {
                                if acc.sum_is_int {
                                    acc.sum = acc.i_sum as f64;
                                }
                                acc.sum_is_int = false;
                                acc.sum += f;
                            }
                            NumVal::I(i) => {
                                if acc.sum_is_int {
                                    acc.i_sum = acc.i_sum.saturating_add(i);
                                } else {
                                    acc.sum += i as f64;
                                }
                            }
                            NumVal::Null => {}
                        }
                    }
                }
                FusedOp::Avg => {
                    if non_null {
                        acc.count += 1;
                        acc.sum += v.as_f64();
                    }
                }
                FusedOp::Min => {
                    if non_null {
                        let replace = match acc.min {
                            None => true,
                            Some(m) => numval_lt(v, m),
                        };
                        if replace {
                            acc.min = Some(v);
                        }
                    }
                }
                FusedOp::Max => {
                    if non_null {
                        let replace = match acc.max {
                            None => true,
                            Some(m) => numval_lt(m, v),
                        };
                        if replace {
                            acc.max = Some(v);
                        }
                    }
                }
            }
        }
        true
    })?;

    if bailed {
        return Ok(None);
    }

    // ---- finalize (mirrors finalize_agg) --------------------------------
    let mut out_row: Vec<Value> = Vec::with_capacity(aggregates.len());
    for (agg, acc) in aggregates.iter().zip(accs.iter()) {
        let v = match AggFunc::from_name(&agg.func) {
            AggFunc::Count => Value::Integer(acc.count as i64),
            AggFunc::Sum => {
                if !acc.seen {
                    Value::Null
                } else if acc.sum_is_int {
                    Value::Integer(acc.i_sum)
                } else {
                    Value::Real(acc.sum)
                }
            }
            AggFunc::Total => Value::Real(if acc.sum_is_int {
                acc.i_sum as f64
            } else {
                acc.sum
            }),
            AggFunc::Avg => {
                if acc.count == 0 {
                    Value::Null
                } else {
                    Value::Real((acc.sum / acc.count as f64 * 1e10).round() / 1e10)
                }
            }
            AggFunc::Min => match acc.min {
                Some(NumVal::I(i)) => Value::Integer(i),
                Some(NumVal::F(f)) => Value::Real(f),
                _ => Value::Null,
            },
            AggFunc::Max => match acc.max {
                Some(NumVal::I(i)) => Value::Integer(i),
                Some(NumVal::F(f)) => Value::Real(f),
                _ => Value::Null,
            },
            _ => Value::Null,
        };
        out_row.push(v);
    }
    let out_cols: Vec<String> = aggregates
        .iter()
        .enumerate()
        .map(|(i, _)| format!("__agg_{}", i))
        .collect();
    Ok(Some(ExecResult {
        columns: out_cols.into(),
        rows: vec![out_row],
    }))
}

/// Numeric ordering for Min/Max accumulation. Cross-type compares as f64
/// (exactly Value::Ord's numeric path; NaN never reaches here).
#[inline]
fn numval_lt(a: NumVal, b: NumVal) -> bool {
    match (a, b) {
        (NumVal::I(x), NumVal::I(y)) => x < y,
        _ => a.as_f64() < b.as_f64(),
    }
}

fn exec_aggregate_no_group_by(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    let n_cols = table.n_columns();
    let prefix = alias.as_deref().unwrap_or(&table.name);

    // Fused typed-aggregate machine first: bare-column numeric aggregates
    // with an optional `col <op> literal` filter. Returns None for any
    // shape it cannot reproduce exactly — then the generic paths below
    // take over (identical results, slower decode/dispatch).
    if let Some(res) =
        try_fused_aggregate(ctx, &table, alias.as_deref(), filter_predicate, aggregates)?
    {
        return Ok(res);
    }

    // Try to resolve each aggregate's arg as a bare Column index.
    // If ALL of them are Columns (or COUNT(*), which has no arg),
    // we can use the index-based fast path. Otherwise, fall back to eval_row.
    let mut agg_col_indices: Vec<Option<usize>> = Vec::with_capacity(aggregates.len());
    let mut all_columns = filter_predicate.is_none();
    for agg in aggregates {
        if let Some(arg) = &agg.arg {
            if let Expr::Column { table: ref_t, name } = arg {
                // Verify the table matches (or is None).
                let matches = ref_t
                    .as_ref()
                    .map(|t| {
                        // SQL scoping: an alias REPLACES the table name —
                        // `t.col` must NOT bind to a `FROM t t2` instance
                        // (otherwise a correlated reference to an outer
                        // un-aliased `t` is silently captured by the inner
                        // alias and compared against itself).
                        if prefix == table.name {
                            t == &table.name || t == prefix
                        } else {
                            t == prefix
                        }
                    })
                    .unwrap_or(true);
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
        Some(
            table
                .columns
                .iter()
                .map(|c| format!("{}.{}", prefix, c.name))
                .collect(),
        )
    };
    let columns_ref = columns.as_ref();

    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();
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
                if decode_row_selective(payload, n_cols, &wanted, rowid, rowid_alias, &mut sel_buf)
                    .is_err()
                {
                    return true;
                }
                if !pred.eval(&sel_buf, &positions, params) {
                    return true;
                }
                saw_any_row = true;
                for i in 0..aggregates.len() {
                    let arg_val: &Value = if agg_pos[i] == usize::MAX {
                        &COUNT_STAR_ARG // COUNT(*)
                    } else if agg_pos[i] < sel_buf.len() {
                        &sel_buf[agg_pos[i]]
                    } else {
                        &Value::Null
                    };
                    update_agg_state(
                        &mut states[i],
                        agg_funcs[i],
                        arg_val,
                        aggregates[i].distinct,
                    );
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
        let wanted_names: Option<Vec<String>> = if filter_predicate.is_some() {
            // The filter eval path needs `col_names` to match row indices.
            // Build the full col_names vec since eval_row expects all cols.
            Some(
                table
                    .columns
                    .iter()
                    .map(|c| format!("{}.{}", prefix, c.name))
                    .collect(),
            )
        } else {
            None
        };

        let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
        let mut full_row_buf: Vec<Value> = Vec::with_capacity(n_cols);

        // Per-aggregate decode positions + COUNT(*) flags, resolved ONCE
        // (the row loop below indexes directly — no per-row position scan).
        let agg_pos: Vec<Option<usize>> = agg_col_indices
            .iter()
            .map(|widx| widx.and_then(|c| wanted.iter().position(|x| *x == c)))
            .collect();
        let agg_is_count_star: Vec<bool> = aggregates.iter().map(|a| a.arg.is_none()).collect();
        // Scratch slot for the rare non-Column arg fallback (eval_row).
        let mut scratch_arg: Vec<Value> = vec![Value::Null; aggregates.len()];

        let root = ctx.table_root(&table);
        let mut bt = Btree::new(ctx.pager, root, false);
        // Use scan_table_borrowed — bypasses Cell::decode's per-row Vec<u8>
        // allocation by passing &[u8] borrows directly into the page buffer.
        // For 10k rows, this saves 10k malloc+free pairs.
        let rowid_alias = table.rowid_alias;
        bt.scan_table_borrowed(|rowid, payload| {
            // Decode only the wanted columns.
            if decode_row_selective(payload, n_cols, &wanted, rowid, rowid_alias, &mut sel_buf)
                .is_err()
            {
                return true;
            }
            // Apply the filter predicate, if any. We need a full row buffer
            // because eval_row indexes by column position. The cheap path
            // is to expand sel_buf back into a full row (NULLs for un-wanted
            // columns). For aggregates only on a few cols, this is still
            // cheaper than decode_row_into because we skip the heavy Text/Blob
            // allocations on un-wanted cols (only Integer/Real decoded).
            if let Some(pred) = filter_predicate {
                let cols = wanted_names
                    .as_ref()
                    .expect("col_names built when filter is present");
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
            for i in 0..aggregates.len() {
                // `agg_pos` / `agg_is_count_star` are precomputed ONCE above
                // (was: a `wanted.iter().position()` linear scan per agg per
                // row — for the 5-aggregate bench shape that was ~20 ns of
                // pure per-row waste, and `sel_buf[pos].clone()` another
                // Value copy per update; update_agg_state only borrows).
                let arg_val: &Value = if agg_is_count_star[i] {
                    &COUNT_STAR_ARG
                } else if let Some(pos) = agg_pos[i] {
                    &sel_buf[pos]
                } else if let Some(arg) = &aggregates[i].arg {
                    let cols = wanted_names
                        .as_ref()
                        .expect("col_names built when not all are Column");
                    // Fall back to eval_row for non-Column args.
                    let mut full_row = vec![Value::Null; n_cols];
                    for (j, &col_idx) in wanted.iter().enumerate() {
                        if j < sel_buf.len() {
                            full_row[col_idx] = sel_buf[j].clone();
                        }
                    }
                    match eval_row(arg, &full_row, cols, &params, &named_params) {
                        Ok(v) => scratch_arg[i] = v,
                        Err(_) => scratch_arg[i] = Value::Null,
                    }
                    &scratch_arg[i]
                } else {
                    &COUNT_STAR_ARG
                };
                update_agg_state(
                    &mut states[i],
                    agg_funcs[i],
                    arg_val,
                    aggregates[i].distinct,
                );
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
fn finish_no_group_by(
    aggregates: &[AggExpr],
    states: Vec<AggState>,
    _saw_any_row: bool,
) -> Result<ExecResult> {
    let mut out_row: Vec<Value> = Vec::with_capacity(aggregates.len());
    for (i, agg) in aggregates.iter().enumerate() {
        out_row.push(finalize_agg(&states[i], &agg.func));
    }
    let out_cols: Vec<String> = aggregates
        .iter()
        .enumerate()
        .map(|(i, _)| format!("__agg_{}", i))
        .collect();
    Ok(ExecResult {
        columns: out_cols.into(),
        rows: vec![out_row],
    })
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
            let matches_t = ref_t
                .as_ref()
                .map(|t| {
                    // SQL scoping: an alias REPLACES the table name —
                    // `t.col` must NOT bind to a `FROM t t2` instance
                    // (otherwise a correlated reference to an outer
                    // un-aliased `t` is silently captured by the inner
                    // alias and compared against itself).
                    if prefix == table.name {
                        t == &table.name || t == prefix
                    } else {
                        t == prefix
                    }
                })
                .unwrap_or(true);
            matches_t && table.find_column(name).is_some()
        }
        Expr::Binary { left, right, .. } => {
            expr_only_columns(left, table, prefix) && expr_only_columns(right, table, prefix)
        }
        Expr::Unary { expr, .. } => expr_only_columns(expr, table, prefix),
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_only_columns(expr, table, prefix)
                && expr_only_columns(low, table, prefix)
                && expr_only_columns(high, table, prefix)
        }
        Expr::IsNull { expr, .. } => expr_only_columns(expr, table, prefix),
        Expr::Is { left, right, .. } => {
            expr_only_columns(left, table, prefix) && expr_only_columns(right, table, prefix)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_only_columns(expr, table, prefix)
                && expr_only_columns(pattern, table, prefix)
                && escape
                    .as_ref()
                    .map(|e| expr_only_columns(e, table, prefix))
                    .unwrap_or(true)
        }
        Expr::Function { args, .. } => args.iter().all(|a| expr_only_columns(a, table, prefix)),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand
                .as_ref()
                .map(|o| expr_only_columns(o, table, prefix))
                .unwrap_or(true)
                && whens.iter().all(|(w, t)| {
                    expr_only_columns(w, table, prefix) && expr_only_columns(t, table, prefix)
                })
                && else_
                    .as_ref()
                    .map(|e| expr_only_columns(e, table, prefix))
                    .unwrap_or(true)
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
            let matches_t = ref_t
                .as_ref()
                .map(|t| {
                    // SQL scoping: an alias REPLACES the table name —
                    // `t.col` must NOT bind to a `FROM t t2` instance
                    // (otherwise a correlated reference to an outer
                    // un-aliased `t` is silently captured by the inner
                    // alias and compared against itself).
                    if prefix == table.name {
                        t == &table.name || t == prefix
                    } else {
                        t == prefix
                    }
                })
                .unwrap_or(true);
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
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_indices(expr, table, prefix, out);
            collect_column_indices(low, table, prefix, out);
            collect_column_indices(high, table, prefix, out);
        }
        Expr::IsNull { expr, .. } => collect_column_indices(expr, table, prefix, out),
        Expr::Is { left, right, .. } => {
            collect_column_indices(left, table, prefix, out);
            collect_column_indices(right, table, prefix, out);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
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
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_column_indices(o, table, prefix, out);
            }
            for (w, t) in whens {
                collect_column_indices(w, table, prefix, out);
                collect_column_indices(t, table, prefix, out);
            }
            if let Some(e) = else_ {
                collect_column_indices(e, table, prefix, out);
            }
        }
        Expr::Row(exprs) => {
            for e in exprs {
                collect_column_indices(e, table, prefix, out);
            }
        }
        _ => {}
    }
}

fn exec_aggregate(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    group_by: &[Expr],
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    // USER AGGREGATES: when any aggregate function name resolves to a
    // registered plugin aggregate, run the generic path. It materializes
    // the input and steps plugin states with full error propagation
    // (the tuned builtin fast paths can't surface plugin errors). The
    // plugin call itself dominates per-row cost, so the missing fusion
    // is not measurable.
    if aggregates
        .iter()
        .any(|a| crate::plugin::lookup_aggregate(&a.func).is_some())
    {
        return exec_plugin_aggregate(ctx, input, group_by, aggregates);
    }
    // Fast path #0: SELECT COUNT(*) FROM t  (no WHERE, no GROUP BY, single COUNT(*)).
    // Uses `Btree::count_rows` which skips decoding every cell payload —
    // just sums `n_cells` across all leaf pages. For a 10k-row table this is
    // ~10x faster than the streaming scan + decode path.
    if group_by.is_empty()
        && aggregates.len() == 1
        && matches!(
            input,
            Plan::Scan {
                predicate: None,
                ..
            }
        )
    {
        if let Plan::Scan { table, .. } = input {
            // Virtual tables have no B+tree to count cells in.
            if table.vtab.is_some() {
                // fall through to the general path (vtab scan + aggregate)
            } else {
                let agg = &aggregates[0];
                // COUNT(*) — arg is None (the planner emits COUNT(*) with no arg).
                // COUNT(col) — arg is Some(Column). We can't use the fast path
                // for COUNT(col) because we need to skip NULLs, which requires
                // decoding.
                if agg.func == "count" && agg.arg.is_none() && !agg.distinct {
                    let root = ctx.table_root(table);
                    let mut bt = Btree::new(ctx.pager, root, false);
                    let n = bt.count_rows()?;
                    let row: Vec<Value> = vec![Value::Integer(n as i64)];
                    return Ok(ExecResult {
                        columns: Arc::from(vec!["__agg_0".to_string()]),
                        rows: vec![row],
                    });
                }
            }
        }
    }
    // Fast path #1: input is a bare Scan.
    // Handles: `SELECT SUM/AVG/MIN/MAX/COUNT(*) FROM t`
    //          `SELECT col, COUNT(*) FROM t GROUP BY col`
    if let Plan::Scan {
        table,
        alias,
        index: None,
        predicate: None,
    } = input
    {
        if table.vtab.is_none() {
            return exec_aggregate_streaming_scan(
                ctx,
                table.clone(),
                alias.clone(),
                None,
                group_by,
                aggregates,
            );
        }
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
        if let Plan::RowidRange {
            table,
            start,
            end,
            residual: None,
            ..
        } = input
        {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx =
                EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
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
            let row: Vec<Value> = vec![Value::Integer(n as i64)];
            return Ok(ExecResult {
                columns: Arc::from(vec!["__agg_0".to_string()]),
                rows: vec![row],
            });
        }
    }
    // Fast path #2: input is Filter(Scan, predicate).
    // Handles: `SELECT COUNT(*) FROM t WHERE val > 5000`
    //          `SELECT col, COUNT(*) FROM t WHERE x > 0 GROUP BY col`
    if let Plan::Filter {
        input: inner,
        predicate,
    } = input
    {
        if let Plan::Scan {
            table,
            alias,
            index: None,
            predicate: None,
        } = inner.as_ref()
        {
            if table.vtab.is_none() {
                return exec_aggregate_streaming_scan(
                    ctx,
                    table.clone(),
                    alias.clone(),
                    Some(predicate),
                    group_by,
                    aggregates,
                );
            }
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
            Plan::IndexRange {
                table,
                index,
                start,
                end,
                residual,
                ..
            } => {
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
                if let Plan::IndexRange {
                    table,
                    index,
                    start,
                    end,
                    residual,
                    ..
                } = inner.as_ref()
                {
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
            // Evaluate the bounds (same logic as exec_index_range —
            // collated index bounds fold through the first column's
            // collation).
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx =
                EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            let fold_bound = |e: &Expr| -> Result<Value> {
                let v = evaluate(e, &eval_ctx)?;
                Ok(match index.columns.first() {
                    Some(ic) => {
                        crate::plugin::collation_fold_key_ref(&ic.collation, &v).into_owned()
                    }
                    None => v,
                })
            };
            let start_key: Option<(Vec<u8>, bool)> = match start {
                Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
                None => None,
            };
            let end_key: Option<(Vec<u8>, bool)> = match end {
                Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
                None => None,
            };
            let scan_start: Vec<u8> = start_key
                .as_ref()
                .map(|(k, _)| k.clone())
                .unwrap_or_default();
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
            let row: Vec<Value> = vec![Value::Integer(n)];
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
    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();
    // Resolve group-by exprs and agg args against the input's column names
    // once, so per-row work is an index read whenever possible.
    let key_col_indices: Vec<Option<usize>> = group_by
        .iter()
        .map(|e| match e {
            Expr::Column { table, name } => {
                resolve_column_index(&inner.columns, table.as_deref(), name)
            }
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
            update_agg_state(
                &mut grouper.groups[gi].1[i],
                agg_funcs[i],
                &arg_val,
                agg.distinct,
            );
        }
    }

    // SQLite semantics: if there is no GROUP BY clause AND no rows were
    // produced by the input, the aggregate still emits ONE row (with
    // COUNT=0, SUM=NULL, AVG=NULL, MIN=NULL, MAX=NULL). This handles the
    // common `SELECT COUNT(*) FROM empty_table` case.
    if group_by.is_empty() && grouper.is_empty() && !aggregates.is_empty() {
        grouper.groups.push((
            Vec::new(),
            (0..n_aggs).map(|_| AggState::default()).collect(),
        ));
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
            Expr::Column {
                table: Some(t),
                name,
            } => format!("{}.{}", t, name),
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

    Ok(ExecResult {
        columns: out_cols.into(),
        rows: out_rows,
    })
}

// ============================================================================
// User (plugin) aggregates
// ============================================================================

/// Generic aggregate execution for statements that use at least one
/// registered plugin aggregate. Mirrors the general (non-fused) builtin
/// path: materialize input rows, group with `HashGrouper`, step per row,
/// finalize per group. Builtins in the same statement keep their normal
/// `AggState` handling so mixed statements (`SELECT count(*), my_agg(x)
/// ...`) work.
fn exec_plugin_aggregate(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    group_by: &[Expr],
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    use crate::plugin::{AggCtx, AggregateFunction};

    let inner = execute(input, ctx)?;
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;

    // Resolve group-by exprs and agg args positionally once.
    let key_col_indices: Vec<Option<usize>> = group_by
        .iter()
        .map(|e| match e {
            Expr::Column { table, name } => {
                resolve_column_index(&inner.columns, table.as_deref(), name)
            }
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

    // Per-aggregate execution slot: builtin state or plugin state.
    enum Slot {
        Builtin(AggState),
        Plugin {
            func: std::sync::Arc<dyn AggregateFunction>,
            state: Option<Box<dyn crate::plugin::AggState>>,
        },
    }

    // Grouper generic over the state type (HashGrouper is hard-wired to
    // AggState). Same SQL-semantic hashing + linear probing.
    struct Grouper {
        buckets: HashMap<u64, Vec<usize>>,
        groups: Vec<(Vec<Value>, Vec<Slot>)>,
    }
    impl Grouper {
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
            let gi = self.groups.len();
            self.groups.push((key.to_vec(), Vec::new()));
            self.buckets.entry(h).or_default().push(gi);
            gi
        }
        fn len(&self) -> usize {
            self.groups.len()
        }
        fn is_empty(&self) -> bool {
            self.groups.is_empty()
        }
    }

    let make_slot = |agg: &AggExpr| -> Result<Slot> {
        if let Some(f) = crate::plugin::lookup_aggregate(&agg.func) {
            // Eager init: an empty group finalizes the fresh state
            // (SQLite calls xFinal even with no xStep).
            Ok(Slot::Plugin {
                func: f.clone(),
                state: Some(f.init()),
            })
        } else {
            Ok(Slot::Builtin(AggState::default()))
        }
    };

    let agg_funcs: Vec<AggFunc> = aggregates
        .iter()
        .map(|a| AggFunc::from_name(&a.func))
        .collect();
    let mut grouper = Grouper {
        buckets: HashMap::new(),
        groups: Vec::new(),
    };
    let mut key_buf: Vec<Value> = Vec::with_capacity(group_by.len());

    for row in &inner.rows {
        key_buf.clear();
        for (ge, kidx) in group_by.iter().zip(key_col_indices.iter()) {
            match kidx {
                Some(idx) => key_buf.push(row[*idx].clone()),
                None => key_buf.push(eval_row(ge, row, &inner.columns, params, named_params)?),
            }
        }
        let gi = grouper.intern(&key_buf);
        if grouper.groups[gi].1.is_empty() {
            grouper.groups[gi].1 = (0..aggregates.len())
                .map(|i| make_slot(&aggregates[i]))
                .collect::<Result<Vec<_>>>()?;
        }
        for (i, agg) in aggregates.iter().enumerate() {
            let arg_val = match (&agg.arg, agg_col_indices[i]) {
                (Some(_), Some(idx)) => row[idx].clone(),
                (Some(arg), None) => eval_row(arg, row, &inner.columns, params, named_params)?,
                (None, _) => Value::Integer(1),
            };
            match &mut grouper.groups[gi].1[i] {
                Slot::Builtin(st) => update_agg_state(st, agg_funcs[i], &arg_val, agg.distinct),
                Slot::Plugin { func, state } => {
                    if !func.arity().accepts(agg.arg.is_some() as usize) {
                        return Err(Error::semantic(format!(
                            "wrong number of arguments to function {}()",
                            agg.func
                        )));
                    }
                    let actx = AggCtx::new(agg.arg.is_some() as usize);
                    if let Some(st) = state.as_mut() {
                        st.step(&actx, &[arg_val])?;
                    }
                }
            }
        }
    }

    // No GROUP BY + empty input → one row of initial states (COUNT=0 etc.).
    if group_by.is_empty() && grouper.is_empty() && !aggregates.is_empty() {
        grouper.groups.push((
            Vec::new(),
            (0..aggregates.len())
                .map(|i| make_slot(&aggregates[i]))
                .collect::<Result<Vec<_>>>()?,
        ));
    }

    let mut out_rows = Vec::with_capacity(grouper.len());
    for (key, states) in grouper.groups {
        let mut row = key;
        for (i, slot) in states.into_iter().enumerate() {
            row.push(match slot {
                Slot::Builtin(st) => finalize_agg(&st, &aggregates[i].func),
                Slot::Plugin { state, .. } => match state {
                    Some(st) => st.value()?,
                    // No rows reached this group's step (e.g. aggregate over
                    // an empty table with GROUP BY): SQLite calls xFinal
                    // with no xStep — the state exists from init().
                    None => Value::Null,
                },
            });
        }
        out_rows.push(row);
    }

    // Column naming identical to the builtin general path so downstream
    // Project/Sort/ORDER BY rewrite logic works unchanged.
    let mut out_cols = Vec::new();
    for (i, g) in group_by.iter().enumerate() {
        let name = match g {
            Expr::Column { table: None, name } => name.clone(),
            Expr::Column {
                table: Some(t),
                name,
            } => format!("{}.{}", t, name),
            _ => format!("col{}", i + 1),
        };
        out_cols.push(name);
    }
    for i in 0..aggregates.len() {
        out_cols.push(format!("__agg_{}", i));
    }

    Ok(ExecResult {
        columns: out_cols.into(),
        rows: out_rows,
    })
}

#[inline]
fn update_agg_state(state: &mut AggState, func: AggFunc, v: &Value, distinct: bool) {
    // Only compute the distinct key if we're actually doing a DISTINCT
    // aggregate. For non-DISTINCT aggregates, this skips per-row hashing
    // entirely. The key is a `SqlValueKey` (a Value clone — free for
    // Integer/Real) instead of the old `format!("{:?}")` String, saving
    // a heap allocation per row for DISTINCT aggregates.
    if distinct && !state.distinct.insert(SqlValueKey(v.clone())) {
        return;
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
                    // Integer→float flip: sync `sum` from `int_sum` ONCE here
                    // (was: a per-row f64 convert+store on the all-integer
                    // hot path, only ever needed at this flip point).
                    if state.sum_is_int {
                        state.sum = state.int_sum as f64;
                    }
                    state.sum_is_int = false;
                    state.sum += v.as_real();
                } else if state.sum_is_int {
                    state.int_sum = state.int_sum.saturating_add(v.as_integer());
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
            if !v.is_null() && (state.min.is_none() || v < state.min.as_ref().unwrap()) {
                state.min = Some(v.clone());
            }
        }
        AggFunc::Max => {
            if !v.is_null() && (state.max.is_none() || v > state.max.as_ref().unwrap()) {
                state.max = Some(v.clone());
            }
        }
        AggFunc::GroupConcat if !v.is_null() => {
            if !state.concat.is_empty() {
                state.concat.push(',');
            }
            state.concat.push_str(&v.as_text());
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
        "total" => Value::Real(if state.sum_is_int {
            state.int_sum as f64
        } else {
            state.sum
        }),
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

fn exec_window(
    ctx: &mut ExecContext<'_>,
    input: &Plan,
    windows: &[WindowExpr],
) -> Result<ExecResult> {
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
            let key: Vec<Value> = w
                .partition_by
                .iter()
                .map(|e| eval_row(e, row, &inner.columns, params, named_params))
                .collect::<Result<_>>()?;
            let key_str = key
                .iter()
                .map(|v| format!("{:?}", v))
                .collect::<Vec<_>>()
                .join("|");
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
                    let va = eval_row(
                        &w.order_by[0].expr,
                        &inner.rows[*a],
                        &inner.columns,
                        params,
                        named_params,
                    )
                    .unwrap_or(Value::Null);
                    let vb = eval_row(
                        &w.order_by[0].expr,
                        &inner.rows[*b],
                        &inner.columns,
                        params,
                        named_params,
                    )
                    .unwrap_or(Value::Null);
                    let ord = va.cmp(&vb);
                    if w.order_by[0].order == Order::Desc {
                        ord.reverse()
                    } else {
                        ord
                    }
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
                let key: Vec<Value> = w
                    .order_by
                    .iter()
                    .map(|t| eval_row(&t.expr, row, &inner.columns, params, named_params))
                    .collect::<Result<_>>()?;
                if prev_key.as_ref() != Some(&key) {
                    rank += count_in_rank + 1;
                    count_in_rank = 0;
                    dense_rank += 1;
                }
                count_in_rank += 1;
                prev_key = Some(key);

                let val = compute_window_value(
                    w,
                    row_num,
                    rank,
                    dense_rank,
                    row,
                    &inner.columns,
                    params,
                    named_params,
                )?;
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
            row.append(&mut extra_cols[i]);
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

fn exec_join(
    ctx: &mut ExecContext<'_>,
    left: &Plan,
    right: &Plan,
    join_type: crate::sql::ast::JoinType,
    condition: &Option<Expr>,
) -> Result<ExecResult> {
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
        if !matched
            && matches!(
                join_type,
                crate::sql::ast::JoinType::Left | crate::sql::ast::JoinType::Full
            )
        {
            let mut combined = left_row.clone();
            combined.extend(vec![Value::Null; n_right]);
            out_rows.push(combined);
        }
    }

    // RIGHT and FULL: emit unmatched right rows with NULL left.
    if matches!(
        join_type,
        crate::sql::ast::JoinType::Right | crate::sql::ast::JoinType::Full
    ) {
        for (ri, right_row) in right_res.rows.iter().enumerate() {
            if !right_matched[ri] {
                let mut combined = vec![Value::Null; n_left];
                combined.extend(right_row.clone());
                out_rows.push(combined);
            }
        }
    }

    Ok(ExecResult {
        columns: combined_cols.into(),
        rows: out_rows,
    })
}

/// Static column list a bare `Plan::Scan` would report — without executing
/// it. Must match `exec_scan` exactly (alias-qualified names when the
/// effective prefix differs from the table name, the cached qualified
/// names otherwise).
fn scan_columns_static(table: &Arc<Table>, alias: &Option<String>) -> std::sync::Arc<[String]> {
    let prefix = alias.as_deref().unwrap_or(&table.name);
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

/// Fused streaming hash join for the OLTP-canonical shape:
/// `INNER JOIN` + single equi-key + both sides bare table scans + all-bare
/// column projection. See `exec_hash_join`'s fused-path comment.
///
/// Returns `Ok(None)` whenever the shape or the data doesn't qualify (the
/// caller falls back to the materialized hash join — never an error, just
/// a different cost profile for the SAME semantics).
fn try_fused_scan_hash_join(
    ctx: &mut ExecContext<'_>,
    left: &Plan,
    right: &Plan,
    condition: &Option<Expr>,
    projection: Option<&[crate::planner::plan::ProjectExpr]>,
) -> Result<Option<ExecResult>> {
    // ---- Shape gates -------------------------------------------------
    let (l_table, l_alias) = match left {
        Plan::Scan {
            table,
            alias,
            index: None,
            predicate: None,
        } if table.vtab.is_none() => (table, alias),
        _ => return Ok(None),
    };
    let (r_table, r_alias) = match right {
        Plan::Scan {
            table,
            alias,
            index: None,
            predicate: None,
        } if table.vtab.is_none() => (table, alias),
        _ => return Ok(None),
    };
    let columns = match projection {
        Some(c) => c,
        None => return Ok(None),
    };
    if columns.is_empty() {
        return Ok(None);
    }
    // Static column lists (identical to what exec_scan reports).
    let l_cols = scan_columns_static(l_table, l_alias);
    let r_cols = scan_columns_static(r_table, r_alias);
    let n_left = l_cols.len();
    // Single pure equi-join key.
    let eq_pairs = extract_equi_join_keys(condition, &l_cols, &r_cols);
    if eq_pairs.len() != 1 {
        return Ok(None);
    }
    let pure_equi = matches!(
        count_eq_leaves_and_purity(condition, &l_cols, &r_cols),
        Some(n) if n == 1
    );
    if !pure_equi {
        return Ok(None);
    }
    let (l_key, r_key) = eq_pairs[0];

    // ---- Projection resolution (bare columns only) --------------------
    // Combined column list as BORROWED strings (left then right — exactly
    // the layout the materialized path's combined list has). One small
    // Vec<&str>, no per-column String clones. Resolution pass ORDER
    // (qualified-exact across both sides BEFORE any suffix fallback) is
    // what makes `b.id` resolve to the b side, not a same-named `a.id`.
    let combined: Vec<&str> = l_cols
        .iter()
        .map(|s| s.as_str())
        .chain(r_cols.iter().map(|s| s.as_str()))
        .collect();
    let mut out_combined: Vec<usize> = Vec::with_capacity(columns.len());
    let mut out_names: Vec<String> = Vec::with_capacity(columns.len());
    for c in columns {
        match &c.expr {
            Expr::Column { table, name } => {
                match resolve_column_index(&combined, table.as_deref(), name) {
                    Some(p) => {
                        out_combined.push(p);
                        out_names.push(match &c.alias {
                            Some(a) => a.clone(),
                            None => expr_display_name(&c.expr),
                        });
                    }
                    None => return Ok(None),
                }
            }
            _ => return Ok(None), // non-bare projection: materialized path
        }
    }

    // ---- Side selection (smaller side builds) --------------------------
    // Per side: table, wanted-column count, local join-key index, live
    // root, row count (a pure n_cells walk — no payload decode).
    struct JoinSide {
        table: Arc<Table>,
        n_cols: usize,
        key_local: usize,
        root: u32,
        count: usize,
    }
    let pager = ctx.pager;
    let side = |t: &Arc<Table>, a: &Option<String>, key: usize| -> Result<JoinSide> {
        let root = ctx.table_root(t);
        let count = Btree::new(pager, root, false).count_rows()? as usize;
        let n_cols = scan_columns_static(t, a).len();
        Ok(JoinSide {
            table: Arc::clone(t),
            n_cols,
            key_local: key,
            root,
            count,
        })
    };
    let l_side = side(l_table, l_alias, l_key)?;
    let r_side = side(r_table, r_alias, r_key)?;
    let (build, probe) = if l_side.count <= r_side.count {
        (l_side, r_side)
    } else {
        (r_side, l_side)
    };

    // ---- Decode plans ---------------------------------------------------
    // Per output column: (is_build_side, position in that side's decode
    // buffer) — packed into ONE struct per column so the emit loop loads
    // a single cache line per output slot.
    struct OutSlot {
        from_build: bool,
        pos: usize,
    }
    let build_is_left = Arc::ptr_eq(&build.table, l_table);
    let mut out_side_build: Vec<bool> = Vec::with_capacity(out_combined.len());
    let mut out_local: Vec<usize> = Vec::with_capacity(out_combined.len());
    for &p in &out_combined {
        let from_left = p < n_left;
        out_side_build.push(from_left == build_is_left);
        out_local.push(if from_left { p } else { p - n_left });
    }
    // Build side wanted columns: key + every build-side projected local.
    // Sorted + deduplicated so `decode_row_selective` takes its
    // allocation-free ascending path (an unsorted list costs a
    // permutation Vec allocation PER ROW).
    let mut build_wanted: Vec<usize> = vec![build.key_local];
    for (i, &is_build) in out_side_build.iter().enumerate() {
        if is_build {
            build_wanted.push(out_local[i]);
        }
    }
    build_wanted.sort_unstable();
    build_wanted.dedup();
    let build_key_pos = build_wanted
        .iter()
        .position(|&c| c == build.key_local)
        .unwrap_or(0);
    // Probe side: same construction.
    let mut probe_wanted: Vec<usize> = vec![probe.key_local];
    for (i, &is_build) in out_side_build.iter().enumerate() {
        if !is_build {
            probe_wanted.push(out_local[i]);
        }
    }
    probe_wanted.sort_unstable();
    probe_wanted.dedup();
    let probe_key_pos = probe_wanted
        .iter()
        .position(|&c| c == probe.key_local)
        .unwrap_or(0);
    // Position of each output column's value inside its side's decode
    // buffer, packed with the side flag for the emit loop.
    let out_slots: Vec<OutSlot> = out_side_build
        .iter()
        .zip(out_local.iter())
        .map(|(&is_build, &local)| {
            if is_build {
                OutSlot {
                    from_build: true,
                    pos: build_wanted.iter().position(|&c| c == local).unwrap_or(0),
                }
            } else {
                OutSlot {
                    from_build: false,
                    pos: probe_wanted.iter().position(|&c| c == local).unwrap_or(0),
                }
            }
        })
        .collect();
    let stride = build_wanted.len();

    // ---- BUILD: stream scan + selective decode + SoA store --------------
    // build_vals: build.count * stride Values (ONE allocation); one
    // u32 ordinal per build row; chain: ordinal -> next ordinal with the
    // same key (u32::MAX = end).
    //
    // The key -> head-ordinal index is a small OPEN-ADDRESSING table with
    // multiplicative (Fibonacci) hashing and linear probing. Join keys are
    // IEEE-754 order keys of small integers — their mantissa lives in the
    // HIGH bits and the low ~43 bits are ZERO, so any hash that relies on
    // low bits (FxHash — measured clustering 1000 keys into ~16 buckets,
    // 142 ns/insert) degrades to linear scans. Taking the TOP log2(cap)
    // bits of key * PHI spreads monotone keys uniformly (~5 ns/op, faster
    // than std's SipHash at ~15 ns/op).
    //
    // The built state is MEMOIZED across statements (see
    // storage/join_cache.rs): key (build root, wanted columns), validated
    // against the pager's write epoch — the same advisory-cache pattern
    // as the B+tree leaf hints. A repeated read-only join skips the
    // build-side scan + decode + hash construction entirely.
    #[inline]
    fn key_slot(k: u64, shift: u32, mask: usize) -> usize {
        ((k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> shift) as usize) & mask
    }
    use crate::storage::join_cache::{JoinBuildState, JoinSlot};
    let join_epoch = pager.write_epoch();
    let cache_key = (build.root, build_wanted.clone());
    let cached: Option<std::sync::Arc<JoinBuildState>> = {
        let jc = pager.join_build_cache().lock();
        jc.get(&cache_key)
            .filter(|st| st.epoch == join_epoch)
            .cloned()
    };

    let built: std::sync::Arc<JoinBuildState>;
    // The probe side's rowid alias is needed by BOTH branches (the probe
    // scan runs after the build/cache decision).
    let probe_alias = probe.table.rowid_alias;
    if let Some(st) = cached {
        built = st;
    } else {
        let n_build = build.count;
        let mut build_vals: Vec<Value> = Vec::with_capacity(n_build.saturating_mul(stride).max(1));
        let mut chain: Vec<u32> = vec![u32::MAX; n_build];
        // Capacity: next power of two >= 2 * n_build (load factor <= 0.5 —
        // probe chains stay ~1.3 slots on average, and an empty slot always
        // exists so the linear probe terminates).
        let table_cap = (n_build.max(1)).next_power_of_two() << 1;
        let table_mask = table_cap.wrapping_sub(1);
        // Top-bits shift for multiplicative hashing (see the build comment).
        let hash_shift = 64 - table_cap.trailing_zeros();
        // One (key, head) pair per slot — a single cache-line load per probe
        // (two parallel arrays would cost two dependent loads). key == u64::MAX
        // marks an empty slot; a real key can never be u64::MAX (order keys of
        // finite doubles / |i| <= 2^53 integers never reach it, and NaN decodes
        // as NULL which skips the build entirely).
        let mut slots: Vec<JoinSlot> = vec![JoinBuildState::empty_slot(); table_cap];
        let mut aborted = false;
        let mut stored: usize = 0;
        let mut buf: Vec<Value> = Vec::new();
        let build_alias = build.table.rowid_alias;
        {
            let mut bt = Btree::new(pager, build.root, false);
            bt.scan_table_borrowed(|rowid, payload| {
                if crate::storage::row_codec::decode_row_selective_sorted(
                    payload,
                    build.n_cols,
                    &build_wanted,
                    rowid,
                    build_alias,
                    &mut buf,
                )
                .is_err()
                {
                    return true; // corrupt row: skip (matches exec_scan)
                }
                let k = match buf.get(build_key_pos) {
                    Some(Value::Integer(i)) => {
                        // Same 2^53 gate as the materialized path's u64_mode:
                        // beyond it, i as f64 rounds and keys could collide.
                        if i.unsigned_abs() > (1u64 << 53) {
                            aborted = true;
                            return false;
                        }
                        crate::types::value::double_order_key(*i as f64)
                    }
                    Some(Value::Real(f)) => crate::types::value::double_order_key(*f),
                    // NULL build keys never match anything (INNER join: skip);
                    // TEXT/BLOB build keys need the byte-key path.
                    Some(Value::Null) => return true,
                    _ => {
                        aborted = true;
                        return false;
                    }
                };
                // count_rows under-counted (corrupt page metadata) or the
                // tree grew mid-scan: fall back to the materialized path
                // rather than indexing past the pre-sized buffers.
                if stored >= n_build {
                    aborted = true;
                    return false;
                }
                let ord = stored as u32;
                stored += 1;
                for v in buf.iter() {
                    build_vals.push(v.clone());
                }
                // Open-addressing insert: find the key's slot (match = bucket
                // chain prepend; empty slot = new head). Load factor <= 0.5
                // (table_cap >= 2 * n_build >= 2 * stored) guarantees an empty
                // slot exists, so the probe always terminates.
                let mut slot = key_slot(k, hash_shift, table_mask);
                loop {
                    let existing = slots[slot].key;
                    if existing == u64::MAX {
                        slots[slot] = JoinSlot { key: k, head: ord };
                        break;
                    }
                    if existing == k {
                        chain[ord as usize] = slots[slot].head;
                        slots[slot].head = ord;
                        break;
                    }
                    slot = (slot + 1) & table_mask;
                }
                true
            })?;
        }
        if aborted {
            return Ok(None);
        }
        // Freshly built: wrap and store in the cross-statement cache.
        debug_assert_eq!(build_vals.len(), stored.saturating_mul(stride));
        built = std::sync::Arc::new(JoinBuildState {
            epoch: join_epoch,
            n_build: stored,
            stride,
            build_vals,
            slots,
            chain,
        });
        crate::storage::join_cache::join_cache_insert(
            &mut pager.join_build_cache().lock(),
            cache_key,
            built.clone(),
        );
    }

    // Shared borrows for the probe loop (the Arc keeps them alive).
    let build_vals: &[Value] = &built.build_vals;
    let slots: &[JoinSlot] = &built.slots;
    let chain: &[u32] = &built.chain;
    let table_cap = slots.len().max(2).next_power_of_two();
    debug_assert_eq!(table_cap, slots.len());
    let table_mask = table_cap.wrapping_sub(1);
    // Top-bits shift for multiplicative hashing (see the build comment).
    let hash_shift = 64 - table_cap.trailing_zeros();

    // ---- PROBE: stream scan + selective decode + emit -------------------
    let n_out = out_combined.len();
    let mut out_rows: Vec<Row> = Vec::with_capacity(probe.count.max(1));
    let mut pbuf: Vec<Value> = Vec::new();
    {
        let mut bt = Btree::new(pager, probe.root, false);
        bt.scan_table_borrowed(|rowid, payload| {
            if crate::storage::row_codec::decode_row_selective_sorted(
                payload,
                probe.n_cols,
                &probe_wanted,
                rowid,
                probe_alias,
                &mut pbuf,
            )
            .is_err()
            {
                return true; // corrupt row: skip
            }
            let k = match pbuf.get(probe_key_pos) {
                Some(Value::Integer(i)) => {
                    // Mirrors the materialized path exactly (no 2^53 gate
                    // on probe keys): identical match semantics either way.
                    crate::types::value::double_order_key(*i as f64)
                }
                Some(Value::Real(f)) => crate::types::value::double_order_key(*f),
                // NULL / TEXT / BLOB probe keys cannot equal the
                // all-numeric build keys: no match, next row.
                _ => return true,
            };
            let mut next = {
                // Open-addressing lookup: probe until an empty slot (no
                // match) or the exact key.
                let mut slot = key_slot(k, hash_shift, table_mask);
                loop {
                    let existing = slots[slot].key;
                    if existing == u64::MAX {
                        break u32::MAX;
                    }
                    if existing == k {
                        break slots[slot].head;
                    }
                    slot = (slot + 1) & table_mask;
                }
            };
            while next != u32::MAX {
                let ord = next as usize;
                let mut out: Row = Vec::with_capacity(n_out);
                for s in &out_slots {
                    let v = if s.from_build {
                        // (ord + 1) rows of `stride` values each are
                        // resident — bounds are structural.
                        &build_vals[ord * stride + s.pos]
                    } else {
                        &pbuf[s.pos]
                    };
                    out.push(v.clone());
                }
                out_rows.push(out);
                next = chain[ord];
            }
            true
        })?;
    }

    let columns_out: std::sync::Arc<[String]> = out_names.into();
    Ok(Some(ExecResult {
        columns: columns_out,
        rows: out_rows,
    }))
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
    // ---- FUSED STREAMING PATH ----
    // INNER equi-join of two bare table scans under a bare-column
    // projection: stream both sides straight off their B+trees with
    // selective decode into reusable buffers — neither side is ever
    // materialized as `Vec<Row>` (which costs one heap allocation per row
    // plus a full-width decode), the build side's projected values live in
    // one SoA allocation, and the key index is an open-addressing
    // Fibonacci-hashed table (~5 ns per op). Returns Ok(None) on any
    // shape or value the fast path doesn't cover; the materialized path
    // below is then 100% in charge (same semantics, different cost
    // profile).
    if std::env::var_os("RSQL_DBG_FUSED").is_some() {
        eprintln!(
            "[dbg] hash-join: fused gate: projection={} join={:?}",
            projection.is_some(),
            join_type
        );
    }
    if projection.is_some()
        && matches!(
            join_type,
            crate::sql::ast::JoinType::Inner | crate::sql::ast::JoinType::Cross
        )
    {
        if let Some(res) = try_fused_scan_hash_join(ctx, left, right, condition, projection)? {
            return Ok(res);
        }
    }
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
            && columns
                .iter()
                .all(|c| matches!(&c.expr, Expr::Column { name, .. } if name != "*"))
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
    let is_inner = matches!(
        join_type,
        crate::sql::ast::JoinType::Inner | crate::sql::ast::JoinType::Cross
    );
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
    let (probe_rows, probe_key_indices, probe_is_left): (&Vec<Row>, Vec<usize>, bool) =
        if build_left {
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
                Some(Value::Real(fv)) => u64_probe_key = crate::types::value::double_order_key(*fv),
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
                if let Some(res) = residual {
                    // Residual predicates need the FULL combined row.
                    let mut combined: Row = Vec::with_capacity(n_left + n_right);
                    if probe_is_left {
                        combined.extend_from_slice(probe_row);
                        combined.extend_from_slice(build_row);
                    } else {
                        combined.extend_from_slice(build_row);
                        combined.extend_from_slice(probe_row);
                    }
                    let v = eval_row(res, &combined, &combined_cols, &params, &named_params)?;
                    if v.is_truthy() {
                        emit_row(
                            &mut out_rows,
                            proj_indices,
                            probe_row,
                            build_row,
                            probe_is_left,
                            n_left,
                            out_width,
                        );
                        matched = true;
                        build_matched[bi] = true;
                    }
                } else {
                    emit_row(
                        &mut out_rows,
                        proj_indices,
                        probe_row,
                        build_row,
                        probe_is_left,
                        n_left,
                        out_width,
                    );
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
            if probe_is_left
                && matches!(
                    join_type,
                    crate::sql::ast::JoinType::Left | crate::sql::ast::JoinType::Full
                )
            {
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
            } else if !probe_is_left
                && matches!(
                    join_type,
                    crate::sql::ast::JoinType::Right | crate::sql::ast::JoinType::Full
                )
            {
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
    if build_left
        && matches!(
            join_type,
            crate::sql::ast::JoinType::Left | crate::sql::ast::JoinType::Full
        )
    {
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
    } else if !build_left
        && matches!(
            join_type,
            crate::sql::ast::JoinType::Right | crate::sql::ast::JoinType::Full
        )
    {
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
        Some((_, names)) => ExecResult {
            columns: names.clone().into(),
            rows: out_rows,
        },
        None => ExecResult {
            columns: combined_cols.into(),
            rows: out_rows,
        },
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
fn count_eq_leaves_and_purity(
    condition: &Option<Expr>,
    left_cols: &[String],
    right_cols: &[String],
) -> Option<usize> {
    fn walk(e: &Expr, lc: &[String], rc: &[String]) -> Option<usize> {
        match e {
            Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => Some(walk(left, lc, rc)? + walk(right, lc, rc)?),
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => {
                let l_in_left = col_index(left, lc).is_some();
                let l_in_right = col_index(left, rc).is_some();
                let r_in_left = col_index(right, lc).is_some();
                let r_in_right = col_index(right, rc).is_some();
                let unambiguous_one_each = ((l_in_left && !l_in_right)
                    && (r_in_right && !r_in_left))
                    || ((l_in_right && !l_in_left) && (r_in_left && !r_in_right));
                if unambiguous_one_each {
                    Some(1)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    condition
        .as_ref()
        .and_then(|c| walk(c, left_cols, right_cols))
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
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            collect_eq_pairs(left, left_cols, right_cols, pairs);
            collect_eq_pairs(right, left_cols, right_cols, pairs);
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            // Try left.col = right.col
            if let (Some(l_idx), Some(r_idx)) =
                (col_index(left, left_cols), col_index(right, right_cols))
            {
                pairs.push((l_idx, r_idx));
                return;
            }
            // Try right.col = left.col
            if let (Some(r_idx), Some(l_idx)) =
                (col_index(left, right_cols), col_index(right, left_cols))
            {
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
                        if prefix.eq_ignore_ascii_case(t) && col_name.eq_ignore_ascii_case(name) {
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
    // NOTE: the combined Vec<String> is NOT built here anymore — the fused
    // projection resolves structurally against the two sides (see
    // `resolve_column_index_two_sides`), and only the non-fused path pays
    // the `to_vec` + per-inner-column `format!`.

    // ---- FUSED PROJECTION resolution ----
    // When this join sits under a Project of bare column references, the
    // join emits ONLY those columns directly: no full-width combined row,
    // no second cloning pass, and text values are cloned once (into the
    // output) instead of twice (combined row, then projection).
    let mut fused: Option<(Vec<usize>, Arc<[String]>)> = None;
    if let Some(columns) = projection {
        if !columns.is_empty()
            && columns
                .iter()
                .all(|c| matches!(&c.expr, Expr::Column { name, .. } if name != "*"))
        {
            let mut indices = Vec::with_capacity(columns.len());
            let mut names = Vec::with_capacity(columns.len());
            let mut all_ok = true;
            for c in columns {
                if let Expr::Column { table, name } = &c.expr {
                    match resolve_column_index_two_sides(
                        &outer_res.columns,
                        inner_prefix,
                        &inner_table.columns,
                        table.as_deref(),
                        name,
                    ) {
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
    // Selective inner decode: when the fused projection only needs some
    // inner columns, decode JUST those (skip the rest — the row codec's
    // selective decoder walks the serial types without allocating Values
    // for un-projected columns). Indices are per-table (0..n_inner_cols).
    let fused_inner_cols: Option<Vec<usize>> = fused.as_ref().and_then(|(indices, _)| {
        let mut inner_idx: Vec<usize> = indices
            .iter()
            .filter_map(|&i| i.checked_sub(outer_width))
            .collect();
        if inner_idx.is_empty() {
            // No inner columns needed at all — an index-only join (rare).
            None
        } else {
            inner_idx.sort_unstable();
            inner_idx.dedup();
            Some(inner_idx)
        }
    });
    let mut key_buf: Vec<u8> = Vec::with_capacity(16);
    // Reused rowid batch + decode buffer across outer rows (one malloc per
    // STATEMENT instead of one per outer row / per matched row).
    let mut rowids: Vec<i64> = Vec::new();
    let mut inner_vals: Row = Vec::new();
    // ONE index + ONE table B+tree handle for the whole join: the pinned
    // root pages and thread-local leaf hints survive across every probe
    // (a fresh `Btree::new` per rowid re-fetched the root page per row).
    let mut index_bt = Btree::new(ctx.pager, inner_index_root, true);
    let mut table_bt = Btree::new(ctx.pager, inner_root, false);

    for outer_row in &outer_res.rows {
        // Extract the join key from the outer row (borrowed — no clone;
        // the order-key encoder only reads it).
        let key_value = match outer_row.get(outer_key_col) {
            Some(v) => v,
            None => continue, // NULL join key — no matches in INNER join.
        };

        // Encode the key for index lookup (order-preserving form, folded
        // through the inner index's collation when it is a collated
        // index), reusing the buffer across rows.
        key_buf.clear();
        match inner_index.columns.first() {
            Some(ic) => crate::plugin::collation_fold_key_ref(&ic.collation, key_value)
                .encode_order_key_into(&mut key_buf),
            None => key_value.encode_order_key_into(&mut key_buf),
        }

        // Look up matching rowids in the index B+tree (reused handle +
        // reused output buffer). A mid-join index split moves the root:
        // re-target the handle when that happens.
        if index_bt.root != inner_index_root {
            index_bt = Btree::new(ctx.pager, inner_index_root, true);
        }
        index_bt.lookup_index_into(&key_buf, &mut rowids)?;
        if index_bt.root != inner_index_root {
            inner_index_root = index_bt.root;
        }

        // Fetch each matching row from the inner table, decoding directly
        // under the page lock (no intermediate payload Vec copy).
        //
        // No dedup set: index entries are (key, rowid) pairs, unique by
        // B+tree construction, so ONE key's lookup yields each rowid at
        // most once. (A corrupted index producing duplicates is
        // `PRAGMA integrity_check`'s to report, not the join's to paper
        // over — and it saves a HashSet allocation per multi-row key.)
        for &rowid in &rowids {
            match &fused_inner_cols {
                Some(wanted) => {
                    // Fused + selective: decode ONLY the needed inner
                    // columns into a small buffer, then MOVE the values
                    // into the output row (a decoded inner row is consumed
                    // by exactly one output row — cloning its Text values
                    // was a pure waste).
                    // Decode into a REUSED buffer (cleared per row) — no
                    // per-row Vec allocation.
                    inner_vals.clear();
                    let found = table_bt.lookup_table_with(rowid, |payload| {
                        decode_row_selective(
                            payload,
                            n_inner_cols,
                            wanted,
                            rowid,
                            inner_table.rowid_alias,
                            &mut inner_vals,
                        )
                    })?;
                    if found.is_some() {
                        let sel = &mut inner_vals;
                        let (indices, _) = fused.as_ref().unwrap();
                        let mut out: Row = Vec::with_capacity(indices.len());
                        for &i in indices {
                            if i < outer_width {
                                // Outer row values are shared across all
                                // matching inner rows — clone (the source
                                // row outlives this iteration).
                                out.push(outer_row[i].clone());
                            } else {
                                // Inner values: find the decoded slot for
                                // this inner column (wanted is sorted+dedup'd).
                                let inner_col = i - outer_width;
                                match wanted.binary_search(&inner_col) {
                                    Ok(slot) => {
                                        // Move out of the decoded row.
                                        out.push(std::mem::replace(&mut sel[slot], Value::Null));
                                    }
                                    Err(_) => out.push(Value::Null),
                                }
                            }
                        }
                        out_rows.push(out);
                    }
                }
                None => {
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
        }
    }

    let out_columns: Arc<[String]> = match &fused {
        Some((_, names)) => names.clone(),
        None => {
            // Non-fused path only: build the combined schema now (the
            // fused path never pays this).
            let mut combined: Vec<String> = outer_res.columns.to_vec();
            combined.extend(
                inner_table
                    .columns
                    .iter()
                    .map(|c| format!("{}.{}", inner_prefix, c.name)),
            );
            combined.into()
        }
    };
    Ok(ExecResult {
        columns: out_columns,
        rows: out_rows,
    })
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

fn exec_union(
    ctx: &mut ExecContext<'_>,
    left: &Plan,
    right: &Plan,
    all: bool,
) -> Result<ExecResult> {
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
    Ok(ExecResult {
        columns,
        rows: seen,
    })
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
    Ok(ExecResult {
        columns,
        rows: seen,
    })
}

// ============================================================================
// RowidLookup
// ============================================================================

fn exec_rowid_lookup(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    rowid_expr: &Expr,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let rowid = evaluate(rowid_expr, &eval_ctx)?.as_integer();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let row = match bt.lookup_table(rowid)? {
        LookupResult::Found(payload) => {
            decode_row(&payload, table.n_columns(), rowid, table.rowid_alias)?
        }
        LookupResult::NotFound => {
            return Ok(ExecResult {
                columns: table.col_names.clone(),
                rows: Vec::new(),
            })
        }
    };
    Ok(ExecResult {
        columns: table.col_names.clone(),
        rows: vec![row],
    })
}

// ============================================================================
// RowidIn (WHERE id IN (?, ?, ...))
// ============================================================================

/// Batched rowid multi-lookup: evaluate the IN-list, sort + dedup the
/// rowids, then seek each row with ONE shared B+tree handle. Rows are
/// emitted in rowid order (ascending), like SQLite's rowid IN seek order.
///
/// Non-integer list members are skipped (SQLite's `id IN (1, 'x')` never
/// matches a row for 'x'); NULLs contribute no rows. If a rowid is absent
/// from the table it is simply skipped — IN semantics need no error.
fn exec_rowid_in(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    values: &[Expr],
    residual: Option<&Expr>,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);

    // Evaluate the list ONCE (not per row — this is the whole point: the
    // old full-scan + Filter path evaluated the IN predicate 10k times).
    let mut rowids: Vec<i64> = Vec::with_capacity(values.len());
    for e in values {
        let v = evaluate(e, &eval_ctx)?;
        // Text/blob/real members can never equal an integer rowid; skip
        // them rather than erroring (SQLite IN semantics: type-mismatched
        // members just don't match).
        if let Value::Integer(i) = v {
            rowids.push(i);
        }
    }
    rowids.sort_unstable();
    rowids.dedup();

    let root = ctx.table_root(&table);
    // ONE B+tree handle for all seeks (the root page stays cached in the
    // pager; each seek then pays only the interior/leaf descent).
    let mut bt = Btree::new(ctx.pager, root, false);
    let n_cols = table.n_columns();
    let alias = table.rowid_alias;
    let mut rows: Vec<Row> = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        if let Some(row) =
            bt.lookup_table_with(rowid, |payload| decode_row(payload, n_cols, rowid, alias))?
        {
            // Residual predicate (e.g. `id IN (...) AND name = 'x'`).
            if let Some(pred) = residual {
                let v = eval_row(pred, &row, &table.col_names, &ctx.params, &ctx.named_params)?;
                if !v.is_truthy() {
                    continue;
                }
            }
            rows.push(row);
        }
    }
    Ok(ExecResult {
        columns: table.col_names.clone(),
        rows,
    })
}

// ============================================================================
// IndexIn (WHERE indexed_col IN (?, ?, ...))
// ============================================================================

/// Batched secondary-index multi-lookup: evaluate each key, seek the index
/// once per key, collect the matching rowids, and fetch the rows in one
/// sorted pass. Rows are emitted in rowid order per seek group (index
/// order), matching SQLite's seek-per-member behavior.
fn exec_index_in(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    index: Arc<crate::schema::Index>,
    key_exprs: &[Expr],
    residual: Option<&Expr>,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);

    let index_root = ctx.index_root(&index);
    let mut index_bt = Btree::new(ctx.pager, index_root, true);
    let table_root = ctx.table_root(&table);
    let mut table_bt = Btree::new(ctx.pager, table_root, false);
    let n_cols = table.n_columns();
    let alias = table.rowid_alias;

    let mut key_buf: Vec<u8> = Vec::with_capacity(16);
    let mut rows: Vec<Row> = Vec::new();

    // Evaluate ALL keys up front and dedup the ORDER-KEY BYTES: distinct
    // keys can't overlap (one row holds one value), but a repeated literal
    // (`k IN (1, 1)`) must seek only once — SQLite dedups IN lists the
    // same way. Deduping on the encoded key is type-exact.
    let mut encoded_keys: Vec<Vec<u8>> = Vec::with_capacity(key_exprs.len());
    for e in key_exprs {
        let v = evaluate(e, &eval_ctx)?;
        // NULL keys never match an index entry (SQLite semantics); other
        // types are encoded order-preservingly and simply miss.
        if v.is_null() {
            continue;
        }
        key_buf.clear();
        // Collated index: fold the probe key the same way stored keys
        // were folded (NOCASE / RTRIM).
        crate::plugin::encode_collated_index_key_into(
            &index.columns,
            std::slice::from_ref(&v),
            &mut key_buf,
        );
        encoded_keys.push(key_buf.clone());
    }
    encoded_keys.sort();
    encoded_keys.dedup();

    for key_buf in &encoded_keys {
        let rowids = index_bt.lookup_index(key_buf)?;
        for rowid in rowids {
            if let Some(row) = table_bt
                .lookup_table_with(rowid, |payload| decode_row(payload, n_cols, rowid, alias))?
            {
                if let Some(pred) = residual {
                    let pv =
                        eval_row(pred, &row, &table.col_names, &ctx.params, &ctx.named_params)?;
                    if !pv.is_truthy() {
                        continue;
                    }
                }
                rows.push(row);
            }
        }
    }
    Ok(ExecResult {
        columns: table.col_names.clone(),
        rows,
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
        rows.retain(
            |row| match eval_row(res, row, &columns, params, named_params) {
                Ok(v) => v.is_truthy(),
                Err(_) => false,
            },
        );
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
fn bare_column_projection(
    columns: &[ProjectExpr],
    table: &Table,
) -> Option<crate::types::ProjectionMapping> {
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
                if decode_row_selective(payload, n_cols, idxs, rowid, rowid_alias, &mut row).is_ok()
                {
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
    Ok(ExecResult {
        columns: out_cols,
        rows,
    })
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
            Ok(ExecResult {
                columns: out_cols,
                rows: vec![row],
            })
        }
        LookupResult::NotFound => Ok(ExecResult {
            columns: out_cols,
            rows: Vec::new(),
        }),
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
    exec_index_lookup_impl(
        ctx,
        table.clone(),
        index,
        key_exprs,
        None,
        table.col_names.clone(),
    )
}

/// IndexLookup with the projection FUSED into the fetch: decodes ONLY the
/// projected columns per row (selective decode), skips the payload Vec
/// copy (`lookup_table_with`), and reuses ONE table B+tree handle across
/// all rowids so the pinned root + leaf hint survive between fetches.
/// `SELECT c FROM t WHERE indexed_col = ?` previously paid a fresh
/// `Btree::new` + root cache-lock + payload malloc per row.
fn exec_index_lookup_projected(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    index: Arc<crate::schema::Index>,
    key_exprs: &[Expr],
    projection: Option<&[crate::planner::plan::ProjectExpr]>,
) -> Result<ExecResult> {
    if std::env::var_os("RSQL_DBG_IDXL").is_some() {
        eprintln!("[dbg] fused IndexLookup path taken");
    }
    let (project, out_cols) =
        match projection.and_then(|columns| bare_column_projection(columns, &table)) {
            Some((p, n)) => (p, n),
            None => (None, table.col_names.clone()),
        };
    exec_index_lookup_impl(ctx, table, index, key_exprs, project.as_deref(), out_cols)
}

fn exec_index_lookup_impl(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    index: Arc<crate::schema::Index>,
    key_exprs: &[Expr],
    project: Option<&[usize]>,
    out_cols: Arc<[String]>,
) -> Result<ExecResult> {
    // Evaluate the key expressions to get the lookup values.
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let key_values: Vec<Value> = key_exprs
        .iter()
        .map(|e| evaluate(e, &eval_ctx))
        .collect::<Result<_>>()?;

    // Encode the key: concatenate the order-preserving encoded form of each
    // indexed column value (must match encode_index_key's encoding —
    // collated index columns fold their TEXT probe keys the same way).
    let mut key_bytes = Vec::new();
    crate::plugin::encode_collated_index_key_into(&index.columns, &key_values, &mut key_bytes);

    // Look up matching rowids in the index (override-aware root), into a
    // reusable buffer (no fresh Vec malloc per query).
    let index_root = ctx.index_root(&index);
    let mut index_bt = Btree::new(ctx.pager, index_root, true);
    let mut rowids: Vec<i64> = Vec::new();
    index_bt.lookup_index_into(&key_bytes, &mut rowids)?;

    // Fetch each row by rowid from the table B+tree. Use the
    // override-aware root: the catalog's Arc<Table> holds the root from
    // CREATE TABLE time, but splits may have moved the actual root
    // (tracked in ctx.root_overrides). Using the stale root made rows
    // beyond the first subtree invisible to indexed lookups after the
    // table had grown.
    //
    // ONE table B+tree handle for the whole batch: the pinned root and
    // the thread-local leaf hint persist across fetches (a fresh
    // `Btree::new` per rowid re-fetched the root page on EVERY row).
    let table_root = ctx.table_root(&table);
    let mut table_bt = Btree::new(ctx.pager, table_root, false);
    let n_cols = table.n_columns();
    let rowid_alias = table.rowid_alias;
    let mut rows: Vec<Row> = Vec::with_capacity(rowids.len());
    fetch_rows_by_rowids(
        &mut table_bt,
        &rowids,
        n_cols,
        rowid_alias,
        project,
        &mut rows,
    )?;

    Ok(ExecResult {
        columns: out_cols,
        rows,
    })
}

/// Fetch table rows for a batch of rowids into `rows`, decoding each row
/// directly under the page lock (no intermediate payload Vec copy). With
/// `project`, decodes ONLY the projected columns (selective decode);
/// decoded values are MOVED into the output rows (no clone pass).
/// The caller's B+tree handle is reused across all fetches so its pinned
/// root page and leaf-hint warmth carry over.
fn fetch_rows_by_rowids(
    table_bt: &mut Btree<'_>,
    rowids: &[i64],
    n_cols: usize,
    rowid_alias: Option<usize>,
    project: Option<&[usize]>,
    rows: &mut Vec<Row>,
) -> Result<()> {
    for &rowid in rowids {
        match project {
            Some(idxs) => {
                let mut row: Row = Vec::with_capacity(idxs.len());
                let found = table_bt.lookup_table_with(rowid, |payload| {
                    decode_row_selective(payload, n_cols, idxs, rowid, rowid_alias, &mut row)
                })?;
                if found.is_some() {
                    rows.push(row);
                }
            }
            None => {
                let found = table_bt.lookup_table_with(rowid, |payload| {
                    decode_row(payload, n_cols, rowid, rowid_alias)
                })?;
                if let Some(row) = found {
                    rows.push(row);
                }
            }
        }
    }
    Ok(())
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
    // Collated index: the bounds fold through the first index column's
    // collation so the scan bounds match the folded stored keys.
    let fold_bound = |e: &Expr| -> Result<Value> {
        let v = evaluate(e, &eval_ctx)?;
        Ok(match index.columns.first() {
            Some(ic) => crate::plugin::collation_fold_key_ref(&ic.collation, &v).into_owned(),
            None => v,
        })
    };
    let start_key: Option<(Vec<u8>, bool)> = match start {
        Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
        None => None,
    };
    let end_key: Option<(Vec<u8>, bool)> = match end {
        Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
        None => None,
    };

    // Scan the index from the start bound.
    let scan_start: Vec<u8> = start_key
        .as_ref()
        .map(|(k, _)| k.clone())
        .unwrap_or_default();
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
    let use_merge_scan =
        max_rowid_hint > 0 && (rowids.len() as i64) * 4 > max_rowid_hint && residual.is_none(); // residual needs full rows in index order? No —
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
        let mut position: std::collections::HashMap<
            i64,
            usize,
            crate::storage::pager::PageIdHashBuild,
        > = std::collections::HashMap::with_capacity_and_hasher(
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
                    Some(pred) => {
                        match eval_row(pred, &row, &plain_names, &params, &named_params) {
                            Ok(v) => v.is_truthy(),
                            Err(_) => false,
                        }
                    }
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
        for row in placed.into_iter().flatten() {
            rows.push(row);
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
    /// Collated index columns (NOCASE / RTRIM) fold their TEXT values so
    /// the stored key matches probe keys.
    #[inline]
    pub fn encode_key(&mut self, row: &[Value]) -> &[u8] {
        self.key_buf.clear();
        for (i, &pos) in self.cols.iter().enumerate() {
            if let Some(v) = row.get(pos) {
                if let Some(ic) = self.idx.columns.get(i) {
                    crate::plugin::collation_fold_key(&ic.collation, v)
                        .encode_order_key_into(&mut self.key_buf);
                } else {
                    v.encode_order_key_into(&mut self.key_buf);
                }
            }
        }
        &self.key_buf
    }
}

/// Sentinel column index in INSERT target lists meaning "the rowid
/// pseudo-column" (`INSERT INTO t (rowid, ...)` on a table without an
/// INTEGER PRIMARY KEY alias). Never a valid real column index.
pub(crate) const ROWID_COLUMN_SENTINEL: usize = usize::MAX;

/// Extract an explicit rowid from a supplied value: integers pass through,
/// integral REALs convert exactly (2^63 is representable), NULL means
/// auto-assign, anything else is an error.
fn rowid_from_value(v: &Value) -> Result<Option<i64>> {
    match v {
        Value::Integer(r) => Ok(Some(*r)),
        Value::Null => Ok(None),
        Value::Real(f) if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 => {
            Ok(Some(*f as i64))
        }
        Value::Real(f) if *f == 9_223_372_036_854_775_808.0 => Ok(Some(i64::MIN)), // 2^63 wraps to MIN in SQLite's conversion
        _ => Err(Error::semantic("rowid must be an integer")),
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

fn exec_insert(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    source: &Plan,
    columns: Option<Vec<usize>>,
    on_conflict: ConflictResolution,
    upsert: Option<&crate::sql::ast::UpsertClause>,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
) -> Result<ExecResult> {
    // Virtual table: xUpdate with old_rowid = None. The source rows are
    // evaluated normally, then handed to the module.
    if table.vtab.is_some() {
        if upsert.is_some() {
            return Err(Error::Unsupported("ON CONFLICT on a virtual table"));
        }
        let source_res = execute(source, ctx)?;
        // RETURNING needs the inserted rows AFTER xUpdate (clone only then).
        let returning_snapshot = if returning.is_some() {
            source_res.rows.clone()
        } else {
            Vec::new()
        };
        vtab_exec::exec_insert_vtab(ctx, &table, source_res.rows, columns.as_ref(), on_conflict)?;
        if let Some(ret) = returning {
            let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
            let mut rows = Vec::with_capacity(returning_snapshot.len());
            for row in &returning_snapshot {
                rows.push(project_returning_row(
                    ret,
                    row,
                    &col_names,
                    &ctx.params,
                    &ctx.named_params,
                )?);
            }
            let names = returning_column_names(ret, &col_names);
            return Ok(ExecResult {
                columns: names.into(),
                rows,
            });
        }
        return Ok(ExecResult {
            columns: Arc::from(Vec::new()),
            rows: Vec::new(),
        });
    }
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
            let eval_ctx =
                EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            let mut explicit_rowid: Option<i64> = None;
            for (i, expr) in exprs.iter().enumerate() {
                if i < target_indices.len() {
                    let col_idx = target_indices[i];
                    let val = evaluate(expr, &eval_ctx)?;
                    if col_idx == ROWID_COLUMN_SENTINEL {
                        explicit_rowid = rowid_from_value(&val)?;
                        continue;
                    }
                    let affinity = table.columns[col_idx].affinity;
                    full_row[col_idx] = affinity.coerce(val);
                }
            }
            // Apply column defaults.
            for (i, col) in table.columns.iter().enumerate() {
                if full_row[i].is_null() && col.default.is_some() {
                    if let Some(default_expr) = &col.default {
                        let eval_ctx = EvalContext::new(
                            &empty_row,
                            &empty_cols,
                            &ctx.params,
                            &ctx.named_params,
                        );
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
                    let r = next_auto_rowid(ctx.pager, current_root, max_rowid)?;
                    if r > max_rowid {
                        max_rowid = r;
                    }
                    full_row[idx] = Value::Integer(r);
                    rowid_autogen = true;
                }
            }

            // NOT NULL + CHECK constraints. Always enforced: the NOT NULL
            // loop is purely positional (no names needed); col_names is
            // only consulted for CHECK expressions (empty when absent).
            enforce_row_constraints(
                &table,
                &full_row,
                &col_names,
                &ctx.params,
                &ctx.named_params,
            )?;
            // FOREIGN KEY (child side) — enforced only when the pragma is on.
            enforce_child_fks(ctx, &table, &full_row)?;

            // BEFORE INSERT triggers (NEW = the row about to be inserted,
            // rowid assigned + constraints enforced). An error aborts the
            // statement before the row is written.
            if crate::executor::triggers::has_triggers_for(
                ctx,
                &table,
                &crate::sql::ast::TriggerEvent::Insert,
            ) {
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
                ctx,
                &table,
                &table_name_lc,
                &mut current_root,
                &mut max_rowid,
                &mut full_row,
                &mut payload_buf,
                &mut index_states,
                on_conflict,
                upsert,
                rowid_autogen,
                explicit_rowid,
            )?;
            let trigger_fired = matches!(
                outcome,
                InsertOutcome::Inserted | InsertOutcome::UpdatedExisting
            );
            match outcome {
                InsertOutcome::Inserted => {
                    inserted += 1;
                    if let Some(ret) = returning {
                        returning_rows.push(project_returning_row(
                            ret,
                            &full_row,
                            &col_names,
                            &ctx.params,
                            &ctx.named_params,
                        )?);
                    }
                }
                InsertOutcome::UpdatedExisting => {
                    inserted += 1;
                    if let Some(ret) = returning {
                        returning_rows.push(project_returning_row(
                            ret,
                            &full_row,
                            &col_names,
                            &ctx.params,
                            &ctx.named_params,
                        )?);
                    }
                }
                InsertOutcome::Skipped => {}
            }
            // AFTER INSERT triggers (skip the catalog lookup entirely when
            // the table has none).
            if trigger_fired
                && crate::executor::triggers::has_triggers_for(
                    ctx,
                    &table,
                    &crate::sql::ast::TriggerEvent::Insert,
                )
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
        return Ok(finish_insert_result(
            inserted,
            returning,
            &col_names,
            returning_rows,
        ));
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
        let mut explicit_rowid: Option<i64> = None;
        for (i, val) in row.iter().enumerate() {
            if i < target_indices.len() {
                let col_idx = target_indices[i];
                if col_idx == ROWID_COLUMN_SENTINEL {
                    explicit_rowid = match val {
                        Value::Integer(r) => Some(*r),
                        Value::Null => None,
                        _ => return Err(Error::semantic("rowid must be an integer")),
                    };
                    continue;
                }
                let affinity = table.columns[col_idx].affinity;
                full_row[col_idx] = affinity.coerce(val.clone());
            }
        }

        // Apply column defaults.
        for (i, col) in table.columns.iter().enumerate() {
            if full_row[i].is_null() && col.default.is_some() {
                let eval_ctx =
                    EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
                if let Some(default_expr) = &col.default {
                    full_row[i] = evaluate(default_expr, &eval_ctx)?;
                }
            }
        }

        // Assign the rowid BEFORE constraint enforcement (see fast path).
        let mut rowid_autogen = false;
        if let Some(idx) = table.rowid_alias {
            if full_row[idx].is_null() {
                let r = next_auto_rowid(ctx.pager, current_root, max_rowid)?;
                if r > max_rowid {
                    max_rowid = r;
                }
                full_row[idx] = Value::Integer(r);
                rowid_autogen = true;
            }
        }

        // NOT NULL + CHECK constraints (see fast path).
        enforce_row_constraints(
            &table,
            &full_row,
            &col_names,
            &ctx.params,
            &ctx.named_params,
        )?;
        enforce_child_fks(ctx, &table, &full_row)?;

        let outcome = exec_insert_one_row(
            ctx,
            &table,
            &table_name_lc,
            &mut current_root,
            &mut max_rowid,
            &mut full_row,
            &mut payload_buf,
            &mut index_states,
            on_conflict,
            upsert,
            rowid_autogen,
            explicit_rowid,
        )?;
        match outcome {
            InsertOutcome::Inserted | InsertOutcome::UpdatedExisting => {
                inserted += 1;
                if let Some(ret) = returning {
                    returning_rows.push(project_returning_row(
                        ret,
                        &full_row,
                        &col_names,
                        &ctx.params,
                        &ctx.named_params,
                    )?);
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
    Ok(finish_insert_result(
        inserted,
        returning,
        &col_names,
        returning_rows,
    ))
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
///
/// `col_indices` empty = all columns in declared order.
pub fn fast_insert_literal_rows(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    col_indices: &[usize],
    rows: Vec<Vec<Value>>,
) -> Result<(i64, u32, i64)> {
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
        let mut explicit_rowid: Option<i64> = None;
        if col_indices.is_empty() {
            // All columns in declared order.
            if row.len() != n_cols {
                return Err(Error::semantic(format!(
                    "table {} has {} columns but {} values were supplied",
                    table.name,
                    n_cols,
                    row.len()
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
                if col_idx == ROWID_COLUMN_SENTINEL {
                    explicit_rowid = rowid_from_value(&v)?;
                    continue;
                }
                full_row[col_idx] = table.columns[col_idx].affinity.coerce(v);
            }
        }

        // Rowid pre-assignment so a NULL rowid-alias doesn't trip NOT NULL
        // (mirrors exec_insert).
        let mut rowid_autogen = false;
        if let Some(idx) = table.rowid_alias {
            if full_row[idx].is_null() {
                let r = next_auto_rowid(ctx.pager, current_root, max_rowid)?;
                if r > max_rowid {
                    max_rowid = r;
                }
                full_row[idx] = Value::Integer(r);
                rowid_autogen = true;
            }
        }

        enforce_row_constraints(table, &full_row, &col_names, &ctx.params, &ctx.named_params)?;
        enforce_child_fks(ctx, table, &full_row)?;

        // BEFORE INSERT triggers.
        if crate::executor::triggers::has_triggers_for(
            ctx,
            table,
            &crate::sql::ast::TriggerEvent::Insert,
        ) {
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
            explicit_rowid,
        )?;
        let ok = matches!(
            outcome,
            InsertOutcome::Inserted | InsertOutcome::UpdatedExisting
        );
        if ok {
            inserted += 1;
        }
        // AFTER INSERT triggers.
        if ok
            && crate::executor::triggers::has_triggers_for(
                ctx,
                table,
                &crate::sql::ast::TriggerEvent::Insert,
            )
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
    // (rows inserted, final live root, final max rowid) — the caller's
    // INSERT-chain setup consumes the live root / max-rowid so the next
    // same-shape statement can skip the derivation entirely.
    Ok((inserted, current_root, max_rowid))
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
            let r = next_auto_rowid(ctx.pager, current_root, max_rowid)?;
            if r > max_rowid {
                max_rowid = r;
            }
            full_row[idx] = Value::Integer(r);
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
        None,
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
    index_states: &mut [IndexMaintState],
    on_conflict: ConflictResolution,
    upsert: Option<&crate::sql::ast::UpsertClause>,
    rowid_autogen_hint: bool,
    explicit_rowid: Option<i64>,
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
    let rowid = if let Some(r) = explicit_rowid {
        // `INSERT INTO t (rowid, ...)` on a table without an INTEGER
        // PRIMARY KEY alias: the rowid is supplied positionally.
        rowid_was_autogenerated = false;
        if r > *max_rowid {
            *max_rowid = r;
        }
        r
    } else if let Some(idx) = table.rowid_alias {
        match &full_row[idx] {
            Value::Integer(i) => {
                rowid_was_autogenerated = rowid_autogen_hint;
                *i
            }
            Value::Null => {
                let r = next_auto_rowid(ctx.pager, *current_root, *max_rowid)?;
                if r > *max_rowid {
                    *max_rowid = r;
                }
                full_row[idx] = Value::Integer(r);
                rowid_was_autogenerated = true;
                r
            }
            _ => {
                return Err(Error::semantic(
                    "rowid alias column must be an integer or NULL",
                ))
            }
        }
    } else {
        let r = next_auto_rowid(ctx.pager, *current_root, *max_rowid)?;
        if r > *max_rowid {
            *max_rowid = r;
        }
        rowid_was_autogenerated = true;
        r
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
                        st.idx
                            .columns
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&t.name))
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
    let mut conflict_cols: Vec<String> = Vec::new();
    let mut conflict_on_target = false;
    for (i, st) in index_states.iter_mut().enumerate() {
        if !st.idx.unique {
            continue;
        }
        // NULLs are distinct in UNIQUE indexes (SQLite semantics): a row
        // with ANY NULL among the indexed columns is exempt from the
        // uniqueness check, so multiple NULL rows coexist.
        if st.idx.columns.iter().any(|c| {
            table
                .find_column(&c.name)
                .map(|p| full_row.get(p).map(|v| v.is_null()).unwrap_or(false))
                .unwrap_or(false)
        }) {
            continue;
        }
        // Any REPLACE/UPSERT path below mutates the index trees, so the
        // append hint may go stale — drop it (it re-pins on the next
        // plain insert).
        st.hint = None;
        let key_bytes = st.encode_key(full_row).to_vec();
        let idx_root = st.root;
        let mut ibt = Btree::new(ctx.pager, idx_root, true);
        let matches = ibt.lookup_index(&key_bytes)?;
        if !matches.is_empty() {
            conflict_rowid = Some(matches[0]);
            conflict_cols = st
                .idx
                .columns
                .iter()
                .map(|c| format!("{}.{}", table.name, c.name))
                .collect();
            conflict_on_target =
                matches!(upsert_target, UpsertTarget::Any | UpsertTarget::Index(_))
                    && (matches!(upsert_target, UpsertTarget::Any) || {
                        if let UpsertTarget::Index(t) = &upsert_target {
                            *t == i
                        } else {
                            false
                        }
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
                    ctx,
                    table,
                    table_name_lc,
                    current_root,
                    full_row,
                    payload_buf,
                    index_states,
                    existing_rowid,
                    u,
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
                    if let Ok(old_row) = decode_row(
                        &old_payload,
                        table.n_columns(),
                        existing_rowid,
                        table.rowid_alias,
                    ) {
                        for st in index_states.iter_mut() {
                            let old_key = encode_index_key(&st.idx, table, &old_row);
                            let mut ibt = Btree::new(ctx.pager, st.root, true);
                            ibt.delete_index(&old_key, existing_rowid)?;
                            st.root = ibt.root;
                        }
                    }
                }
            }
            _ => {
                // SQLite's message shape: "UNIQUE constraint failed: t.col"
                // (comma-joined for composite keys).
                let cols = if conflict_cols.is_empty() {
                    table.name.clone()
                } else {
                    conflict_cols.join(", ")
                };
                return Err(Error::constraint(format!(
                    "UNIQUE constraint failed: {}",
                    cols
                )));
            }
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
                                ctx,
                                table,
                                table_name_lc,
                                current_root,
                                full_row,
                                payload_buf,
                                index_states,
                                rowid,
                                u,
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
                        _ => {
                            let pk = table
                                .rowid_alias
                                .and_then(|i| table.columns.get(i).map(|c| c.name.clone()))
                                .unwrap_or_else(|| "rowid".to_string());
                            return Err(Error::constraint(format!(
                                "UNIQUE constraint failed: {}.{}",
                                table.name, pk
                            )));
                        }
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
            if let Ok(old_row) =
                decode_row(&old_payload, table.n_columns(), rowid, table.rowid_alias)
            {
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
    index_states: &mut [IndexMaintState],
    existing_rowid: i64,
    upsert: &crate::sql::ast::UpsertClause,
) -> Result<InsertOutcome> {
    match &upsert.action {
        crate::sql::ast::UpsertAction::DoNothing => Ok(InsertOutcome::Skipped),
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
            let old_row = match decode_row(&old_payload, n_cols, existing_rowid, table.rowid_alias)
            {
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
            enforce_row_constraints(
                table,
                &new_row,
                &plain_names,
                &ctx.params,
                &ctx.named_params,
            )?;

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
                let did_in_place = bt
                    .update_table(existing_rowid, payload_buf)
                    .unwrap_or(false);
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
pub(crate) fn encode_index_key(
    index: &crate::schema::Index,
    table: &Table,
    row: &[Value],
) -> Vec<u8> {
    let mut key_bytes = Vec::new();
    for col in &index.columns {
        if let Some(pos) = table.find_column(&col.name) {
            if let Some(v) = row.get(pos) {
                // Collated index: fold TEXT keys through the column's
                // collation so probe keys and stored keys agree.
                crate::plugin::collation_fold_key(&col.collation, v)
                    .encode_order_key_into(&mut key_bytes);
            }
        }
    }
    key_bytes
}

// ---------------------------------------------------------------------------
// UPDATE write-set unique-index simulation (SQLite sequential semantics)
// ---------------------------------------------------------------------------

/// Outcome of the UPDATE unique-index write-set simulation.
#[derive(Default)]
pub(crate) struct UpdateConflictPlan {
    /// Write-set indices (positions) to SKIP: rows that would violate a
    /// unique index under OR IGNORE, or rows deleted by OR REPLACE.
    pub skip: std::collections::HashSet<usize>,
    /// Rowids to DELETE before applying the write set (OR REPLACE
    /// conflicting holders — baseline rows or write-set rows).
    pub delete_rowids: Vec<i64>,
}

/// True when any indexed column of `row` is NULL (UNIQUE-exempt).
fn index_key_has_null(index: &crate::schema::Index, table: &Table, row: &[Value]) -> bool {
    index.columns.iter().any(|c| {
        table
            .find_column(&c.name)
            .map(|p| row.get(p).map(|v| v.is_null()).unwrap_or(false))
            .unwrap_or(false)
    })
}

/// Simulate SQLite's per-row sequential unique-index checking for an
/// UPDATE write set, BEFORE any row is applied, so the statement aborts
/// atomically (SQLite's ABORT rolls back the whole statement).
///
/// SQLite applies UPDATE rows one at a time (in scan order), deleting
/// each row's old index entries and inserting the new ones, checking
/// uniqueness at insert time. Row K's new key therefore conflicts iff it
/// is held at that moment by:
///   (a) a row outside the write set (never touched),
///   (b) a LATER write-set row's old key (not yet vacated), or
///   (c) an EARLIER write-set row's new key (already claimed).
/// Because nothing has been applied yet, the pristine index B+tree shows
/// every write-set row's OLD key; the simulation subtracts vacated keys
/// and adds claimed keys as it walks the write set in order.
///
/// Handles the UPDATE conflict algorithms:
///  - ABORT / FAIL / ROLLBACK (default): `Err` with the SQLite-exact
///    "UNIQUE constraint failed: t.c" message.
///  - OR IGNORE: the conflicting row's update is skipped (row keeps its
///    old values; changes() does not count it).
///  - OR REPLACE: the conflicting HOLDER row is deleted (table + all
///    its index entries) before the write set applies.
///
/// `write_set` entries are `(rowid, old_row, new_row)` in scan order.
/// Returns `Ok(plan)` with no conflicts when `unique_indexes` is empty.
pub(crate) fn simulate_update_unique(
    ctx: &ExecContext<'_>,
    table: &Table,
    unique_indexes: &[Arc<crate::schema::Index>],
    write_set: &[(i64, &[Value], &[Value])],
    or_conflict: ConflictResolution,
) -> Result<UpdateConflictPlan> {
    let mut plan = UpdateConflictPlan::default();
    if unique_indexes.is_empty() || write_set.is_empty() {
        return Ok(plan);
    }
    // rowid -> write-set position (classifies probe matches).
    let pos_of: std::collections::HashMap<i64, usize> = write_set
        .iter()
        .enumerate()
        .map(|(i, r)| (r.0, i))
        .collect();
    // Rows already condemned by OR REPLACE in this statement — their
    // entries are (virtually) gone for every index.
    let mut deleted: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for idx in unique_indexes {
        // Per-row (old_key, new_key) pairs, encoded once. NULL rows carry
        // keys too (they have index entries) but are exempt from CHECKING.
        let mut keys: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(write_set.len());
        for (_, old_row, new_row) in write_set {
            keys.push((
                encode_index_key(idx, table, old_row),
                encode_index_key(idx, table, new_row),
            ));
        }
        let root = ctx.index_root(idx);
        let mut ibt = Btree::new(ctx.pager, root, true);
        // New keys claimed by PROCESSED write-set rows (their old entries
        // are already vacated; their new entries are live).
        let mut pending_new: std::collections::HashMap<Vec<u8>, i64> =
            std::collections::HashMap::with_capacity(write_set.len());
        let mut probe: Vec<i64> = Vec::new();
        for pos in 0..write_set.len() {
            let rowid = write_set[pos].0;
            let (ref old_key, ref new_key) = keys[pos];
            if old_key == new_key {
                // Index entry unchanged — the entry stays; claim it so
                // LATER rows probing this key see it as taken.
                pending_new.insert(new_key.clone(), rowid);
                continue;
            }
            let null_new = index_key_has_null(idx, table, write_set[pos].2);
            let mut conflict: Option<i64> = None;
            if !null_new {
                ibt.lookup_index_into(new_key, &mut probe)?;
                for &m in probe.iter() {
                    if m == rowid || deleted.contains(&m) {
                        continue; // own entry, or virtually deleted holder
                    }
                    match pos_of.get(&m) {
                        Some(&lpos) if lpos < pos => {
                            // Earlier write-set row: its old entry was
                            // vacated at its application time — UNLESS its
                            // key never changed or the row was skipped
                            // (OR IGNORE), in which case the entry persists.
                            let skipped = plan.skip.contains(&lpos);
                            let unchanged = keys[lpos].0 == keys[lpos].1;
                            if skipped || unchanged {
                                conflict = Some(m);
                                break;
                            }
                        }
                        _ => {
                            // Later write-set row's old entry, or an
                            // untouched baseline row — present now.
                            conflict = Some(m);
                            break;
                        }
                    }
                }
                if conflict.is_none() {
                    if let Some(&holder) = pending_new.get(new_key) {
                        if holder != rowid {
                            conflict = Some(holder);
                        }
                    }
                }
            }
            if let Some(holder) = conflict {
                match or_conflict {
                    ConflictResolution::Ignore => {
                        plan.skip.insert(pos);
                        continue; // row keeps old values; no key claimed
                    }
                    ConflictResolution::Replace => {
                        // Delete the conflicting holder row; the current
                        // row's update then proceeds cleanly.
                        plan.delete_rowids.push(holder);
                        deleted.insert(holder);
                        if let Some(&hpos) = pos_of.get(&holder) {
                            plan.skip.insert(hpos); // its pending update is discarded
                        }
                    }
                    _ => {
                        let cols = idx
                            .columns
                            .iter()
                            .map(|c| format!("{}.{}", table.name, c.name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(Error::constraint(format!(
                            "UNIQUE constraint failed: {}",
                            cols
                        )));
                    }
                }
            }
            pending_new.insert(new_key.clone(), rowid);
        }
    }
    Ok(plan)
}

/// Delete rows by rowid (table cell + every index entry) — the OR REPLACE
/// holder-deletion step. Mirrors the DELETE rowid fast path: fetch the
/// payload, delete the table cell, remove all index entries, and keep the
/// cached max-rowid consistent. FK parent checks run first when
/// `PRAGMA foreign_keys = ON` (SQLite's REPLACE-triggered deletes enforce
/// FKs too).
pub(crate) fn delete_rows_by_rowid(
    ctx: &mut ExecContext<'_>,
    table: &Table,
    rowids: &[i64],
) -> Result<()> {
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let table_name_lc = table.name.to_ascii_lowercase();
    let n_cols = table.n_columns();
    for &rowid in rowids {
        let root = ctx.table_root(table);
        if ctx.pager.foreign_keys_enabled() {
            let mut bt = Btree::new(ctx.pager, root, false);
            if let LookupResult::Found(payload) = bt.lookup_table(rowid)? {
                if let Ok(old_row) = decode_row(&payload, n_cols, rowid, table.rowid_alias) {
                    enforce_parent_delete_fks(ctx, table, &old_row, rowid, 0)?;
                }
            }
        }
        let root = ctx.table_root(table);
        let (new_root, old_payload) = {
            let mut bt = Btree::new(ctx.pager, root, false);
            let payload = bt.delete_table_get_payload(rowid)?;
            (bt.root, payload)
        };
        ctx.set_table_root_lc(&table_name_lc, new_root);
        if let Some(payload) = old_payload {
            if !indexes.is_empty() {
                if let Ok(row) = decode_row(&payload, n_cols, rowid, table.rowid_alias) {
                    for idx in &indexes {
                        delete_index_entry(ctx, idx, table, &row, rowid)?;
                    }
                }
            }
            ctx.invalidate_max_rowid_if_deleted(&table_name_lc, rowid);
        }
    }
    Ok(())
}

/// Apply a rowid-alias move for one UPDATE'd row: `UPDATE t SET id = X`
/// where `id` is the INTEGER PRIMARY KEY. SQLite moves the row — its
/// B+tree cell key changes — and enforces rowid uniqueness ("UNIQUE
/// constraint failed: t.id"). NULL auto-assigns a fresh rowid.
///
/// Returns `Ok(true)` when the row was moved, `Ok(false)` when the
/// conflict resolution was OR IGNORE (row keeps its old position — the
/// caller skips it). The payload must be encoded WITHOUT the alias
/// column (`encode_row_aliased`) — the rowid is the cell key.
fn apply_rowid_alias_move(
    ctx: &mut ExecContext<'_>,
    table: &Table,
    rowid: i64,
    new_alias: &Value,
    payload: &[u8],
    new_row: &[Value],
    or_conflict: ConflictResolution,
) -> Result<bool> {
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let alias_col = table
        .rowid_alias
        .and_then(|i| table.columns.get(i))
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "rowid".to_string());
    let target = match new_alias {
        Value::Integer(i) => *i,
        Value::Null => {
            // NULL on the rowid alias auto-assigns (INSERT semantics).
            let root = ctx.table_root(table);
            let max_rowid = ctx.get_or_scan_max_rowid(table)?;
            next_auto_rowid(ctx.pager, root, max_rowid)?
        }
        _ => return Err(Error::constraint("datatype mismatch")),
    };
    if target == rowid {
        // Same rowid — a plain in-place payload update; not a move.
        return Ok(false);
    }
    // Rowid uniqueness for the target.
    {
        let root = ctx.table_root(table);
        let mut bt = Btree::new(ctx.pager, root, false);
        if let LookupResult::Found(_) = bt.lookup_table(target)? {
            match or_conflict {
                ConflictResolution::Ignore => return Ok(false),
                ConflictResolution::Replace => {
                    delete_rows_by_rowid(ctx, table, &[target])?;
                }
                _ => {
                    return Err(Error::constraint(format!(
                        "UNIQUE constraint failed: {}.{}",
                        table.name, alias_col
                    )));
                }
            }
        }
    }
    // Move = delete the old cell (+ index entries, FK parent checks)
    // then insert at the new rowid (+ index entries).
    delete_rows_by_rowid(ctx, table, &[rowid])?;
    {
        let root = ctx.table_root(table);
        let mut bt = Btree::new(ctx.pager, root, false);
        bt.insert_table(target, payload)?;
        if bt.root != root {
            ctx.set_table_root(&table.name, bt.root);
        }
    }
    for idx in &indexes {
        insert_index_entry(ctx, idx, table, new_row, target)?;
    }
    Ok(true)
}

/// Insert an entry into an index for a given row.
fn insert_index_entry(
    ctx: &mut ExecContext<'_>,
    index: &crate::schema::Index,
    table: &Table,
    row: &[Value],
    rowid: i64,
) -> Result<()> {
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
fn delete_index_entry(
    ctx: &mut ExecContext<'_>,
    index: &crate::schema::Index,
    table: &Table,
    row: &[Value],
    rowid: i64,
) -> Result<()> {
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

// ============================================================================
// Auto-ROWID allocation (sqlite3BtreeNewRowid semantics)
// ============================================================================

/// Tiny xorshift64* PRNG for the rowid lottery taken when a table already
/// contains the largest possible integer rowid. This path is rare enough
/// (requires a row at i64::MAX) that a time-seeded shift register supplies
/// plenty of entropy without pulling in a rand dependency.
struct RowidRng(u64);

impl RowidRng {
    fn seeded() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        // xorshift64* must never start at zero.
        RowidRng(nanos | 1)
    }

    /// Next *positive* candidate rowid (SQLite restricts the lottery to
    /// positive candidates; see https://www.sqlite.org/autoinc.html).
    fn next_positive(&mut self) -> i64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let r = (v >> 1) as i64; // clear the sign bit -> [0, i64::MAX]
        if r == 0 {
            1
        } else {
            r
        }
    }
}

/// Rowid to assign for an auto-allocated insert (NULL INTEGER PRIMARY KEY,
/// or a rowid table with no alias). Normally `max_rowid + 1`; but when the
/// table already holds the largest possible integer rowid, mirror SQLite:
/// pick random positive candidates until one is unused (100 attempts),
/// then linearly scan for the first gap in the rowid space, and finally
/// fail gracefully instead of overflowing.
pub fn next_auto_rowid(pager: &Pager, root: u32, max_rowid: i64) -> Result<i64> {
    // Fast path: there is still room above the current maximum.
    if max_rowid < i64::MAX {
        return Ok(max_rowid + 1);
    }
    // Overflow lottery: random positive candidates, checked against the
    // live tree (rows inserted earlier in this statement/transaction are
    // already in the pager's dirty pages, so the lookup sees them).
    let mut rng = RowidRng::seeded();
    let mut bt = Btree::new(pager, root, false);
    for _ in 0..100 {
        let candidate = rng.next_positive();
        if matches!(bt.lookup_table(candidate)?, LookupResult::NotFound) {
            return Ok(candidate);
        }
    }
    // Linear fallback: a table scan walks rowids in ascending order, so
    // the first hole between consecutive rowids — or the slot below the
    // smallest key — is provably unused.
    let mut prev: Option<i64> = None;
    let mut free: Option<i64> = None;
    bt.scan_table(|rowid, _| {
        match prev {
            None => {
                if rowid > i64::MIN {
                    free = Some(rowid - 1);
                    return false;
                }
            }
            Some(p) => {
                if rowid > p.wrapping_add(1) {
                    free = Some(p + 1);
                    return false;
                }
            }
        }
        prev = Some(rowid);
        true
    })?;
    free.ok_or_else(|| Error::Runtime("table is full: every possible rowid is in use".into()))
}

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

fn exec_update(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    source: &Plan,
    assignments: &[(usize, Expr)],
    returning: Option<&[crate::sql::ast::ResultColumn]>,
    or_conflict: ConflictResolution,
    from: Option<&crate::planner::plan::UpdateFrom>,
) -> Result<ExecResult> {
    // `UPDATE ... FROM` (SQLite 3.33+): the target table is joined with
    // the FROM side and the WHERE clause spans both. Per SQLite
    // semantics: a target row matching multiple FROM rows is updated
    // ONCE (the last match supplies the SET expression values).
    if let Some(uf) = from {
        let result =
            exec_update_from(ctx, &table, source, assignments, returning, or_conflict, uf)?;
        if !ctx.in_transaction && !ctx.deferred_flush {
            ctx.pager.flush()?;
        }
        return Ok(result);
    }
    // Virtual table: scan matching rows through the module, evaluate the
    // SET expressions per row, batch xUpdate.
    if table.vtab.is_some() {
        let pred = extract_source_predicate(source);
        vtab_exec::exec_update_vtab(ctx, &table, assignments, pred.as_ref())?;
        return Ok(ExecResult {
            columns: Arc::from(Vec::new()),
            rows: Vec::new(),
        });
    }
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
    if let Some(result) =
        try_streaming_update(ctx, &table, source, assignments, returning, or_conflict)?
    {
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

    // ---- Pass 1: compute every new row (assignments + constraints),
    // without applying anything. This keeps the statement atomic: any
    // constraint error (NOT NULL, CHECK, UNIQUE) aborts BEFORE the first
    // B+tree modification, exactly like SQLite's statement-level ABORT.
    let mut write_set: Vec<(i64, Vec<Value>, Vec<Value>)> =
        Vec::with_capacity(source_res.rows.len());
    for row in &source_res.rows {
        let rowid = if let Some(idx) = table.rowid_alias {
            row[idx].as_integer()
        } else {
            return Err(Error::Unsupported(
                "UPDATE on a table without INTEGER PRIMARY KEY",
            ));
        };

        let mut new_row = row.clone();
        for (col_idx, expr) in assignments {
            new_row[*col_idx] = eval_row(expr, row, &col_names, &ctx.params, &ctx.named_params)?;
            let aff = table.columns[*col_idx].affinity;
            new_row[*col_idx] = aff.coerce(new_row[*col_idx].clone());
        }
        // NULL assigned to the rowid alias: SQLite rejects with
        // "datatype mismatch" (INSERT auto-assigns, UPDATE does not) —
        // checked BEFORE NOT NULL, which would otherwise mislabel it.
        if let Some(alias_idx) = table.rowid_alias {
            if matches!(new_row.get(alias_idx), Some(Value::Null)) {
                return Err(Error::constraint("datatype mismatch"));
            }
        }
        // NOT NULL + CHECK constraints on the updated row.
        enforce_row_constraints(&table, &new_row, &col_names, &ctx.params, &ctx.named_params)?;
        // Child-side FK: the new values must reference existing parents.
        enforce_child_fks(ctx, &table, &new_row)?;
        write_set.push((rowid, row.clone(), new_row));
    }

    // ---- Unique-index write-set simulation (SQLite sequential
    // semantics, collation-aware): errors atomically for ABORT-family,
    // produces the OR IGNORE skip / OR REPLACE delete plan otherwise.
    let unique_indexes: Vec<Arc<crate::schema::Index>> =
        indexes.iter().filter(|i| i.unique).cloned().collect();
    let ws_refs: Vec<(i64, &[Value], &[Value])> = write_set
        .iter()
        .map(|(r, o, n)| (*r, o.as_slice(), n.as_slice()))
        .collect();
    let plan = simulate_update_unique(ctx, &table, &unique_indexes, &ws_refs, or_conflict)?;

    // OR REPLACE: delete the conflicting holder rows (table + all index
    // entries) before applying the write set.
    if !plan.delete_rowids.is_empty() {
        delete_rows_by_rowid(ctx, &table, &plan.delete_rowids)?;
    }

    // ---- Pass 2: apply.
    for (pos, (rowid, row, new_row)) in write_set.into_iter().enumerate() {
        if plan.skip.contains(&pos) {
            continue; // OR IGNORE: row keeps its old values
        }
        // Rowid-alias move (`UPDATE t SET id = X`): the B+tree cell key
        // changes — delete + reinsert at the new rowid with uniqueness.
        if let Some(alias_idx) = table.rowid_alias {
            if let Some(new_alias) = new_row.get(alias_idx) {
                if !matches!(new_alias, Value::Integer(i) if *i == rowid) {
                    let payload = encode_row_aliased(&new_row, table.rowid_alias);
                    if apply_rowid_alias_move(
                        ctx,
                        &table,
                        rowid,
                        new_alias,
                        &payload,
                        &new_row,
                        or_conflict,
                    )? {
                        ctx.changes += 1;
                        updated += 1;
                        if let Some(ret) = returning {
                            returning_rows.push(project_returning_row(
                                ret,
                                &new_row,
                                &col_names,
                                &ctx.params,
                                &ctx.named_params,
                            )?);
                        }
                        continue;
                    }
                    // OR IGNORE kept the old position — skip the row.
                    if !matches!(new_alias, Value::Integer(_)) {
                        continue;
                    }
                }
            }
        }
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
            let old_key = encode_index_key(idx, &table, &row);
            let new_key = encode_index_key(idx, &table, &new_row);
            if old_key == new_key {
                // No change to this index's key — skip maintenance.
                continue;
            }
            delete_index_entry(ctx, idx, &table, &row, rowid)?;
            insert_index_entry(ctx, idx, &table, &new_row, rowid)?;
        }
        ctx.changes += 1;
        updated += 1;
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(
                ret,
                &new_row,
                &col_names,
                &ctx.params,
                &ctx.named_params,
            )?);
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

/// `UPDATE t SET ... FROM <expr> [WHERE ...]` (SQLite 3.33+).
///
/// Semantics (SQLite docs): the target table is joined with the FROM
/// side; the WHERE clause supplies the join condition and/or residual
/// filter, evaluated over target++from combined rows. If a target row
/// matches MULTIPLE FROM rows, it is updated ONCE — the LAST match
/// supplies the values for the SET expressions ("one arbitrary matching
/// row" in the docs; the last in practice). Target rows with no match
/// are left untouched (this is an inner-match update, not a LEFT JOIN
/// against the target — the target is always the driving side).
///
/// Column resolution: combined names are `target-qualified ++ from-
/// qualified` ("t.v", "src.v"). Unqualified references resolve
/// target-first (SQLite raises "ambiguous column name"; we prefer the
/// target side, which is what `SET v = v + 1` means in practice).
fn exec_update_from(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    target: &Plan,
    assignments: &[(usize, Expr)],
    returning: Option<&[crate::sql::ast::ResultColumn]>,
    or_conflict: ConflictResolution,
    uf: &crate::planner::plan::UpdateFrom,
) -> Result<ExecResult> {
    // Materialize both sides. The FROM side may itself be a join /
    // subquery / CTE — `execute` handles all plan shapes.
    let target_res = execute(target, ctx)?;
    let from_res = execute(&uf.plan, ctx)?;
    // Combined namespace for SET / WHERE evaluation.
    let mut combined_cols: Vec<String> =
        Vec::with_capacity(target_res.columns.len() + from_res.columns.len());
    combined_cols.extend(target_res.columns.iter().cloned());
    combined_cols.extend(from_res.columns.iter().cloned());
    let n_target = target_res.columns.len();
    // Plain (unqualified) target names for constraint checks and
    // RETURNING projection (SQLite: RETURNING references the target row).
    let plain_cols: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();

    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let has_update_triggers = crate::executor::triggers::has_triggers_for(
        ctx,
        table,
        &crate::sql::ast::TriggerEvent::Update(vec![]),
    );
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    let mut updated: i64 = 0;

    // ---- Pass 1: compute the write set (nothing applied yet — atomic
    // statement abort on any constraint violation).
    let mut write_set: Vec<(i64, Vec<Value>, Vec<Value>)> = Vec::new();
    for row in &target_res.rows {
        let rowid = if let Some(idx) = table.rowid_alias {
            row[idx].as_integer()
        } else {
            return Err(Error::Unsupported(
                "UPDATE on a table without INTEGER PRIMARY KEY",
            ));
        };
        // Find the LAST FROM row (if any) whose combination with this
        // target row satisfies the WHERE clause.
        let mut last_match: Option<Vec<Value>> = None;
        for frow in &from_res.rows {
            let mut combined: Vec<Value> = Vec::with_capacity(n_target + frow.len());
            combined.extend(row.iter().cloned());
            combined.extend(frow.iter().cloned());
            if let Some(w) = &uf.where_clause {
                let keep = eval_row(w, &combined, &combined_cols, &ctx.params, &ctx.named_params)?;
                if !keep.is_truthy() {
                    continue;
                }
            }
            last_match = Some(combined);
        }
        let Some(combined) = last_match else { continue };

        let mut new_row = row.clone();
        for (col_idx, expr) in assignments {
            new_row[*col_idx] = eval_row(
                expr,
                &combined,
                &combined_cols,
                &ctx.params,
                &ctx.named_params,
            )?;
            let aff = table.columns[*col_idx].affinity;
            new_row[*col_idx] = aff.coerce(new_row[*col_idx].clone());
        }
        // NULL assigned to the rowid alias: SQLite rejects with
        // "datatype mismatch" (before NOT NULL relabels it).
        if let Some(alias_idx) = table.rowid_alias {
            if matches!(new_row.get(alias_idx), Some(Value::Null)) {
                return Err(Error::constraint("datatype mismatch"));
            }
        }
        enforce_row_constraints(table, &new_row, &plain_cols, &ctx.params, &ctx.named_params)?;
        // Child-side FK: the new values must reference existing parents.
        enforce_child_fks(ctx, table, &new_row)?;
        write_set.push((rowid, row.clone(), new_row));
    }

    // ---- Unique-index write-set simulation (collation-aware).
    let unique_indexes: Vec<Arc<crate::schema::Index>> =
        indexes.iter().filter(|i| i.unique).cloned().collect();
    let ws_refs: Vec<(i64, &[Value], &[Value])> = write_set
        .iter()
        .map(|(r, o, n)| (*r, o.as_slice(), n.as_slice()))
        .collect();
    let plan = simulate_update_unique(ctx, table, &unique_indexes, &ws_refs, or_conflict)?;
    if !plan.delete_rowids.is_empty() {
        delete_rows_by_rowid(ctx, table, &plan.delete_rowids)?;
    }

    // ---- Pass 2: apply.
    for (pos, (rowid, row, new_row)) in write_set.into_iter().enumerate() {
        if plan.skip.contains(&pos) {
            continue; // OR IGNORE: row keeps its old values
        }
        // Rowid-alias move (`UPDATE t SET id = X`).
        if let Some(alias_idx) = table.rowid_alias {
            if let Some(new_alias) = new_row.get(alias_idx) {
                if !matches!(new_alias, Value::Integer(i) if *i == rowid) {
                    let payload = encode_row_aliased(&new_row, table.rowid_alias);
                    if apply_rowid_alias_move(
                        ctx,
                        table,
                        rowid,
                        new_alias,
                        &payload,
                        &new_row,
                        or_conflict,
                    )? {
                        ctx.changes += 1;
                        updated += 1;
                        if let Some(ret) = returning {
                            returning_rows.push(project_returning_row(
                                ret,
                                &new_row,
                                &plain_cols,
                                &ctx.params,
                                &ctx.named_params,
                            )?);
                        }
                        continue;
                    }
                    if !matches!(new_alias, Value::Integer(_)) {
                        continue;
                    }
                }
            }
        }
        let payload = encode_row_aliased(&new_row, table.rowid_alias);
        let root = ctx.table_root(table);
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            let did_in_place = bt.update_table(rowid, &payload).unwrap_or(false);
            if !did_in_place {
                bt.delete_table(rowid)?;
                bt.insert_table(rowid, &payload)?;
            }
            new_root = bt.root;
        }
        ctx.set_table_root(&table.name, new_root);
        for idx in &indexes {
            let old_key = encode_index_key(idx, table, &row);
            let new_key = encode_index_key(idx, table, &new_row);
            if old_key == new_key {
                continue;
            }
            delete_index_entry(ctx, idx, table, &row, rowid)?;
            insert_index_entry(ctx, idx, table, &new_row, rowid)?;
        }
        // AFTER UPDATE triggers (NEW = post-change, OLD = pre-change).
        if has_update_triggers {
            let changed_cols: Vec<String> = assignments
                .iter()
                .map(|(idx, _)| table.columns[*idx].name.clone())
                .collect();
            crate::executor::triggers::fire_triggers(
                ctx,
                table,
                &crate::sql::ast::TriggerEvent::Update(changed_cols),
                crate::sql::ast::TriggerWhen::After,
                Some(&new_row),
                Some(&row),
                &table.col_names,
            )?;
        }
        ctx.changes += 1;
        updated += 1;
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(
                ret,
                &new_row,
                &plain_cols,
                &ctx.params,
                &ctx.named_params,
            )?);
        }
    }
    if let Some(ret) = returning {
        return Ok(ExecResult {
            columns: returning_column_names(ret, &plain_cols).into(),
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
    or_conflict: ConflictResolution,
) -> Result<Option<ExecResult>> {
    // Detect the source shape and extract (table, filter_predicate, range, rowid).
    enum StreamingSource<'a> {
        Scan {
            table: &'a Arc<Table>,
            filter: Option<&'a Expr>,
        },
        RowidRange {
            table: &'a Arc<Table>,
            start: Option<&'a Expr>,
            end: Option<&'a Expr>,
            residual: Option<&'a Expr>,
        },
        RowidLookup {
            table: &'a Arc<Table>,
            rowid: &'a Expr,
        },
        IndexRange {
            table: &'a Arc<Table>,
            index: &'a Arc<crate::schema::Index>,
            start: Option<&'a (Expr, bool)>,
            end: Option<&'a (Expr, bool)>,
            residual: Option<&'a Expr>,
        },
    }
    let src = match source {
        Plan::Scan { table: t, .. } => StreamingSource::Scan {
            table: t,
            filter: None,
        },
        Plan::Filter { input, predicate } => {
            if let Plan::Scan { table: t, .. } = input.as_ref() {
                StreamingSource::Scan {
                    table: t,
                    filter: Some(predicate),
                }
            } else {
                return Ok(None);
            }
        }
        Plan::RowidRange {
            table: t,
            start,
            end,
            residual,
            ..
        } => StreamingSource::RowidRange {
            table: t,
            start: start.as_ref(),
            end: end.as_ref(),
            residual: residual.as_ref(),
        },
        Plan::RowidLookup {
            table: t, rowid, ..
        } => StreamingSource::RowidLookup { table: t, rowid },
        Plan::IndexRange {
            table: t,
            index,
            start,
            end,
            residual,
            ..
        } => StreamingSource::IndexRange {
            table: t,
            index,
            start: start.as_ref(),
            end: end.as_ref(),
            residual: residual.as_ref(),
        },
        _ => return Ok(None),
    };

    // The source table must match the UPDATE's target table (otherwise
    // we'd be updating rows from a different table, which isn't what
    // this fast path is for).
    // `*t` copies the `&Arc<Table>` field out of the match-ergonomics
    // double reference (`.clone()` on a `&&Arc` only clones the reference).
    let src_table: &Arc<Table> = match &src {
        StreamingSource::Scan { table: t, .. } => t,
        StreamingSource::RowidRange { table: t, .. } => t,
        StreamingSource::RowidLookup { table: t, .. } => t,
        StreamingSource::IndexRange { table: t, .. } => t,
    };
    if !src_table.name.eq_ignore_ascii_case(&table.name) {
        return Ok(None);
    }

    // Rowid-alias reassignment (`UPDATE t SET id = X`) moves the row —
    // its B+tree cell key changes. The streaming path patches payloads
    // in place at the OLD rowid, so it can't express a move; the general
    // path handles moves (delete + reinsert with uniqueness).
    if let Some(alias_idx) = table.rowid_alias {
        if assignments.iter().any(|(a_idx, _)| *a_idx == alias_idx) {
            return Ok(None);
        }
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
            let s = match start {
                Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
                None => i64::MIN,
            };
            let e = match end {
                Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
                None => i64::MAX,
            };
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
    let compiled_residual: Option<crate::executor::predicate::CompiledPredicate> = residual_pred
        .and_then(|p| crate::executor::predicate::compile_predicate(p, table, &table.name));

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
    let compiled_assignments: Vec<Option<crate::executor::predicate::CompiledExpr>> = assignments
        .iter()
        .map(|(_, e)| crate::executor::predicate::compile_expr(e, col_names, params.len()))
        .collect();
    let compiled_ref: Option<&[Option<crate::executor::predicate::CompiledExpr>]> =
        if compiled_assignments.iter().any(|c| c.is_some()) {
            Some(&compiled_assignments)
        } else {
            None
        };

    // Payload-patch fast path state: built when eligible (no constraints,
    // every SET compiles, no RETURNING) — see `UpdatePatchCtx`.
    let fk_enforced = ctx.pager.foreign_keys_enabled();
    let mut patch_ctx: Option<UpdatePatchCtx> = UpdatePatchCtx::try_new(
        table,
        assignments,
        &compiled_assignments,
        compiled_residual.as_ref(),
        residual_pred,
        returning,
        fk_enforced,
    );
    if std::env::var_os("RSQL_DBG_FUSED").is_some() {
        eprintln!(
            "[dbg] streaming-update eligibility: patch_ctx={} compiled_all={} compiled_residual={} residual={:?}",
            patch_ctx.is_some(),
            compiled_assignments.iter().all(|c| c.is_some()),
            compiled_residual.is_some(),
            residual_pred.is_some()
        );
    }

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
                    .map(|col_idx| assignments.iter().any(|(a_idx, _)| *a_idx == col_idx))
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
    // Rows patched IN PLACE by the fused single-pass scan (see the
    // RowidRange / Scan / merge-scan branches): these never enter
    // `updates`, so the final count adds them explicitly.
    let mut fused_patched: usize = 0;

    let mut bt = Btree::new(ctx.pager, root, false);
    if let Some(rowid) = lookup_rowid {
        // ---- SINGLE-ROW FAST PATH --------------------------------------
        // `UPDATE t SET ... WHERE id = ?` — the OLTP workhorse. When no
        // indexed column is assigned and no AFTER UPDATE triggers exist,
        // process and apply the row in ONE pass: fetch (leaf-hinted) →
        // decode → SET → constraints → encode → in-place patch. Skips the
        // `updates` / `order` / `sorted_updates` / `deferred` / `done_mask`
        // Vec machinery and its per-statement allocations entirely.
        if touched_indexes.is_empty() && !has_update_triggers && residual_pred.is_none() {
            let mut applied = false;
            let fetch = bt.lookup_table_with(rowid, |payload| {
                // Decode + SET + constraints + encode, all before the page
                // lock is released (payload borrowed, no copy).
                row_buf.clear();
                if decode_row_into(payload, n_cols, rowid, table.rowid_alias, &mut row_buf).is_err()
                {
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
                    returning_rows.push(project_returning_row(
                        ret,
                        &new_row,
                        col_names,
                        &params,
                        &named_params,
                    )?);
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
                    columns: returning_column_names(returning.unwrap_or(&[]), &table.col_names)
                        .into(),
                    rows: returning_rows,
                }));
            }
            if fetch.is_some() {
                // Row exists but was skipped (decode failure) — fall through
                // to the general path for identical semantics.
            } else {
                // Rowid absent: zero rows updated.
                return Ok(Some(ExecResult {
                    columns: returning_column_names(returning.unwrap_or(&[]), &table.col_names)
                        .into(),
                    rows: returning_rows,
                }));
            }
        }
        // RowidLookup source — fetch exactly one row by rowid.
        match bt.lookup_table(rowid)? {
            LookupResult::Found(payload) => {
                // `payload` is an owned Vec — hand it over for phase 2's
                // index maintenance (free stash, saves a re-fetch descent).
                let old_owned = if needs_old_payload {
                    Some(payload.clone())
                } else {
                    None
                };
                if let Err(e) = process_update_row(
                    ctx,
                    &payload,
                    n_cols,
                    rowid,
                    &mut row_buf,
                    &mut new_row,
                    &mut payload_buf,
                    assignments,
                    col_names,
                    &params,
                    &named_params,
                    table,
                    residual_pred,
                    &mut updates,
                    &mut update_arena,
                    &mut returning_rows,
                    returning,
                    old_owned,
                    compiled_ref,
                    compiled_residual.as_ref(),
                    patch_ctx.as_mut(),
                ) {
                    first_error = Some(e);
                }
            }
            LookupResult::NotFound => {}
        }
        Ok::<(), crate::error::Error>(())
    } else if matches!(src, StreamingSource::RowidRange { .. }) {
        // FUSED single-pass patch: when the payload-patch path is
        // eligible and nothing downstream needs the collected write set
        // (no index maintenance, no AFTER UPDATE triggers, no RETURNING),
        // the scan itself patches cells IN PLACE — one header walk per
        // row, zero payload copies, and phase 2's second table walk
        // disappears entirely. Overflow-payload / size-change rows fall
        // back to the ordinary collect path below.
        if patch_ctx.is_some() && touched_indexes.is_empty() && !has_update_triggers {
            #[allow(clippy::option_if_let_else)]
            let Some(pc) = patch_ctx.as_mut() else {
                unreachable!("eligibility checked is_some above")
            };
            let mut not_fusable: Vec<i64> = Vec::new();
            let mut overflow_fallback: Vec<i64> = Vec::new();
            let mut fused_err: Option<crate::error::Error> = None;
            bt.scan_table_range_patch(
                range_start,
                range_end,
                |rowid, payload| match fused_patch_row(
                    pc,
                    &mut row_buf,
                    payload,
                    rowid,
                    n_cols,
                    table.rowid_alias,
                    assignments,
                    compiled_ref,
                    compiled_residual.as_ref(),
                    residual_pred,
                    &params,
                    &named_params,
                    table,
                ) {
                    Some(true) => {
                        fused_patched += 1;
                        true
                    }
                    Some(false) => true,
                    None => {
                        not_fusable.push(rowid);
                        true
                    }
                },
                &mut overflow_fallback,
            )?;
            if std::env::var_os("RSQL_DBG_FUSED").is_some() {
                eprintln!(
                    "[dbg] fused RowidRange: patched={} not_fusable={} overflow={}",
                    fused_patched,
                    not_fusable.len(),
                    overflow_fallback.len()
                );
            }
            // Fallback rows (overflow payloads / size changes): the
            // ordinary collect path, exactly as before.
            for rowid in not_fusable.into_iter().chain(overflow_fallback) {
                match bt.lookup_table(rowid)? {
                    LookupResult::Found(payload) => {
                        if let Err(e) = process_update_row(
                            ctx,
                            &payload,
                            n_cols,
                            rowid,
                            &mut row_buf,
                            &mut new_row,
                            &mut payload_buf,
                            assignments,
                            col_names,
                            &params,
                            &named_params,
                            table,
                            residual_pred,
                            &mut updates,
                            &mut update_arena,
                            &mut returning_rows,
                            returning,
                            None,
                            compiled_ref,
                            compiled_residual.as_ref(),
                            None,
                        ) {
                            fused_err = Some(e);
                            break;
                        }
                    }
                    LookupResult::NotFound => {}
                }
            }
            if let Some(e) = fused_err {
                first_error = Some(e);
            }
            Ok::<(), crate::error::Error>(())
        } else {
            bt.scan_table_range_borrowed(range_start, range_end, |rowid, payload| {
                let old_owned = if needs_old_payload {
                    Some(payload.to_vec())
                } else {
                    None
                };
                if let Err(e) = process_update_row(
                    ctx,
                    payload,
                    n_cols,
                    rowid,
                    &mut row_buf,
                    &mut new_row,
                    &mut payload_buf,
                    assignments,
                    col_names,
                    &params,
                    &named_params,
                    table,
                    residual_pred,
                    &mut updates,
                    &mut update_arena,
                    &mut returning_rows,
                    returning,
                    old_owned,
                    compiled_ref,
                    compiled_residual.as_ref(),
                    patch_ctx.as_mut(),
                ) {
                    first_error = Some(e);
                    return false; // stop the scan
                }
                true
            })
        }
    } else if let StreamingSource::IndexRange {
        index, start, end, ..
    } = &src
    {
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
        // Collated index: fold bounds through the first column's collation.
        let fold_bound = |e: &Expr| -> Result<Value> {
            let v = evaluate(e, &eval_ctx)?;
            Ok(match index.columns.first() {
                Some(ic) => crate::plugin::collation_fold_key_ref(&ic.collation, &v).into_owned(),
                None => v,
            })
        };
        let start_key: Option<(Vec<u8>, bool)> = match start {
            Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
            None => None,
        };
        let end_key: Option<(Vec<u8>, bool)> = match end {
            Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
            None => None,
        };
        let scan_start: Vec<u8> = start_key
            .as_ref()
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
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
            // RANGE-BOUNDED walk: only the leaves covering
            // [min(rowid), max(rowid)] are visited — when the selection
            // clusters in rowid space (time-ordered inserts, sequential
            // ids — the common bulk-update shape) this walks HALF or less
            // of the table. Never worse than the full walk (a scattered
            // selection's min..max covers everything).
            let walk_lo = rowids[0];
            let walk_hi = rowids[rowids.len() - 1];
            // DENSE BITSET membership when rowids cluster: (max-min)/count
            // <= 64 keeps the bitset at <= 8 bytes of set-bits per row; the
            // per-row membership test is one shift+mask instead of the
            // sorted-pointer dance. Sparse selections keep the pointer walk.
            let span = (walk_hi - walk_lo) as u64;
            let dense = span / rowids.len() as u64 <= 64
                && (rowids.len() as u64) * 8 >= (span / 8 + 1).max(1)
                && (span / 8 + 1) <= 4 << 20;
            let bitset: Vec<u64> = if dense {
                let words = (span / 64 + 1) as usize;
                let mut b = vec![0u64; words];
                for &r in &rowids {
                    let bit = (r - walk_lo) as u64;
                    b[(bit / 64) as usize] |= 1u64 << (bit % 64);
                }
                b
            } else {
                Vec::new()
            };
            let mut ri = 0usize;
            let mut err: Option<crate::error::Error> = None;
            // Fused membership+patch walk: same walk bounds / bitset /
            // sorted-pointer membership as below, but members are patched
            // IN PLACE (see the RowidRange branch for eligibility notes).
            if patch_ctx.is_some() && touched_indexes.is_empty() && !has_update_triggers {
                #[allow(clippy::option_if_let_else)]
                let Some(pc) = patch_ctx.as_mut() else {
                    unreachable!("eligibility checked is_some above")
                };
                let mut not_fusable: Vec<i64> = Vec::new();
                let mut overflow_fallback: Vec<i64> = Vec::new();
                bt.scan_table_range_patch(
                    walk_lo,
                    walk_hi,
                    |rowid, payload| {
                        let is_match = if dense {
                            let bit = (rowid - walk_lo) as u64;
                            bitset[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0
                        } else {
                            while ri < rowids.len() && rowids[ri] < rowid {
                                ri += 1;
                            }
                            if ri >= rowids.len() {
                                return false; // all matches processed
                            }
                            rowids[ri] == rowid
                        };
                        if !is_match {
                            if !dense && ri >= rowids.len() {
                                return false;
                            }
                            return true;
                        }
                        if !dense {
                            ri += 1;
                        }
                        match fused_patch_row(
                            pc,
                            &mut row_buf,
                            payload,
                            rowid,
                            n_cols,
                            table.rowid_alias,
                            assignments,
                            compiled_ref,
                            compiled_residual.as_ref(),
                            residual_pred,
                            &params,
                            &named_params,
                            table,
                        ) {
                            Some(true) => {
                                fused_patched += 1;
                                true
                            }
                            Some(false) => true,
                            None => {
                                not_fusable.push(rowid);
                                true
                            }
                        }
                    },
                    &mut overflow_fallback,
                )?;
                if std::env::var_os("RSQL_DBG_FUSED").is_some() {
                    eprintln!(
                        "[dbg] fused merge-scan: patched={} not_fusable={} overflow={}",
                        fused_patched,
                        not_fusable.len(),
                        overflow_fallback.len()
                    );
                }
                // Fallback rows: ordinary collect path.
                for rowid in not_fusable.into_iter().chain(overflow_fallback) {
                    match bt.lookup_table(rowid)? {
                        LookupResult::Found(payload) => {
                            if let Err(e) = process_update_row(
                                ctx,
                                &payload,
                                n_cols,
                                rowid,
                                &mut row_buf,
                                &mut new_row,
                                &mut payload_buf,
                                assignments,
                                col_names,
                                &params,
                                &named_params,
                                table,
                                residual_pred,
                                &mut updates,
                                &mut update_arena,
                                &mut returning_rows,
                                returning,
                                None,
                                compiled_ref,
                                compiled_residual.as_ref(),
                                None,
                            ) {
                                first_error = Some(e);
                                break;
                            }
                        }
                        LookupResult::NotFound => {}
                    }
                }
            } else {
                bt.scan_table_range_borrowed(walk_lo, walk_hi, |rowid, payload| {
                    let is_match = if dense {
                        let bit = (rowid - walk_lo) as u64;
                        bitset[(bit / 64) as usize] & (1u64 << (bit % 64)) != 0
                    } else {
                        while ri < rowids.len() && rowids[ri] < rowid {
                            ri += 1;
                        }
                        if ri >= rowids.len() {
                            return false; // all matches processed
                        }
                        rowids[ri] == rowid
                    };
                    if !is_match {
                        // Past the last wanted rowid: stop the walk early (the
                        // range may over-cover).
                        if !dense && ri >= rowids.len() {
                            return false;
                        }
                        return true;
                    }
                    if !dense {
                        ri += 1;
                    }
                    let old_owned = if needs_old_payload {
                        Some(payload.to_vec())
                    } else {
                        None
                    };
                    if let Err(e) = process_update_row(
                        ctx,
                        payload,
                        n_cols,
                        rowid,
                        &mut row_buf,
                        &mut new_row,
                        &mut payload_buf,
                        assignments,
                        col_names,
                        &params,
                        &named_params,
                        table,
                        residual_pred,
                        &mut updates,
                        &mut update_arena,
                        &mut returning_rows,
                        returning,
                        old_owned,
                        compiled_ref,
                        compiled_residual.as_ref(),
                        patch_ctx.as_mut(),
                    ) {
                        err = Some(e);
                        return false;
                    }
                    true
                })?;
            }
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
                        let old_owned = if needs_old_payload {
                            Some(payload.clone())
                        } else {
                            None
                        };
                        if let Err(e) = process_update_row(
                            ctx,
                            &payload,
                            n_cols,
                            rowid,
                            &mut row_buf,
                            &mut new_row,
                            &mut payload_buf,
                            assignments,
                            col_names,
                            &params,
                            &named_params,
                            table,
                            residual_pred,
                            &mut updates,
                            &mut update_arena,
                            &mut returning_rows,
                            returning,
                            old_owned,
                            compiled_ref,
                            compiled_residual.as_ref(),
                            patch_ctx.as_mut(),
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
        // Full scan (optionally filtered). Fused single-pass patch when
        // eligible (see the RowidRange branch for the conditions).
        if patch_ctx.is_some() && touched_indexes.is_empty() && !has_update_triggers {
            #[allow(clippy::option_if_let_else)]
            let Some(pc) = patch_ctx.as_mut() else {
                unreachable!("eligibility checked is_some above")
            };
            let mut not_fusable: Vec<i64> = Vec::new();
            let mut overflow_fallback: Vec<i64> = Vec::new();
            bt.scan_table_range_patch(
                i64::MIN,
                i64::MAX,
                |rowid, payload| match fused_patch_row(
                    pc,
                    &mut row_buf,
                    payload,
                    rowid,
                    n_cols,
                    table.rowid_alias,
                    assignments,
                    compiled_ref,
                    compiled_residual.as_ref(),
                    residual_pred,
                    &params,
                    &named_params,
                    table,
                ) {
                    Some(true) => {
                        fused_patched += 1;
                        true
                    }
                    Some(false) => true,
                    None => {
                        not_fusable.push(rowid);
                        true
                    }
                },
                &mut overflow_fallback,
            )?;
            if std::env::var_os("RSQL_DBG_FUSED").is_some() {
                eprintln!(
                    "[dbg] fused full-scan: patched={} not_fusable={} overflow={}",
                    fused_patched,
                    not_fusable.len(),
                    overflow_fallback.len()
                );
            }
            for rowid in not_fusable.into_iter().chain(overflow_fallback) {
                match bt.lookup_table(rowid)? {
                    LookupResult::Found(payload) => {
                        if let Err(e) = process_update_row(
                            ctx,
                            &payload,
                            n_cols,
                            rowid,
                            &mut row_buf,
                            &mut new_row,
                            &mut payload_buf,
                            assignments,
                            col_names,
                            &params,
                            &named_params,
                            table,
                            residual_pred,
                            &mut updates,
                            &mut update_arena,
                            &mut returning_rows,
                            returning,
                            None,
                            compiled_ref,
                            compiled_residual.as_ref(),
                            None,
                        ) {
                            first_error = Some(e);
                            break;
                        }
                    }
                    LookupResult::NotFound => {}
                }
            }
            Ok::<(), crate::error::Error>(())
        } else {
            bt.scan_table_borrowed(|rowid, payload| {
                let old_owned = if needs_old_payload {
                    Some(payload.to_vec())
                } else {
                    None
                };
                if let Err(e) = process_update_row(
                    ctx,
                    payload,
                    n_cols,
                    rowid,
                    &mut row_buf,
                    &mut new_row,
                    &mut payload_buf,
                    assignments,
                    col_names,
                    &params,
                    &named_params,
                    table,
                    residual_pred,
                    &mut updates,
                    &mut update_arena,
                    &mut returning_rows,
                    returning,
                    old_owned,
                    compiled_ref,
                    compiled_residual.as_ref(),
                    patch_ctx.as_mut(),
                ) {
                    first_error = Some(e);
                    return false; // stop the scan
                }
                true
            })
        }
    }?;
    let new_root = bt.root;
    ctx.set_table_root(&table.name, new_root);

    // Surface any constraint error BEFORE applying updates (statement
    // aborts atomically — the pager snapshot/rollback handles the rest).
    if let Some(e) = first_error {
        return Err(e);
    }

    // ---- Unique-index write-set simulation (SQLite sequential
    // semantics, collation-aware — NOCASE / RTRIM / custom collations
    // all fold into the encoded keys). Runs BEFORE phase 2 touches
    // anything, so ABORT-family conflicts abort the statement atomically
    // with the SQLite-exact message; OR IGNORE produces the skip set;
    // OR REPLACE produces holder rowids to delete.
    let unique_touched: Vec<Arc<crate::schema::Index>> = touched_indexes
        .iter()
        .filter(|i| i.unique)
        .map(|i| (**i).clone())
        .collect();
    let plan = if !unique_touched.is_empty() {
        let mut ws_old: Vec<Value> = Vec::with_capacity(n_cols);
        let mut ws_new: Vec<Value> = Vec::with_capacity(n_cols);
        let mut ws: Vec<(i64, Vec<Value>, Vec<Value>)> = Vec::with_capacity(updates.len());
        for (rowid, range, old_stash) in updates.iter() {
            let Some(old_payload) = old_stash.as_deref() else {
                continue;
            };
            ws_old.clear();
            if decode_row_into(old_payload, n_cols, *rowid, table.rowid_alias, &mut ws_old).is_err()
            {
                continue;
            }
            let new_payload = &update_arena[range.clone()];
            ws_new.clear();
            if decode_row_into(new_payload, n_cols, *rowid, table.rowid_alias, &mut ws_new).is_err()
            {
                continue;
            }
            ws.push((*rowid, ws_old.clone(), ws_new.clone()));
        }
        let ws_refs: Vec<(i64, &[Value], &[Value])> = ws
            .iter()
            .map(|(r, o, n)| (*r, o.as_slice(), n.as_slice()))
            .collect();
        simulate_update_unique(ctx, table, &unique_touched, &ws_refs, or_conflict)?
    } else {
        UpdateConflictPlan::default()
    };
    // OR REPLACE: delete the conflicting holder rows (table + all index
    // entries) before the write set applies.
    if !plan.delete_rowids.is_empty() {
        delete_rows_by_rowid(ctx, table, &plan.delete_rowids)?;
    }
    // OR IGNORE: drop the skipped rows' RETURNING output. Phase 1 pushed
    // RETURNING rows in lockstep with `updates` (one per pushed update),
    // so position k in returning_rows == position k in updates.
    if !plan.skip.is_empty() && returning.is_some() {
        returning_rows = returning_rows
            .iter()
            .enumerate()
            .filter(|(k, _)| !plan.skip.contains(k))
            .map(|(_, r)| r.clone())
            .collect();
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
    // OR IGNORE / OR REPLACE: drop the skipped rows entirely (their
    // RETURNING rows were already filtered above).
    if !plan.skip.is_empty() {
        order.retain(|&i| !plan.skip.contains(&i));
    }
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
            let old_row_v: Option<Vec<Value>> = old_payload_opt
                .and_then(|op| decode_row(op, n_cols, *rowid, table.rowid_alias).ok());
            let new_row_v = decode_row(new_payload, n_cols, *rowid, table.rowid_alias).ok();
            if let (Some(old_r), Some(new_r)) = (old_row_v, new_row_v) {
                let changed_cols: Vec<String> = assignments
                    .iter()
                    .map(|(idx, _)| table.columns[*idx].name.clone())
                    .collect();
                crate::executor::triggers::fire_triggers(
                    ctx,
                    table,
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
                if decode_row_into(
                    old_payload,
                    n_cols,
                    *rowid,
                    table.rowid_alias,
                    &mut old_row_buf,
                )
                .is_err()
                {
                    continue;
                }
                new_row.clear();
                if decode_row_into(new_payload, n_cols, *rowid, table.rowid_alias, &mut new_row)
                    .is_err()
                {
                    continue;
                }
                for (ti, idx) in touched_indexes.iter().enumerate() {
                    let old_key = encode_index_key(idx, table, &old_row_buf);
                    let new_key = encode_index_key(idx, table, &new_row);
                    if old_key == new_key {
                        continue;
                    }
                    let ibt = &mut index_bts[ti];
                    if ibt.delete_index(&old_key, *rowid).is_ok()
                        && ibt.insert_index(&new_key, *rowid).is_err()
                    {
                        // Insert failed after a successful delete — the
                        // entry is gone; propagate a hard error.
                        return Err(Error::corruption(format!(
                            "index maintenance failed for {} (rowid {})",
                            idx.name, rowid
                        )));
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
    // Rows patched in place by the fused single-pass scans never entered
    // `updates` — count them here.
    updated += fused_patched as i64;
    ctx.changes += fused_patched as i64;
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(Some(ExecResult {
            columns: returning_column_names(ret, col_names).into(),
            rows: returning_rows,
        }));
    }
    Ok(Some(ExecResult {
        columns: Arc::from(vec!["updated".to_string()]),
        rows: vec![vec![Value::Integer(updated)]],
    }))
}

/// Fused in-place row patch for the single-pass UPDATE scan
/// (`scan_table_range_patch`). Returns:
///   `Some(true)`  — row patched in place (payload bytes written)
///   `Some(false)` — row skipped (residual predicate filtered it out)
///   `None`        — not fusable (size change / short payload / decode
///                   failure): the caller must run the ordinary collect
///                   path for this row.
///
/// Mirrors the patch branch of `process_update_row` but writes the new
/// payload bytes INTO the cell's own storage (same-size only), skipping
/// the update-arena copy and phase 2's second table walk entirely.
fn fused_patch_row(
    pc: &mut UpdatePatchCtx,
    row_buf: &mut Vec<Value>,
    payload: &mut [u8],
    rowid: i64,
    n_cols: usize,
    alias: Option<usize>,
    assignments: &[(usize, Expr)],
    compiled: Option<&[Option<crate::executor::predicate::CompiledExpr>]>,
    compiled_pred: Option<&crate::executor::predicate::CompiledPredicate>,
    residual_pred: Option<&Expr>,
    params: &[Value],
    named_params: &HashMap<String, Value>,
    table: &Arc<Table>,
) -> Option<bool> {
    let _ = named_params;
    if !crate::storage::row_codec::row_column_regions_into(payload, n_cols, alias, &mut pc.regions)
    {
        return None; // short payload (older schema) — general path handles defaults
    }
    // Decode ONLY the wanted columns, directly from their regions.
    row_buf.clear();
    row_buf.resize(n_cols, Value::Null);
    for &col in &pc.wanted {
        if alias == Some(col) {
            row_buf[col] = Value::Integer(rowid);
        } else {
            let (roff, rlen) = pc.regions[col];
            match Value::decode(&payload[roff as usize..(roff + rlen) as usize]) {
                Ok((v, _)) => row_buf[col] = v,
                Err(_) => return None,
            }
        }
    }
    // Residual filter (compiled positional evaluation).
    let keep = match (residual_pred, compiled_pred) {
        (None, _) => true,
        (Some(_), Some(cp)) => cp.eval(row_buf, IDENTITY_POSITIONS, params),
        // Unreachable (UpdatePatchCtx::try_new guarantees a compiled
        // residual whenever one exists) — but fall back rather than
        // silently filtering.
        (Some(_), None) => return None,
    };
    if !keep {
        return Some(false);
    }
    // Stage the encoded new values.
    pc.staging.clear();
    pc.slot_regions.clear();
    for (i, (col_idx, _)) in assignments.iter().enumerate() {
        let v = match compiled.and_then(|c| c.get(i)) {
            Some(Some(cexpr)) => cexpr.eval(row_buf, params),
            // Eligibility guarantees Some — unreachable; fall back safely.
            _ => return None,
        };
        let v = table.columns[*col_idx].affinity.coerce(v);
        let off = pc.staging.len();
        v.encode_into(&mut pc.staging);
        pc.slot_regions.push((off, pc.staging.len() - off));
    }
    // Same-size in-place patch of the assigned regions. TWO PHASES —
    // validate EVERY assigned region's size BEFORE writing ANY bytes: the
    // payload is the LIVE cell storage, so a partial patch followed by a
    // size-mismatch fallback would leave earlier columns written and the
    // fallback row-decode would apply the SETs a second time (observed as
    // patch_update_multi_assign's doubled values). The collect-path
    // variant patches a COPY, so it could check-and-write inline; we
    // cannot.
    let mut all_sizes_match = true;
    for (col, (_roff, rlen)) in pc.regions.iter().enumerate() {
        if let Some(slot) = pc.assigned_slot.get(col).and_then(|s| *s) {
            let (_soff, slen) = pc.slot_regions[slot];
            if slen as u32 != *rlen {
                all_sizes_match = false;
                break;
            }
        }
    }
    if !all_sizes_match {
        return None; // size change — general path (delete + insert)
    }
    for (col, (roff, rlen)) in pc.regions.iter().enumerate() {
        if let Some(slot) = pc.assigned_slot.get(col).and_then(|s| *s) {
            let (soff, slen) = pc.slot_regions[slot];
            payload[*roff as usize..(*roff + *rlen) as usize]
                .copy_from_slice(&pc.staging[soff..soff + slen]);
        }
    }
    Some(true)
}

pub(crate) struct UpdatePatchCtx {
    /// Columns to decode: assigned + everything the SET/residual
    /// expressions reference (ascending, deduped).
    wanted: Vec<usize>,
    /// assigned_slot[col] = Some(slot) when column `col` is assigned.
    assigned_slot: Vec<Option<usize>>,
    /// Staging buffer for the encoded new values (reused per row).
    staging: Vec<u8>,
    /// (offset, len) of each assignment's encoded value in `staging`.
    slot_regions: Vec<(usize, usize)>,
    /// Column regions of the current row's old payload (reused per row).
    regions: Vec<(u32, u32)>,
}

impl UpdatePatchCtx {
    /// Build the patch context when eligible, else None.
    fn try_new(
        table: &Arc<Table>,
        assignments: &[(usize, Expr)],
        compiled: &[Option<crate::executor::predicate::CompiledExpr>],
        compiled_residual: Option<&crate::executor::predicate::CompiledPredicate>,
        residual_pred: Option<&Expr>,
        returning: Option<&[crate::sql::ast::ResultColumn]>,
        fk_enforced: bool,
    ) -> Option<UpdatePatchCtx> {
        // RETURNING may project UNassigned columns — the patch path never
        // decodes them. No constraints (they'd read undecoded Nulls). Every
        // SET must compile (the AST walk needs the full row + col names).
        // The rowid-alias column's nullability is EXEMPT from the check:
        // callers guarantee no assignment targets it (a rowid reassignment
        // bails to the general path before this), and its payload slot is
        // the 1-byte rowid marker — never a decoded NULL.
        if returning.is_some()
            || table
                .columns
                .iter()
                .enumerate()
                .any(|(i, c)| table.rowid_alias != Some(i) && !c.nullable)
            || !table.check_exprs.is_empty()
            || (fk_enforced && !table.foreign_keys.is_empty())
            || compiled.len() != assignments.len()
            || compiled.iter().any(|c| c.is_none())
            || (residual_pred.is_some() && compiled_residual.is_none())
        {
            return None;
        }
        let mut wanted: Vec<usize> = Vec::new();
        for c in compiled.iter().flatten() {
            crate::executor::predicate::compiled_expr_columns(c, &mut wanted);
        }
        if let Some(cp) = compiled_residual {
            crate::executor::predicate::compiled_columns(cp, &mut wanted);
        }
        wanted.sort_unstable();
        wanted.dedup();
        let mut assigned_slot = vec![None; table.n_columns()];
        for (slot, (col_idx, _)) in assignments.iter().enumerate() {
            assigned_slot[*col_idx] = Some(slot);
        }
        Some(UpdatePatchCtx {
            wanted,
            assigned_slot,
            staging: Vec::with_capacity(64),
            slot_regions: Vec::with_capacity(assignments.len()),
            regions: Vec::with_capacity(table.n_columns()),
        })
    }
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
    updates: &mut Vec<crate::types::CellUpdate>,
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
    // Payload-patch fast path state (see `UpdatePatchCtx`); None when
    // ineligible. Taken by value (&mut) — the scratch buffers are reused
    // across rows.
    patch: Option<&mut UpdatePatchCtx>,
) -> Result<()> {
    // ---- Payload-patch fast path -----------------------------------
    // Selectively decode ONLY the referenced columns, evaluate the SETs,
    // and patch the assigned columns' byte regions in a copy of the old
    // payload. Falls back to the generic path below on ANY mismatch
    // (size change, missing column, decode error) — the generic body
    // re-decodes the full row, so correctness is identical.
    //
    // SINGLE header walk + SINGLE payload copy: the record header is
    // walked once (row_column_regions_into) to get every column's
    // (offset, len) region; the wanted columns decode straight from
    // those regions; the arena gets ONE copy of the old payload with
    // the assigned regions patched in place. (The previous shape walked
    // the header twice and copied the payload twice — ~25-40 ns/row on
    // UPDATE-range-shaped workloads.)
    if let Some(pc) = patch {
        if crate::storage::row_codec::row_column_regions_into(
            payload,
            n_cols,
            table.rowid_alias,
            &mut pc.regions,
        ) {
            // Decode ONLY the wanted columns, directly from their regions.
            row_buf.clear();
            row_buf.resize(n_cols, Value::Null);
            let mut decode_ok = true;
            for &col in &pc.wanted {
                if table.rowid_alias == Some(col) {
                    row_buf[col] = Value::Integer(rowid);
                } else {
                    let (roff, rlen) = pc.regions[col];
                    match Value::decode(&payload[roff as usize..(roff + rlen) as usize]) {
                        Ok((v, _)) => row_buf[col] = v,
                        Err(_) => {
                            decode_ok = false;
                            break;
                        }
                    }
                }
            }
            if decode_ok {
                let keep = match (residual_pred, compiled_pred) {
                    (None, _) => true,
                    (Some(_), Some(cp)) => cp.eval(row_buf, IDENTITY_POSITIONS, params),
                    (Some(_), None) => false, // eligibility guaranteed a compiled residual
                };
                if !keep {
                    return Ok(());
                }
                // Stage the encoded new values.
                pc.staging.clear();
                pc.slot_regions.clear();
                for (i, (col_idx, _)) in assignments.iter().enumerate() {
                    let v = match compiled.and_then(|c| c.get(i)) {
                        Some(Some(cexpr)) => cexpr.eval(row_buf, params),
                        _ => Value::Null, // eligibility guaranteed Some — unreachable
                    };
                    let v = table.columns[*col_idx].affinity.coerce(v);
                    let off = pc.staging.len();
                    v.encode_into(&mut pc.staging);
                    pc.slot_regions.push((off, pc.staging.len() - off));
                }
                // All assigned regions must keep their encoded size for
                // the in-place patch to work.
                let mut sizes_match = true;
                for (col, (_roff, rlen)) in pc.regions.iter().enumerate() {
                    if let Some(slot) = pc.assigned_slot.get(col).and_then(|s| *s) {
                        let (_soff, slen) = pc.slot_regions[slot];
                        if slen as u32 != *rlen {
                            sizes_match = false;
                            break;
                        }
                    }
                }
                if sizes_match {
                    // ONE payload copy — into the arena — then patch the
                    // assigned regions in place.
                    let start = update_arena.len();
                    update_arena.extend_from_slice(payload);
                    for (col, (roff, rlen)) in pc.regions.iter().enumerate() {
                        if let Some(slot) = pc.assigned_slot.get(col).and_then(|s| *s) {
                            let (soff, slen) = pc.slot_regions[slot];
                            update_arena[start + *roff as usize..start + (*roff + *rlen) as usize]
                                .copy_from_slice(&pc.staging[soff..soff + slen]);
                        }
                    }
                    updates.push((rowid, start..update_arena.len(), old_payload_stash));
                    return Ok(());
                }
                // Fall through — the generic path re-decodes the full row.
            }
            // Fall through — the generic path re-decodes the full row.
        }
        // Fall through — the generic path re-decodes the full row.
    }
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
            {
                let v = eval_row(pred, row_buf, col_names, params, named_params)?;
                v.is_truthy()
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
        returning_rows.push(project_returning_row(
            ret,
            new_row,
            col_names,
            params,
            named_params,
        )?);
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
        Plan::Scan {
            table: t,
            predicate,
            ..
        } => (t, predicate.as_ref(), None, None),
        Plan::Filter { input, predicate } => match input.as_ref() {
            Plan::Scan {
                table: t,
                predicate: None,
                ..
            } => (t, Some(predicate), None, None),
            _ => return Ok(None),
        },
        Plan::RowidRange {
            table: t,
            start,
            end,
            residual,
            ..
        } => (
            t,
            residual.as_ref(),
            Some((start.as_ref(), end.as_ref())),
            None,
        ),
        Plan::IndexRange {
            table: t,
            index,
            start,
            end,
            residual,
            ..
        } => (
            t,
            residual.as_ref(),
            None,
            Some((index, start.as_ref(), end.as_ref())),
        ),
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
        // collecting rowids (bounds logic mirrors try_streaming_update —
        // collated index bounds fold through the first column's collation).
        let empty_row: Vec<Value> = Vec::new();
        let empty_cols: Vec<String> = Vec::new();
        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
        let fold_bound = |e: &Expr| -> Result<Value> {
            let v = evaluate(e, &eval_ctx)?;
            Ok(match index.columns.first() {
                Some(ic) => crate::plugin::collation_fold_key_ref(&ic.collation, &v).into_owned(),
                None => v,
            })
        };
        let start_key: Option<(Vec<u8>, bool)> = match start {
            Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
            None => None,
        };
        let end_key: Option<(Vec<u8>, bool)> = match end {
            Some((e, inc)) => Some((fold_bound(e)?.encode_order_key(), *inc)),
            None => None,
        };
        let scan_start: Vec<u8> = start_key
            .as_ref()
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
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
                        if decode_row_into(&payload, n_cols, rid, table.rowid_alias, &mut row_buf)
                            .is_err()
                        {
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
                if decode_row_into(payload, n_cols, rowid, table.rowid_alias, &mut row_buf).is_err()
                {
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
                if decode_row_into(payload, n_cols, rowid, table.rowid_alias, &mut row_buf).is_err()
                {
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
    // Maintenance-free bulk fast path: no indexes, no RETURNING, no
    // triggers, no enforced FKs, and rowids collected in table order
    // (range/full-scan sources — the IndexRange collector emits INDEX key
    // order, not rowid order, and keeps the per-row loop). One sticky
    // leaf instead of a root descent per row: sequential mass deletes
    // drop from ~500 ns/row to ~40-80 ns/row.
    let rowids_in_table_order = index_range.is_none();
    if !need_row
        && !ctx.pager.foreign_keys_enabled()
        && rowids_in_table_order
        && !has_delete_triggers
    {
        let mut bt = Btree::new(ctx.pager, new_root, false);
        let n = bt.delete_rowids_inorder(&rowids)?;
        new_root = bt.root;
        ctx.set_table_root_lc(&table_name_lc, new_root);
        deleted = n as i64;
        ctx.changes += n as i64;
        if !ctx.in_transaction && !ctx.deferred_flush {
            ctx.pager.flush()?;
        }
        return Ok(Some(ExecResult {
            columns: Arc::from(vec!["deleted".to_string()]),
            rows: vec![vec![Value::Integer(deleted)]],
        }));
    }
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
                returning_rows.push(project_returning_row(
                    ret,
                    &row,
                    &col_names,
                    &params,
                    &named_params,
                )?);
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
    if let Some(ret) = returning {
        return Ok(Some(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        }));
    }
    Ok(Some(ExecResult {
        columns: Arc::from(vec!["deleted".to_string()]),
        rows: vec![vec![Value::Integer(deleted)]],
    }))
}

fn exec_delete(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    source: &Plan,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
) -> Result<ExecResult> {
    // Virtual table: collect matching rowids through the module, batch
    // xUpdate (delete ops).
    if table.vtab.is_some() {
        let pred = extract_source_predicate(source);
        vtab_exec::exec_delete_vtab(ctx, &table, pred.as_ref())?;
        return Ok(ExecResult {
            columns: Arc::from(Vec::new()),
            rows: Vec::new(),
        });
    }
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
    if let Plan::RowidLookup {
        table: src_table,
        rowid,
        ..
    } = source
    {
        if Arc::ptr_eq(src_table, &table) || src_table.name.eq_ignore_ascii_case(&table.name) {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx =
                EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            let rowid_val = evaluate(rowid, &eval_ctx)?.as_integer();
            let root = ctx.table_root(&table);
            // FOREIGN KEY (parent side): check BEFORE deleting — the row's
            // key values are needed for the child-reference scan and for
            // CASCADE / SET NULL rewrites.
            if ctx.pager.foreign_keys_enabled() {
                let mut bt = Btree::new(ctx.pager, root, false);
                if let LookupResult::Found(payload) = bt.lookup_table(rowid_val)? {
                    if let Ok(old_row) =
                        decode_row(&payload, table.n_columns(), rowid_val, table.rowid_alias)
                    {
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
                    let row =
                        decode_row(&payload, table.n_columns(), rowid_val, table.rowid_alias)?;
                    if let (Some(ret), Some(names)) = (returning, col_names_fast.as_deref()) {
                        returning_rows.push(project_returning_row(
                            ret,
                            &row,
                            names,
                            &ctx.params,
                            &ctx.named_params,
                        )?);
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
            return Err(Error::Unsupported(
                "DELETE on a table without INTEGER PRIMARY KEY",
            ));
        };
        if rowid > max_deleted {
            max_deleted = rowid;
        }
        // RETURNING: project the pre-delete row.
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(
                ret,
                row,
                &col_names,
                &ctx.params,
                &ctx.named_params,
            )?);
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

#[cfg(test)]
mod perf_probe_tests {
    use super::ExecContext;
    use crate::storage::btree::Btree;
    use crate::storage::row_codec::decode_row_selective;
    use crate::types::Value;
    use crate::Database;

    fn build_table(n: i64) -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)",
            [],
        )
        .unwrap();
        db.execute("BEGIN", []).unwrap();
        let mut i = 1;
        while i <= n {
            let end = (i + 99).min(n);
            let values: String = (i..=end)
                .map(|j| format!("('name{}', {}, {})", j, j, j))
                .collect::<Vec<_>>()
                .join(",");
            db.execute(
                &format!("INSERT INTO t (name, val, score) VALUES {values}"),
                [],
            )
            .unwrap();
            i = end + 1;
        }
        db.execute("COMMIT", []).unwrap();
        db
    }

    #[test]
    #[ignore]
    fn perf_scan_breakdown() {
        let db = build_table(10_000);
        let rounds = 300;
        let table = db.catalog.get_table("t").unwrap();
        let n_cols = table.n_columns();

        // Root resolution mirrors api.rs: shared maps (post-split roots)
        // first, catalog table.root_page as the base.
        let catalog_ptr: *const crate::schema::Catalog = &db.catalog;
        let shared = db.maps.read().clone();
        let ctx = ExecContext::new_reader(&db.pager, catalog_ptr, shared);
        let root = ctx.table_root(&table);

        // (a) bare btree walk
        let mut bt;
        let mut cells = 0usize;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            bt = Btree::new(&db.pager, root, false);
            bt.scan_table_borrowed(|_, _| {
                cells += 1;
                true
            })
            .unwrap();
        }
        let walk = t.elapsed().as_nanos() as f64 / rounds as f64 / 10_000.0;

        // (b) walk + selective decode of val only
        let wanted: Vec<usize> = vec![2];
        let mut sel: Vec<Value> = Vec::new();
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            bt = Btree::new(&db.pager, root, false);
            bt.scan_table_borrowed(|rowid, payload| {
                let _ = decode_row_selective(
                    payload,
                    n_cols,
                    &wanted,
                    rowid,
                    table.rowid_alias,
                    &mut sel,
                );
                true
            })
            .unwrap();
        }
        let decode1 = t.elapsed().as_nanos() as f64 / rounds as f64 / 10_000.0;

        // (c) walk + full decode_row
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            bt = Btree::new(&db.pager, root, false);
            let mut rows: Vec<Vec<Value>> = Vec::new();
            bt.scan_table_borrowed(|rowid, payload| {
                if let Ok(r) =
                    crate::storage::row_codec::decode_row(payload, n_cols, rowid, table.rowid_alias)
                {
                    rows.push(r);
                }
                true
            })
            .unwrap();
        }
        let full = t.elapsed().as_nanos() as f64 / rounds as f64 / 10_000.0;

        // (d) public API shapes
        for sql in [
            "SELECT COUNT(*), SUM(val), MIN(val), MAX(val), AVG(val) FROM t",
            "SELECT SUM(val), COUNT(*), AVG(score) FROM t WHERE val > 5000",
            "SELECT * FROM t",
        ] {
            let _ = db.query(sql, []).unwrap();
            let t = std::time::Instant::now();
            for _ in 0..rounds {
                let _ = db.query(sql, []).unwrap();
            }
            let q = t.elapsed().as_nanos() as f64 / rounds as f64 / 10_000.0;
            println!("query  {sql:<62} {q:7.1} ns/row");
        }
        println!("cells/scan = {cells} (first round)");
        println!(
            "walk-only = {walk:.1} ns/row   walk+seldecode(val) = {decode1:.1} ns/row   walk+decode_row(full) = {full:.1} ns/row"
        );
    }
}
