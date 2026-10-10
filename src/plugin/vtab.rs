//! Virtual tables: SQLite-style `CREATE VIRTUAL TABLE ... USING module(...)`
//! with a callback protocol closely modeled on `sqlite3_module`.
//!
//! A *module* is the implementation (registered with
//! [`Database::create_module`]); a *virtual table* is an instance created
//! (or re-connected) by `CREATE VIRTUAL TABLE`; a *cursor* is one scan over
//! the virtual table's rows.
//!
//! # Callback protocol
//!
//! ```text
//! CREATE VIRTUAL TABLE t USING csv(data='a,b\n1,2')
//!            │
//!            ▼
//!   module.create(args)        ── returns Box<dyn VirtualTable>
//!            │
//!    SELECT * FROM t WHERE x = 5
//!            │
//!            ▼
//!   table.best_index(constraints)   ── module picks a strategy + which
//!            │                        constraints it will handle itself
//!            ▼
//!   table.open()               ── returns Box<dyn VirtualTableCursor>
//!   cursor.filter(idx_num, idx_str, args)  ── start the scan (bound values
//!            │                               for the handled constraints)
//!            ▼
//!   cursor.eof? ── no  ── cursor.column(i) / cursor.rowid() ── cursor.next()
//!            │
//!           yes ── cursor dropped
//! ```
//!
//! Writes (`INSERT` / `UPDATE` / `DELETE`) call [`VirtualTable::update`]
//! when the module opted in with [`ModuleCaps::WRITABLE`].

use crate::error::{Error, Result};
use crate::types::Value;
use std::sync::Arc;

/// Capabilities a module advertises.
pub struct ModuleCaps;

impl ModuleCaps {
    /// Module implements [`VirtualTable::update`] (INSERT/UPDATE/DELETE on
    /// the virtual table are allowed).
    pub const WRITABLE: u32 = 1;
    /// `xConnect == xCreate` (ephemeral, in-memory tables that don't
    /// persist anything across connections — e.g. `series`).
    pub const EPHEMERAL: u32 = 2;
}

/// Connection-scoped virtual-table instance, attached to the catalog's
/// `Table` as `Table::vtab`. Holds the module name, the CREATE-time args,
/// and the live connection state behind a Mutex.
///
/// Two states:
/// - `Connected` — created by `CREATE VIRTUAL TABLE` (xCreate) or by
///   `ensure_connected` (xConnect, on first use after reopen);
/// - `Pending` — deserialized from the schema row at open time, before any
///   module is registered. The first statement touching the table resolves
///   the module from the plugin registry (thread-local scope) and calls
///   `connect`; if the module isn't registered, the statement fails with
///   `no such module: <name>` (SQLite shows the same error at first use).
pub struct VtabInstance {
    pub table_name: String,
    /// Lowercase module name (from CREATE VIRTUAL TABLE / the schema row).
    pub module_name: String,
    /// CREATE-time module args (argv[3..] in SQLite terms).
    pub args: Vec<String>,
    /// Resolved module Arc, cached after the first successful lookup.
    resolved: parking_lot::Mutex<Option<Arc<dyn VirtualTableModule>>>,
    /// xCreate-time instance (Connected) or deferred (Pending).
    state: parking_lot::Mutex<VtabState>,
    /// Cached aux (hidden) column descriptors, filled at connect.
    aux: parking_lot::Mutex<Vec<(String, String)>>,
    /// Cached shadow-table declarations, filled at connect.
    shadows: parking_lot::Mutex<Vec<ShadowTable>>,
    /// True when the module's in-memory state must be rebuilt from the
    /// content shadow before the next use (first use after reopen, or a
    /// rollback that may have restored the shadow's pages).
    needs_reindex: std::sync::atomic::AtomicBool,
}

enum VtabState {
    /// Module not yet resolved (deserialized from the schema row).
    Pending,
    /// Live instance.
    Connected(Box<dyn VirtualTable>),
}

impl VtabState {
    fn is_pending(&self) -> bool {
        matches!(self, VtabState::Pending)
    }
}

impl std::fmt::Debug for VtabInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtabInstance")
            .field("table", &self.table_name)
            .field("module", &self.module_name)
            .field("args", &self.args)
            .finish()
    }
}

impl VtabInstance {
    /// A live instance (CREATE VIRTUAL TABLE path).
    pub fn connected(
        table_name: String,
        module: Arc<dyn VirtualTableModule>,
        args: Vec<String>,
        instance: Box<dyn VirtualTable>,
    ) -> Self {
        let aux = instance.aux_columns();
        let shadows = instance.shadow_tables();
        Self {
            table_name,
            module_name: module.name().to_ascii_lowercase(),
            args,
            resolved: parking_lot::Mutex::new(Some(module)),
            state: parking_lot::Mutex::new(VtabState::Connected(instance)),
            aux: parking_lot::Mutex::new(aux),
            shadows: parking_lot::Mutex::new(shadows),
            // A freshly-created instance is in sync by construction.
            needs_reindex: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A pending instance (schema-load path): connected on first use.
    pub fn pending(table_name: String, module_name: String, args: Vec<String>) -> Self {
        Self {
            table_name,
            module_name: module_name.to_ascii_lowercase(),
            args,
            resolved: parking_lot::Mutex::new(None),
            state: parking_lot::Mutex::new(VtabState::Pending),
            aux: parking_lot::Mutex::new(Vec::new()),
            shadows: parking_lot::Mutex::new(Vec::new()),
            // Reopen: the module state must be rebuilt from the shadow.
            needs_reindex: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Resolve the module Arc (registry lookup through the thread-local
    /// statement scope; cached).
    fn resolve_module(&self) -> Result<Arc<dyn VirtualTableModule>> {
        if let Some(m) = self.resolved.lock().clone() {
            return Ok(m);
        }
        let m = super::lookup_module(&self.module_name)
            .ok_or_else(|| Error::semantic(format!("no such module: {}", self.module_name)))?;
        *self.resolved.lock() = Some(m.clone());
        Ok(m)
    }

    /// Resolve the module and xConnect if pending. Uses the thread-local
    /// plugin scope (must be inside a statement).
    pub(crate) fn ensure_connected(&self) -> Result<()> {
        {
            let st = self.state.lock();
            if matches!(*st, VtabState::Connected(_)) {
                return Ok(());
            }
        }
        let module = self.resolve_module()?;
        let instance = module.connect(&self.table_name, &self.args)?;
        let aux = instance.aux_columns();
        let shadows = instance.shadow_tables();
        let mut st = self.state.lock();
        // Another thread may have connected concurrently — keep theirs.
        if matches!(*st, VtabState::Connected(_)) {
            return Ok(());
        }
        *self.aux.lock() = aux;
        *self.shadows.lock() = shadows;
        *st = VtabState::Connected(instance);
        Ok(())
    }

    /// Run a closure with the live `&mut Box<dyn VirtualTable>`. Connects
    /// first if pending. The state lock is held for the closure's duration
    /// (cursors opened inside are independent of the lock).
    pub(crate) fn with_table<R>(
        &self,
        f: impl FnOnce(&mut Box<dyn VirtualTable>) -> Result<R>,
    ) -> Result<R> {
        self.ensure_connected()?;
        let mut st = self.state.lock();
        match &mut *st {
            VtabState::Connected(t) => f(t),
            VtabState::Pending => Err(Error::semantic(format!(
                "virtual table {} could not be connected",
                self.table_name
            ))),
        }
    }

    /// Is the module writable (INSERT/UPDATE/DELETE allowed)?
    pub fn writable(&self) -> Result<bool> {
        Ok(self.resolve_module()?.caps() & ModuleCaps::WRITABLE != 0)
    }

    /// True while the module hasn't been resolved (schema-load state).
    pub fn is_pending(&self) -> bool {
        self.state.lock().is_pending() && self.resolved.lock().is_none()
    }

    /// Force a pending instance into the Connected state (used by
    /// `Database::create_module`, which rebuilds the catalog Table around
    /// the connected instance). No-op when already connected.
    pub(crate) fn set_connected(&self, instance: Box<dyn VirtualTable>) -> Result<()> {
        let mut st = self.state.lock();
        if matches!(*st, VtabState::Connected(_)) {
            return Ok(());
        }
        *st = VtabState::Connected(instance);
        Ok(())
    }

    /// xDestroy the instance (DROP TABLE path): resolves the module and
    /// returns it with the CREATE args.
    pub(crate) fn module_and_args(&self) -> Result<(Arc<dyn VirtualTableModule>, Vec<String>)> {
        Ok((self.resolve_module()?, self.args.clone()))
    }

    /// Cached aux (hidden) column descriptors: (name, declared type).
    pub fn aux_columns(&self) -> Vec<(String, String)> {
        self.aux.lock().clone()
    }

    /// Cached shadow-table declarations.
    pub fn shadow_tables(&self) -> Vec<ShadowTable> {
        self.shadows.lock().clone()
    }

    /// The content shadow's table name, when the module declared one.
    pub fn content_shadow(&self) -> Option<String> {
        self.shadows
            .lock()
            .iter()
            .find(|s| s.content)
            .map(|s| s.name.clone())
    }

    /// Does the module want an in-memory rebuild before the next use?
    pub fn needs_reindex(&self) -> bool {
        self.needs_reindex
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Mark the module's state as potentially diverged from the content
    /// shadow (rollback / reopen): the next scan or DML rebuilds it.
    pub fn request_reindex(&self) {
        self.needs_reindex
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// The reindex ran; in-memory state matches the shadow again.
    pub fn clear_reindex(&self) {
        self.needs_reindex
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Run `reindex` on the connected instance with the given rows.
    pub(crate) fn run_reindex(&self, rows: &[(i64, Vec<Value>)]) -> Result<()> {
        self.with_table(|vt| vt.reindex(rows))
    }

    /// Connect now (if the module resolves — built-ins always do) and
    /// return the module's USER column descriptors, so the catalog entry
    /// can be rebuilt with the real schema at open time.
    pub(crate) fn connect_now(&self) -> Result<Vec<(String, String)>> {
        self.ensure_connected()?;
        self.with_table(|vt| Ok(vt.columns()))
    }
}

/// A constraint passed to `best_index`: one WHERE term the engine can see
/// for this scan, e.g. `WHERE x = 5` → `Constraint { column: 0, op: Eq, .. }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VtabConstraintOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    Glob,
    /// Full-text `col MATCH ?` (FTS5-style). The module receives the
    /// query string; `as_str` matches SQLite's
    /// SQLITE_INDEX_CONSTRAINT_MATCH rendering.
    Match,
    /// A module-overloaded function call used as a WHERE term — SQLite's
    /// `xFindFunction` mechanism (`SQLITE_INDEX_CONSTRAINT_FUNCTION`):
    /// `WHERE geopoly_overlap(geom, ?)` becomes a constraint on `geom`
    /// whose value is the OTHER argument. Only produced when the module
    /// lists the function in [`VirtualTable::overloaded_functions`] and
    /// the FIRST argument is a column of this table.
    Function(&'static str),
}

/// One WHERE constraint on a virtual-table column (or the rowid, `column ==
/// None`).
#[derive(Clone, Debug)]
pub struct VtabConstraint {
    /// Column index; `None` = rowid.
    pub column: Option<usize>,
    pub op: VtabConstraintOp,
    /// The RHS expression — evaluated by the engine before `filter` is
    /// called (bound parameters resolved).
    pub expr: crate::sql::ast::Expr,
}

impl VtabConstraintOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            VtabConstraintOp::Eq => "=",
            VtabConstraintOp::Lt => "<",
            VtabConstraintOp::Le => "<=",
            VtabConstraintOp::Gt => ">",
            VtabConstraintOp::Ge => ">=",
            VtabConstraintOp::Like => "LIKE",
            VtabConstraintOp::Glob => "GLOB",
            VtabConstraintOp::Match => "MATCH",
            VtabConstraintOp::Function(f) => f,
        }
    }
}

/// The strategy a module returns from `best_index`.
pub struct IndexInfo {
    /// Opaque strategy id passed back to `filter` (SQLite's `idxNum`).
    pub idx_num: usize,
    /// Optional strategy string passed back to `filter` (SQLite's `idxStr`).
    pub idx_str: Option<String>,
    /// For each constraint in the input list: `true` = the module will
    /// handle it in `filter` (the engine then does NOT re-apply it);
    /// `false` = leave it as a residual predicate the engine applies.
    pub handled: Vec<bool>,
    /// For each constraint: `true` = the module wants the value at
    /// `filter` time but does NOT promise exactness — SQLite's
    /// `omit=0` (geopoly's overlap/within are bbox PREFILTERS; the
    /// re-applied function does the exact test). The conjunct stays a
    /// residual too.
    pub recheck: Vec<bool>,
    /// Estimated scan cost (arbitrary units; lower = better). The engine
    /// compares full-table vtab scans only (0.0 = free).
    pub estimated_cost: f64,
    /// Estimated row count (0 = unknown).
    pub estimated_rows: i64,
}

impl IndexInfo {
    /// Default strategy: handle nothing, full scan.
    pub fn full_scan(n_constraints: usize) -> Self {
        Self {
            idx_num: 0,
            idx_str: None,
            handled: vec![false; n_constraints],
            recheck: vec![false; n_constraints],
            estimated_cost: 1e9,
            estimated_rows: 0,
        }
    }
}

/// One row of an `xUpdate` call, mirroring SQLite's argv protocol:
/// `argv[0]` is the OLD rowid (`None` = insert), `argv[1..]` are the NEW
/// column values (`None` = leave unchanged for UPDATE, NULL for INSERT).
pub type VtabUpdateArg = Vec<Value>;

/// One argument to `update` (see [`VtabUpdateArg`]).
#[derive(Clone, Debug)]
pub struct UpdateOp {
    /// OLD rowid — `None` for INSERT, `Some` for UPDATE/DELETE.
    pub old_rowid: Option<i64>,
    /// NEW rowid (already resolved by the engine: explicit insert value,
    /// old rowid for UPDATE when the statement doesn't move it, or a
    /// NULL meaning "module assigns" / "delete" when new_rowid is None
    /// AND all columns are None).
    ///
    /// Precisely: `INSERT` → old_rowid=None; `DELETE` → columns empty;
    /// `UPDATE` → both Some.
    pub new_rowid: Option<i64>,
    /// New column values; empty Vec for DELETE.
    pub columns: Vec<Option<Value>>,
}

/// A virtual-table module: the factory + behavior definition.
pub trait VirtualTableModule: Send + Sync {
    /// Module name — `CREATE VIRTUAL TABLE ... USING <name>`.
    fn name(&self) -> &str;

    /// Capability bits (see [`ModuleCaps`]).
    fn caps(&self) -> u32 {
        0
    }

    /// Create a new virtual-table instance. `args` are the raw tokens
    /// between the parentheses of `USING module(...)` — SQLite passes them
    /// as strings (argv[0] = module name, argv[1] = db name, argv[2] =
    /// table name, argv[3..] = user args); we pass only the USER args
    /// (argv[3..]) plus the table name.
    ///
    /// `create` is called for CREATE VIRTUAL TABLE; `connect` is called
    /// on database open for previously-created tables. Ephemeral modules
    /// typically return the same thing from both.
    fn create(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>>;
    fn connect(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        self.create(table, args)
    }

    /// Destroy the persistent side of a virtual table when its
    /// `DROP TABLE` runs (for modules with external state). Called only
    /// for modules whose `create` != `connect` matters; default no-op.
    fn destroy(&self, table: &str, args: &[String]) -> Result<()> {
        let _ = (table, args);
        Ok(())
    }
}

/// A connected virtual-table instance.
pub trait VirtualTable: Send {
    /// The module's declared column list (name + declared type). Cached by
    /// the engine as the catalog `Table` schema.
    fn columns(&self) -> Vec<(String, String)>;

    /// Plan a scan: see [`IndexInfo`]. Must not fail on empty constraints
    /// (full scans are always legal).
    fn best_index(&self, constraints: &[VtabConstraint]) -> Result<IndexInfo>;

    /// Open a cursor for one scan.
    fn open(&self) -> Result<Box<dyn VirtualTableCursor>>;

    /// Write path (modules advertising [`ModuleCaps::WRITABLE`]).
    /// `ops` are applied in order; a successful return commits them.
    /// `rowid_out` for INSERT: Some(new_rowid) if the module assigns one
    /// itself, None to accept the engine-suggested rowid.
    fn update(&mut self, _ops: Vec<UpdateOp>) -> Result<Vec<Option<i64>>> {
        Err(Error::Unsupported("virtual table is read-only"))
    }

    /// Called by the engine after `CREATE VIRTUAL TABLE` created the
    /// catalog row (vtab instances persist their own external state, if
    /// any). Default no-op.
    fn on_create(&mut self) -> Result<()> {
        Ok(())
    }

    /// Hidden (aux) columns beyond the user columns — FTS5's `rank` and
    /// table-self columns, for example. They are NOT part of the catalog
    /// schema (SELECT * and INSERT arity see only user columns), but the
    /// engine appends their values to every scan row under marker names
    /// and resolves explicit references to them. The module indexes them
    /// AFTER its user columns: user column i → cursor column i, aux
    /// column j → cursor column n_user + j.
    fn aux_columns(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Engine-managed shadow tables (see [`ShadowTable`]). Called once at
    /// CREATE VIRTUAL TABLE (the engine creates the tables) and again at
    /// connect (the engine locates the content shadow by name).
    fn shadow_tables(&self) -> Vec<ShadowTable> {
        Vec::new()
    }

    /// Rebuild the module's derived state from the content shadow's rows
    /// (rowid + user column values, scan order). Called at first use
    /// after reopen and after any rollback that may have restored the
    /// shadow's pages — the ground truth is always the shadow.
    fn reindex(&mut self, _rows: &[(i64, Vec<Value>)]) -> Result<()> {
        Ok(())
    }

    /// Statement-scoped aux function names (e.g. `bm25`, `highlight`,
    /// `snippet`). When a query scans this virtual table, calls to these
    /// functions resolve against the current scan's module state.
    fn aux_functions(&self) -> &'static [&'static str] {
        &[]
    }

    /// Function names this module OVERLOADS as index constraints —
    /// SQLite's `xFindFunction` protocol (`SQLITE_INDEX_CONSTRAINT_FUNCTION`).
    /// A WHERE conjunct `fn(col, expr)` whose name matches (case-insensitive)
    /// and whose FIRST argument is a column of this table is offered to
    /// `best_index` as a [`VtabConstraintOp::Function`] constraint on that
    /// column with `expr` as the value (geopoly's overlap/within).
    fn overloaded_functions(&self) -> &'static [&'static str] {
        &[]
    }

    /// The error message for an INSERT whose explicit rowid already
    /// exists — SQLite's `rtreeConstraintError`: "UNIQUE constraint
    /// failed: <table>.<first column>". `None` (default) = the module
    /// handles rowid conflicts itself (fts5) or allows duplicates.
    /// When Some, the engine checks the content shadow BEFORE writing
    /// and rejects (or applies OR REPLACE / OR IGNORE) with this text.
    fn rowid_unique_error(&self) -> Option<String> {
        None
    }

    /// The column whose INTEGER value is the table's rowid (rtree's `id`
    /// column — SQLite's cell.iRowid). When Some(c), the engine uses
    /// that column's value as the vtab rowid (shadow key, max-rowid
    /// tracking, and the duplicate-rowid conflict check).
    fn rowid_column(&self) -> Option<usize> {
        None
    }

    /// Normalize the vtab column values before the engine writes them
    /// to the content shadow (geopoly stores the canonical BLOB form of
    /// a JSON _shape in its t_rowid.a0, exactly like SQLite). Default:
    /// identity.
    fn shadow_normalize(&self, values: &[Value]) -> Vec<Value> {
        values.to_vec()
    }

    /// External-content tables (FTS5 `content='other_table'`): the module
    /// stores no content of its own — user-column reads are served from
    /// `(table, rowid_column)` of the named table, fetched by the engine
    /// at scan time. Returns (content table, rowid column).
    fn external_content(&self) -> Option<(String, String)> {
        None
    }

    /// Set the module's default rank weights (FTS5's
    /// `INSERT INTO t(t, rank) VALUES('rank', 'bm25(w0, ...)')`).
    fn set_rank_weights(&mut self, _weights: &[f64]) -> Result<()> {
        Ok(())
    }

    /// Evaluate one aux function for one row of the current scan. The
    /// rowid identifies the row (the value the engine passed to the
    /// function's first argument — the table self column).
    fn eval_aux(&self, _name: &str, _rowid: i64, _args: &[Value]) -> Result<Value> {
        Err(Error::Unsupported("module does not implement eval_aux"))
    }
}

/// One scan position over a virtual table.
pub trait VirtualTableCursor: Send {
    /// Start (or restart) the scan: `idx_num`/`idx_str` from `best_index`,
    /// `args` = values for the constraints marked handled.
    fn filter(&mut self, idx_num: usize, idx_str: Option<&str>, args: &[Value]) -> Result<()>;

    /// Advance. Only called when `eof` is false.
    fn next(&mut self) -> Result<()>;

    /// Scan finished?
    fn eof(&self) -> bool;

    /// Read column `i` (in the order of [`VirtualTable::columns`]).
    fn column(&self, i: usize) -> Result<Value>;

    /// Current row's rowid.
    fn rowid(&self) -> Result<i64>;
}

/// A virtual-table column descriptor used by the catalog bridge.
#[derive(Clone, Debug)]
pub struct VtabColumnDef {
    pub name: String,
    pub declared_type: String,
}

/// An engine-managed shadow table: the engine creates the regular table
/// at `CREATE VIRTUAL TABLE` time (its DDL comes from the module), keeps
/// the CONTENT shadow's rows in sync around `update()`, and replays them
/// into the module at connect/reopen and after any rollback that may
/// have restored the shadow's pages (`reindex`).
///
/// This is the engine-side equivalent of SQLite's fts5/rtree shadow
/// tables (`t_content`, `t_node`, ...): modules that keep derived state
/// in memory get transactional persistence for free — the shadow rows
/// ride the ordinary pager (they roll back with the transaction), and
/// the engine tells the module to rebuild whenever the ground truth may
/// have moved under it.
#[derive(Clone, Debug)]
pub struct ShadowTable {
    /// Shadow table name (e.g. `t_content`).
    pub name: String,
    /// Full CREATE TABLE statement (plain rowid table; no indexes — the
    /// module owns any derived structure).
    pub create_sql: String,
    /// `true` = the engine maintains the row content here: one row per
    /// vtab row, rowid = the vtab rowid, columns 1.. = the vtab's user
    /// column values in declared order. `update()` ops are applied to
    /// the shadow before the module sees them; `reindex` receives the
    /// shadow's rows.
    pub content: bool,
    /// Column mapping when the shadow's layout is NOT the engine default
    /// (vtab column i → shadow column i): `map[i]` is the shadow column
    /// index holding vtab column i's value. Shadow columns not in the
    /// map are skipped on read and written NULL. Used by geopoly, whose
    /// content shadow is SQLite-layout `t_rowid(rowid, nodeno, a0, a1,
    /// ...)` — the same table real SQLite creates, so geopoly tables in
    /// real SQLite files reindex transparently.
    pub content_map: Option<Vec<usize>>,
}

/// Build the catalog `Table` schema from a module's declared columns.
/// The caller attaches the `vtab` instance afterwards.
pub(crate) fn vtab_columns_to_schema(
    table_name: &str,
    cols: &[(String, String)],
) -> crate::schema::Table {
    let mut table = crate::schema::Table {
        name: table_name.to_string(),
        columns: Vec::with_capacity(cols.len()),
        root_page: 0,
        without_rowid: false,
        strict: false,
        rowid_alias: None,
        create_sql: String::new(),
        check_exprs: Vec::new(),
        foreign_keys: Vec::new(),
        col_names: std::sync::Arc::from(Vec::new()),
        col_affinities: std::sync::Arc::from(Vec::new()),
        qualified_col_names: std::sync::Arc::from(Vec::new()),
        vtab: None,
        pk_conflict: None,
        check_labels: Vec::new(),
        conflict_clauses: false,
    };
    for (name, ty) in cols {
        let affinity = crate::types::Affinity::from_declared_type(ty);
        table.columns.push(crate::schema::Column {
            name: name.clone(),
            affinity,
            declared_type: ty.clone(),
            decimal: crate::schema::parse_decimal_spec(ty),
            nullable: true,
            default: None,
            primary_key: false,
            primary_key_order: crate::sql::ast::Order::Asc,
            pk_seq: 0,
            explicit_not_null: false,
            autoincrement: false,
            unique: false,
            collation: "BINARY".to_string(),
            pk_collation: String::new(),
            generated: None,
            not_null_conflict: None,
        });
    }
    table.rebuild_name_caches();
    table
}
