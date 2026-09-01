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
        Self {
            table_name,
            module_name: module.name().to_ascii_lowercase(),
            args,
            resolved: parking_lot::Mutex::new(Some(module)),
            state: parking_lot::Mutex::new(VtabState::Connected(instance)),
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
        let mut st = self.state.lock();
        // Another thread may have connected concurrently — keep theirs.
        if matches!(*st, VtabState::Connected(_)) {
            return Ok(());
        }
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
        qualified_col_names: std::sync::Arc::from(Vec::new()),
        vtab: None,
    };
    for (name, ty) in cols {
        let affinity = crate::types::Affinity::from_declared_type(ty);
        table.columns.push(crate::schema::Column {
            name: name.clone(),
            affinity,
            declared_type: ty.clone(),
            nullable: true,
            default: None,
            primary_key: false,
            primary_key_order: crate::sql::ast::Order::Asc,
            autoincrement: false,
            unique: false,
            collation: "BINARY".to_string(),
            generated: None,
        });
    }
    table.rebuild_name_caches();
    table
}
