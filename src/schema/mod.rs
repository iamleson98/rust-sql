//! Schema catalog: tables, columns, indexes, views.
//!
//! The catalog is itself stored as a special table (`sqlite_master` equivalent)
//! in the database. On open, we read the catalog into memory for fast lookup.

use crate::error::{Error, Result};
use crate::sql::ast::{
    ColumnConstraint, ColumnDef, ConflictResolution, ForeignKeyAction, IndexedColumn,
    Order, TableConstraint,
};
use crate::storage::page::PageId;
use crate::types::{Affinity, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// A column definition in the catalog.
#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub affinity: Affinity,
    pub declared_type: String,
    pub nullable: bool,
    pub default: Option<crate::sql::ast::Expr>,
    pub primary_key: bool,
    pub primary_key_order: Order,
    pub autoincrement: bool,
    pub unique: bool,
    pub collation: String,
    /// For generated columns: the expression and whether it is STORED.
    pub generated: Option<(crate::sql::ast::Expr, bool)>,
}

impl Column {
    /// Apply this column's affinity to a value.
    pub fn coerce(&self, v: Value) -> Value {
        self.affinity.coerce(v)
    }
}

/// One FOREIGN KEY clause of a table (from a column-level `REFERENCES` or
/// a table-level `FOREIGN KEY (...) REFERENCES ...`).
#[derive(Clone, Debug)]
pub struct ForeignKeyClause {
    /// Child-side column indices (into `Table::columns`).
    pub columns: Vec<usize>,
    /// Referenced (parent) table name.
    pub ref_table: String,
    /// Referenced (parent) column names. Empty means "the parent's PRIMARY
    /// KEY" (SQLite's implicit form: `REFERENCES parent`).
    pub ref_columns: Vec<String>,
    pub on_delete: ForeignKeyAction,
    pub on_update: ForeignKeyAction,
}

/// A table in the catalog.
#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub root_page: PageId,
    pub without_rowid: bool,
    pub strict: bool,
    /// The index of the column that is the rowid alias (INTEGER PRIMARY KEY),
    /// if any. INSERTs to this column are stored as the B+tree key.
    pub rowid_alias: Option<usize>,
    pub create_sql: String,
    /// CHECK constraints (column-level and table-level). Evaluated against
    /// the full row after defaults are applied; a NULL or false result
    /// rejects the write with `CHECK constraint failed: <table>`.
    pub check_exprs: Vec<crate::sql::ast::Expr>,
    /// FOREIGN KEY clauses (column-level REFERENCES and table-level FOREIGN
    /// KEY). Enforced on INSERT/UPDATE (child side) and DELETE/parent-key
    /// UPDATE (parent side) when `PRAGMA foreign_keys = ON` (default OFF,
    /// matching SQLite).
    pub foreign_keys: Vec<ForeignKeyClause>,
    /// Cached unqualified column names (`["id", "name", ...]`), shared by
    /// every executor fast path. Built once in `build_table`; cloning is a
    /// single refcount bump instead of N `String` deep clones per query.
    pub col_names: std::sync::Arc<[String]>,
    /// Cached `"table.column"`-qualified names, matching what `exec_scan`
    /// reports for an un-aliased scan. Built once in `build_table`.
    pub qualified_col_names: std::sync::Arc<[String]>,
}

impl Table {
    /// Look up a column by name (case-insensitive). Returns its index.
    pub fn find_column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Returns true if the column at `idx` is the rowid alias.
    pub fn is_rowid_alias(&self, idx: usize) -> bool {
        self.rowid_alias == Some(idx)
    }

    /// Number of columns (excluding the implicit rowid).
    pub fn n_columns(&self) -> usize {
        self.columns.len()
    }

    /// Affinities for all columns (used for INSERT coercion).
    pub fn affinities(&self) -> Vec<Affinity> {
        self.columns.iter().map(|c| c.affinity).collect()
    }
}

/// An index in the catalog.
#[derive(Clone, Debug)]
pub struct Index {
    pub name: String,
    pub table: String,
    pub columns: Vec<IndexColumn>,
    pub root_page: PageId,
    pub unique: bool,
    pub partial_expr: Option<crate::sql::ast::Expr>,
    pub create_sql: String,
}

#[derive(Clone, Debug)]
pub struct IndexColumn {
    pub name: String,
    pub order: Order,
    pub collation: String,
}

impl Index {
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }
}

/// A view in the catalog.
#[derive(Clone, Debug)]
pub struct View {
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub select: crate::sql::ast::SelectStatement,
    pub create_sql: String,
}

/// A trigger in the catalog.
#[derive(Clone, Debug)]
pub struct Trigger {
    pub name: String,
    pub table: String,
    pub when: crate::sql::ast::TriggerWhen,
    pub events: Vec<crate::sql::ast::TriggerEvent>,
    pub for_each_row: bool,
    pub when_clause: Option<crate::sql::ast::Expr>,
    pub body: Vec<crate::sql::ast::Statement>,
    pub create_sql: String,
}

/// The in-memory catalog: maps names to tables, indexes, views, triggers.
#[derive(Default)]
pub struct Catalog {
    tables: HashMap<String, Arc<Table>>,
    indexes: HashMap<String, Arc<Index>>,
    views: HashMap<String, Arc<View>>,
    triggers: HashMap<String, Arc<Trigger>>,
    /// Indexes grouped by table name (for fast lookup during query planning).
    indexes_by_table: HashMap<String, Vec<Arc<Index>>>,
    /// Triggers grouped by table name.
    triggers_by_table: HashMap<String, Vec<Arc<Trigger>>>,
    /// Schema cookie — bumped whenever the schema changes.
    pub schema_cookie: u32,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_table(&self, name: &str) -> Option<Arc<Table>> {
        self.tables.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Alloc-free fast path for already-lowercase names (the common case:
    /// table names in SQL are usually written lowercase, and the fast
    /// INSERT scanner slices them straight out of the statement text).
    /// Falls back to a lowercasing lookup for mixed-case names.
    pub fn get_table_fast(&self, name: &str) -> Option<Arc<Table>> {
        if let Some(t) = self.tables.get(name) {
            return Some(t.clone());
        }
        self.tables.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn get_index(&self, name: &str) -> Option<Arc<Index>> {
        self.indexes.get(&name.to_ascii_lowercase()).cloned()
    }

    /// All tables (name, table) — used by api.rs to seed the persisted-root
    /// map after loading the schema.
    pub fn all_tables(&self) -> Vec<(String, Arc<Table>)> {
        self.tables.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// All indexes (name, index).
    pub fn all_indexes(&self) -> Vec<(String, Arc<Index>)> {
        self.indexes.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn get_view(&self, name: &str) -> Option<Arc<View>> {
        self.views.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn indexes_on_table(&self, table: &str) -> Vec<Arc<Index>> {
        self.indexes_by_table
            .get(&table.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }

    pub fn triggers_on_table(&self, table: &str) -> Vec<Arc<Trigger>> {
        self.triggers_by_table
            .get(&table.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }

    pub fn add_table(&mut self, table: Table) {
        let key = table.name.to_ascii_lowercase();
        let idx_key = table.name.to_ascii_lowercase();
        let table_arc = Arc::new(table);
        // IndexesByTable entry for the table (will be populated by add_index).
        self.indexes_by_table.entry(idx_key).or_default();
        self.tables.insert(key, table_arc);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    pub fn add_index(&mut self, index: Index) {
        let key = index.name.to_ascii_lowercase();
        let table_key = index.table.to_ascii_lowercase();
        let idx_arc = Arc::new(index);
        self.indexes_by_table.entry(table_key).or_default().push(idx_arc.clone());
        self.indexes.insert(key, idx_arc);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    pub fn add_view(&mut self, view: View) {
        let key = view.name.to_ascii_lowercase();
        self.views.insert(key, Arc::new(view));
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    pub fn add_trigger(&mut self, trigger: Trigger) {
        let key = trigger.name.to_ascii_lowercase();
        let table_key = trigger.table.to_ascii_lowercase();
        let trig_arc = Arc::new(trigger);
        self.triggers_by_table.entry(table_key).or_default().push(trig_arc.clone());
        self.triggers.insert(key, trig_arc);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    /// ALTER TABLE RENAME TO: rewrite every other table's FOREIGN KEY
    /// clauses that reference `old_name` so they point at `new_name`
    /// (SQLite rewrites REFERENCES in modern rename mode).
    pub fn rename_fk_references(&mut self, old_name: &str, new_name: &str) {
        let tables: Vec<String> = self
            .tables
            .values()
            .filter(|t| {
                t.foreign_keys
                    .iter()
                    .any(|fk| fk.ref_table.eq_ignore_ascii_case(old_name))
            })
            .map(|t| t.name.to_ascii_lowercase())
            .collect();
        for key in tables {
            if let Some(t) = self.tables.get_mut(&key) {
                let mut t2 = (**t).clone();
                for fk in t2.foreign_keys.iter_mut() {
                    if fk.ref_table.eq_ignore_ascii_case(old_name) {
                        fk.ref_table = new_name.to_string();
                    }
                }
                // The stored CREATE SQL keeps its old REFERENCES text; the
                // in-memory catalog is authoritative until reopen, where
                // the (unrewritten) SQL re-parses with the old name — so
                // rewrite the create_sql text as well.
                let old_ref = format!("REFERENCES {}", old_name);
                let new_ref = format!("REFERENCES {}", new_name);
                t2.create_sql = t2.create_sql.replace(&old_ref, &new_ref);
                *t = Arc::new(t2);
            }
        }
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    /// ALTER TABLE RENAME TO: move the table entry (and its index and
    /// trigger registrations) from `old_name` to `new_name` without
    /// touching the underlying B-trees. `drop_table` + `add_table` would
    /// discard the index/triggers-by-table maps — a rename must keep them.
    pub fn rename_table(&mut self, old_name: &str, new_name: &str) -> Option<()> {
        let old_key = old_name.to_ascii_lowercase();
        let new_key = new_name.to_ascii_lowercase();
        if self.tables.contains_key(&new_key) {
            return None;
        }
        let t = self.tables.remove(&old_key)?;
        // Move index registrations.
        if let Some(idx_list) = self.indexes_by_table.remove(&old_key) {
            for idx in &idx_list {
                self.indexes.remove(&idx.name.to_ascii_lowercase());
                // Re-key the index's table field by cloning with the new
                // table name.
                let mut i2 = (**idx).clone();
                i2.table = new_name.to_string();
                let i2 = Arc::new(i2);
                self.indexes.insert(i2.name.to_ascii_lowercase(), i2.clone());
                self.indexes_by_table
                    .entry(new_key.clone())
                    .or_default()
                    .push(i2);
            }
        }
        // Move trigger registrations.
        if let Some(trig_list) = self.triggers_by_table.remove(&old_key) {
            for trig in &trig_list {
                self.triggers.remove(&trig.name.to_ascii_lowercase());
                let mut t2 = (**trig).clone();
                t2.table = new_name.to_string();
                let t2 = Arc::new(t2);
                self.triggers.insert(t2.name.to_ascii_lowercase(), t2.clone());
                self.triggers_by_table
                    .entry(new_key.clone())
                    .or_default()
                    .push(t2);
            }
        }
        self.tables.insert(new_key, t);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(())
    }

    /// Replace a table's Arc in place (used by ALTER TABLE RENAME after
    /// rename_table moved the entry under the new key).
    pub fn replace_table(&mut self, name: &str, table: Table) -> Option<()> {
        let key = name.to_ascii_lowercase();
        if !self.tables.contains_key(&key) {
            return None;
        }
        self.tables.insert(key, Arc::new(table));
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(())
    }

    pub fn drop_table(&mut self, name: &str) -> Option<Arc<Table>> {
        let key = name.to_ascii_lowercase();
        let t = self.tables.remove(&key)?;
        // Remove all indexes on this table.
        if let Some(idx_list) = self.indexes_by_table.remove(&key) {
            for idx in idx_list {
                self.indexes.remove(&idx.name.to_ascii_lowercase());
            }
        }
        // Remove triggers on this table.
        if let Some(trig_list) = self.triggers_by_table.remove(&key) {
            for trig in trig_list {
                self.triggers.remove(&trig.name.to_ascii_lowercase());
            }
        }
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(t)
    }

    pub fn drop_index(&mut self, name: &str) -> Option<Arc<Index>> {
        let key = name.to_ascii_lowercase();
        let idx = self.indexes.remove(&key)?;
        if let Some(list) = self.indexes_by_table.get_mut(&idx.table.to_ascii_lowercase()) {
            list.retain(|i| i.name.to_ascii_lowercase() != key);
        }
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(idx)
    }

    pub fn drop_view(&mut self, name: &str) -> Option<Arc<View>> {
        let key = name.to_ascii_lowercase();
        let v = self.views.remove(&key)?;
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(v)
    }

    pub fn drop_trigger(&mut self, name: &str) -> Option<Arc<Trigger>> {
        let key = name.to_ascii_lowercase();
        let t = self.triggers.remove(&key)?;
        if let Some(list) = self.triggers_by_table.get_mut(&t.table.to_ascii_lowercase()) {
            list.retain(|tr| tr.name.to_ascii_lowercase() != key);
        }
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(t)
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty() && self.indexes.is_empty() && self.views.is_empty()
    }
}

/// Build a `Table` from a parsed `CREATE TABLE` statement.
pub fn build_table(
    name: &str,
    columns: &[ColumnDef],
    constraints: &[TableConstraint],
    root_page: PageId,
    without_rowid: bool,
    strict: bool,
    create_sql: &str,
) -> Result<Table> {
    let mut table_columns = Vec::with_capacity(columns.len());
    let mut rowid_alias: Option<usize> = None;

    // First, find the PRIMARY KEY at the table level.
    let mut table_pk: Vec<IndexedColumn> = Vec::new();
    for c in constraints {
        if let TableConstraint::PrimaryKey { columns } = c {
            table_pk = columns.clone();
        }
    }

    for (i, col) in columns.iter().enumerate() {
        let affinity = if col.type_name.is_empty() {
            Affinity::Blob
        } else {
            Affinity::from_declared_type(&col.type_name)
        };
        let mut nullable = true;
        let mut primary_key = false;
        let mut primary_key_order = Order::Asc;
        let mut autoincrement = false;
        let mut unique = false;
        let mut default = None;
        let mut collation = "BINARY".to_string();
        let mut generated = None;

        for constraint in &col.constraints {
            match constraint {
                ColumnConstraint::PrimaryKey { autoincrement: ai, order } => {
                    primary_key = true;
                    autoincrement = *ai;
                    primary_key_order = *order;
                    nullable = false;
                    // INTEGER PRIMARY KEY is a rowid alias.
                    if affinity == Affinity::Integer {
                        rowid_alias = Some(i);
                    }
                }
                ColumnConstraint::NotNull => nullable = false,
                ColumnConstraint::Null => nullable = true,
                ColumnConstraint::Unique => unique = true,
                ColumnConstraint::Check(_) => {}
                ColumnConstraint::Default(e) => default = Some(e.clone()),
                ColumnConstraint::Collate(c) => collation = c.clone(),
                ColumnConstraint::References { .. } => {}
                ColumnConstraint::GeneratedAs { expr, stored } => {
                    generated = Some((expr.clone(), *stored));
                }
            }
        }

        table_columns.push(Column {
            name: col.name.clone(),
            affinity,
            declared_type: col.type_name.clone(),
            nullable,
            default,
            primary_key,
            primary_key_order,
            autoincrement,
            unique,
            collation,
            generated,
        });
    }

    // Handle table-level PRIMARY KEY: mark columns as PK.
    if !table_pk.is_empty() {
        for ic in &table_pk {
            if let Some(idx) = table_columns.iter().position(|c| c.name.eq_ignore_ascii_case(&ic.name)) {
                table_columns[idx].primary_key = true;
                table_columns[idx].primary_key_order = ic.order;
                table_columns[idx].nullable = false;
                // If single INTEGER PRIMARY KEY at table level, it's also a rowid alias.
                if table_pk.len() == 1 && table_columns[idx].affinity == Affinity::Integer {
                    rowid_alias = Some(idx);
                }
            }
        }
    }

    // Mark UNIQUE columns from table-level UNIQUE constraints.
    for c in constraints {
        if let TableConstraint::Unique(cols) = c {
            for ic in cols {
                if let Some(idx) = table_columns.iter().position(|c| c.name.eq_ignore_ascii_case(&ic.name)) {
                    table_columns[idx].unique = true;
                }
            }
        }
    }

    // Collect CHECK constraints: column-level first, then table-level.
    let mut check_exprs = Vec::new();
    for col in columns {
        for constraint in &col.constraints {
            if let ColumnConstraint::Check(e) = constraint {
                check_exprs.push(e.clone());
            }
        }
    }
    for c in constraints {
        if let TableConstraint::Check(e) = c {
            check_exprs.push(e.clone());
        }
    }

    // Collect FOREIGN KEY clauses: column-level REFERENCES first, then
    // table-level FOREIGN KEY (...). Child columns resolve to indices now
    // (case-insensitive); unknown child columns are a semantic error.
    let mut foreign_keys = Vec::new();
    for (i, col) in columns.iter().enumerate() {
        for constraint in &col.constraints {
            if let ColumnConstraint::References { table: rt, columns: rc, on_delete, on_update } = constraint {
                foreign_keys.push(ForeignKeyClause {
                    columns: vec![i],
                    ref_table: rt.clone(),
                    ref_columns: rc.clone(),
                    on_delete: *on_delete,
                    on_update: *on_update,
                });
            }
        }
    }
    for c in constraints {
        if let TableConstraint::ForeignKey { columns: cols, ref_table, ref_columns, on_delete, on_update } = c {
            let mut child_idx = Vec::with_capacity(cols.len());
            for cn in cols {
                match table_columns.iter().position(|tc| tc.name.eq_ignore_ascii_case(cn)) {
                    Some(idx) => child_idx.push(idx),
                    None => {
                        return Err(Error::semantic(format!(
                            "unknown column {} in FOREIGN KEY definition",
                            cn
                        )))
                    }
                }
            }
            foreign_keys.push(ForeignKeyClause {
                columns: child_idx,
                ref_table: ref_table.clone(),
                ref_columns: ref_columns.clone(),
                on_delete: *on_delete,
                on_update: *on_update,
            });
        }
    }

    let plain: Vec<String> = table_columns.iter().map(|c| c.name.clone()).collect();
    let qualified: Vec<String> = table_columns
        .iter()
        .map(|c| format!("{}.{}", name, c.name))
        .collect();

    Ok(Table {
        name: name.to_string(),
        columns: table_columns,
        root_page,
        without_rowid,
        strict,
        rowid_alias,
        create_sql: create_sql.to_string(),
        check_exprs,
        foreign_keys,
        col_names: plain.into(),
        qualified_col_names: qualified.into(),
    })
}

/// Default conflict resolution for INSERT/UPDATE.
pub fn default_conflict_resolution(or: Option<ConflictResolution>) -> ConflictResolution {
    or.unwrap_or(ConflictResolution::Abort)
}

/// Convert a parsed `CREATE INDEX` statement's columns to catalog columns.
pub fn build_index_columns(cols: &[IndexedColumn], table: &Table) -> Result<Vec<IndexColumn>> {
    let mut out = Vec::with_capacity(cols.len());
    for c in cols {
        if table.find_column(&c.name).is_none() {
            return Err(Error::semantic(format!(
                "column {} not found in table {}",
                c.name, table.name
            )));
        }
        out.push(IndexColumn {
            name: c.name.clone(),
            order: c.order,
            collation: c.collation.clone().unwrap_or_else(|| "BINARY".to_string()),
        });
    }
    Ok(out)
}

/// Encode a catalog entry as a row in the schema table (`sqlite_master`).
/// Columns: (type, name, tbl_name, rootpage, sql).
pub fn encode_schema_row(kind: &str, name: &str, tbl_name: &str, rootpage: PageId, sql: &str) -> Vec<Value> {
    vec![
        Value::Text(kind.to_string()),
        Value::Text(name.to_string()),
        Value::Text(tbl_name.to_string()),
        Value::Integer(rootpage as i64),
        Value::Text(sql.to_string()),
    ]
}

/// Decode a schema row.
pub fn decode_schema_row(row: &[Value]) -> Option<(&str, &str, &str, PageId, &str)> {
    if row.len() < 5 {
        return None;
    }
    let kind = match &row[0] {
        Value::Text(s) => s.as_str(),
        _ => return None,
    };
    let name = match &row[1] {
        Value::Text(s) => s.as_str(),
        _ => return None,
    };
    let tbl_name = match &row[2] {
        Value::Text(s) => s.as_str(),
        _ => return None,
    };
    let rootpage = match &row[3] {
        Value::Integer(i) => *i as PageId,
        _ => return None,
    };
    let sql = match &row[4] {
        Value::Text(s) => s.as_str(),
        _ => "",
    };
    Some((kind, name, tbl_name, rootpage, sql))
}

/// Convert FK action to integer code for storage.
pub fn fk_action_to_int(a: ForeignKeyAction) -> i64 {
    match a {
        ForeignKeyAction::NoAction => 0,
        ForeignKeyAction::Restrict => 1,
        ForeignKeyAction::SetNull => 2,
        ForeignKeyAction::SetDefault => 3,
        ForeignKeyAction::Cascade => 4,
    }
}

/// Convert integer code back to FK action.
pub fn int_to_fk_action(i: i64) -> ForeignKeyAction {
    match i {
        1 => ForeignKeyAction::Restrict,
        2 => ForeignKeyAction::SetNull,
        3 => ForeignKeyAction::SetDefault,
        4 => ForeignKeyAction::Cascade,
        _ => ForeignKeyAction::NoAction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Parser;

    fn parse_table(sql: &str) -> (String, Vec<ColumnDef>, Vec<TableConstraint>, bool, bool) {
        let stmt = crate::sql::parse(sql).unwrap();
        match stmt {
            crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
                name, columns, constraints, without_rowid, strict, ..
            }) => (name.name, columns, constraints, without_rowid, strict),
            _ => panic!("not a CREATE TABLE"),
        }
    }

    #[test]
    fn build_simple_table() {
        let (name, cols, cons, wo, st) = parse_table(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE)",
        );
        let table = build_table(&name, &cols, &cons, 1, wo, st, "CREATE TABLE users (...)").unwrap();
        assert_eq!(table.name, "users");
        assert_eq!(table.columns.len(), 3);
        assert_eq!(table.columns[0].name, "id");
        assert!(table.columns[0].primary_key);
        assert_eq!(table.rowid_alias, Some(0));
        assert!(!table.columns[1].nullable);
        assert!(table.columns[2].unique);
    }

    #[test]
    fn build_table_with_composite_pk() {
        let (name, cols, cons, wo, st) = parse_table(
            "CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY(a, b))",
        );
        let table = build_table(&name, &cols, &cons, 1, wo, st, "").unwrap();
        assert!(table.columns[0].primary_key);
        assert!(table.columns[1].primary_key);
        // Composite PK is NOT a rowid alias.
        assert_eq!(table.rowid_alias, None);
    }
}
