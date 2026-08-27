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
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The maximum number of pages cached in memory.
const DEFAULT_CACHE_PAGES: usize = 2048;

/// Page size: 16 KiB (larger than SQLite's 4 KiB default) to reduce splits.
/// This trades some memory for fewer B+tree splits and better scan locality.
// const DEFAULT_PAGE_SIZE: u32 = 16384;

/// A database. Owns the pager and catalog.
pub struct Database {
    pager: Pager,
    catalog: Catalog,
    path: PathBuf,
    in_transaction: bool,
    /// Snapshot taken at BEGIN, used by ROLLBACK to restore the pager's
    /// state to the pre-transaction point.
    txn_snapshot: Option<crate::storage::pager::PagerSnapshot>,
    /// Root page overrides (table_name -> current root). Updated when B+tree
    /// splits change the root, since the catalog's Arc<Table> is immutable.
    root_overrides: HashMap<String, u32>,
    /// Max rowid per table (avoids O(n) scan on every INSERT).
    max_rowids: HashMap<String, i64>,
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
        Ok(Self { pager, catalog, path, in_transaction: false, txn_snapshot: None, root_overrides: HashMap::new(), max_rowids: HashMap::new() })
    }

    /// Open an in-memory database (no file). The data is lost when the
    /// `Database` is dropped.
    pub fn open_in_memory() -> Result<Self> {
        let path = PathBuf::from(":memory:");
        // Use a temp file under the hood — we don't support pure in-memory yet.
        let tmp = tempfile::NamedTempFile::new().map_err(|e| Error::Io(e))?;
        let mut db = Self::open(tmp.path())?;
        db.path = path;
        Ok(db)
    }

    /// Execute a statement that does not return rows (INSERT/UPDATE/DELETE/CREATE/...).
    pub fn execute<P: Params>(&mut self, sql: &str, params: P) -> Result<()> {
        let stmt = parse(sql)?;
        let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
        let in_txn = self.in_transaction;
        let txn_snap = self.txn_snapshot.take();
        let mut ctx = ExecContext::new(&mut self.pager, catalog_ptr);
        ctx.in_transaction = in_txn;
        ctx.txn_snapshot = txn_snap;
        ctx.root_overrides = std::mem::take(&mut self.root_overrides);
        ctx.max_rowids = std::mem::take(&mut self.max_rowids);
        for (i, v) in params.into_iter().enumerate() {
            ctx.bind(&format!("{}", i), v);
        }
        let result = Self::execute_statement_static(stmt, &mut ctx, &mut self.catalog, sql);
        self.in_transaction = ctx.in_transaction;
        self.txn_snapshot = ctx.txn_snapshot;
        self.root_overrides = std::mem::take(&mut ctx.root_overrides);
        self.max_rowids = std::mem::take(&mut ctx.max_rowids);
        result
    }

    /// Execute a query and return all rows.
    pub fn query<P: Params>(&mut self, sql: &str, params: P) -> Result<Vec<Row>> {
        let stmt = parse(sql)?;
        let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
        if let Some(plan) = plan_opt {
            let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
            let in_txn = self.in_transaction;
            let txn_snap = self.txn_snapshot.take();
            let mut ctx = ExecContext::new(&mut self.pager, catalog_ptr);
            ctx.in_transaction = in_txn;
            ctx.txn_snapshot = txn_snap;
            ctx.root_overrides = self.root_overrides.clone();
            ctx.max_rowids = self.max_rowids.clone();
            for (i, v) in params.into_iter().enumerate() {
                ctx.bind(&format!("{}", i), v);
            }
            let res = execute(&plan, &mut ctx)?;
            self.txn_snapshot = ctx.txn_snapshot;
            Ok(res.rows)
        } else {
            Ok(Vec::new())
        }
    }

    /// Execute a query and return (column_names, rows).
    pub fn query_with_columns<P: Params>(&mut self, sql: &str, params: P) -> Result<(Vec<String>, Vec<Row>)> {
        let stmt = parse(sql)?;
        let plan_opt = Self::plan_for_statement(&self.catalog, &stmt)?;
        if let Some(plan) = plan_opt {
            let catalog_ptr: *const crate::schema::Catalog = &self.catalog;
            let in_txn = self.in_transaction;
            let txn_snap = self.txn_snapshot.take();
            let mut ctx = ExecContext::new(&mut self.pager, catalog_ptr);
            ctx.in_transaction = in_txn;
            ctx.txn_snapshot = txn_snap;
            ctx.root_overrides = self.root_overrides.clone();
            ctx.max_rowids = self.max_rowids.clone();
            for (i, v) in params.into_iter().enumerate() {
                ctx.bind(&format!("{}", i), v);
            }
            let res = execute(&plan, &mut ctx)?;
            self.txn_snapshot = ctx.txn_snapshot;
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
        let source = if let Some(pred) = &upd.where_clause {
            crate::planner::plan::Plan::Filter {
                input: Box::new(scan),
                predicate: pred.clone(),
            }
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
            crate::planner::plan::Plan::Filter {
                input: Box::new(scan),
                predicate: pred.clone(),
            }
        } else {
            scan
        };
        Ok(crate::planner::plan::Plan::Delete {
            table,
            source: Box::new(source),
        })
    }

    fn execute_statement_static(stmt: Statement, ctx: &mut ExecContext, catalog: &mut Catalog, original_sql: &str) -> Result<()> {
        match stmt {
            Statement::Create(c) => Self::execute_create(c, ctx, catalog, original_sql),
            Statement::Drop(d) => Self::execute_drop(d, ctx, catalog),
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
            Statement::Pragma(p) => Self::execute_pragma(p, ctx),
            Statement::Attach(_) | Statement::Detach(_) => Ok(()),
            Statement::Vacuum(_) => Ok(()),
            Statement::Explain(_) => Ok(()),
            Statement::Select(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                // These produce rows; for `execute`, we just discard them.
                let plan_opt = match &stmt {
                    Statement::Select(s) => {
                        let mut planner = Planner::new(catalog);
                        Some(planner.plan_select(s)?)
                    }
                    Statement::Insert(_) => Some(Self::plan_insert(catalog, &stmt)?),
                    Statement::Update(_) => Some(Self::plan_update(catalog, &stmt)?),
                    Statement::Delete(_) => Some(Self::plan_delete(catalog, &stmt)?),
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
                    page.borrow_mut().init_leaf_table();
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
                    page.borrow_mut().init_leaf_index();
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
fn insert_schema_row(pager: &mut Pager, row: &[Value]) -> Result<()> {
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
fn delete_schema_row(pager: &mut Pager, kind: &str, name: &str) -> Result<()> {
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
fn load_schema(pager: &mut Pager, catalog: &mut Catalog) -> Result<()> {
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
