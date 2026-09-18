//! Schema catalog: tables, columns, indexes, views.
//!
//! The catalog is itself stored as a special table (`sqlite_master` equivalent)
//! in the database. On open, we read the catalog into memory for fast lookup.

use crate::error::{Error, Result};
use crate::sql::ast::{
    ColumnConstraint, ColumnDef, ConflictResolution, ForeignKeyAction, IndexedColumn, Order,
    TableConstraint,
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
    /// PostgreSQL-borrowed DECIMAL(p,s) / NUMERIC(p,s) enforcement:
    /// when the declared type carries an explicit precision and scale,
    /// writes are ROUNDED to `s` decimal places (half away from zero,
    /// like PG's numeric). This is an intentional divergence from SQLite,
    /// which silently ignores the precision/scale (probe-pinned footgun:
    /// inserting 1.555 into DECIMAL(10,2) keeps 1.555). Scale 0 columns
    /// store INTEGER; otherwise REAL (f64 approximation, documented).
    pub decimal: Option<(u8, u8)>,
    pub nullable: bool,
    pub default: Option<crate::sql::ast::Expr>,
    pub primary_key: bool,
    pub primary_key_order: Order,
    /// 1-based position within the (possibly compound) PRIMARY KEY clause;
    /// 0 = not part of the PK. `PRAGMA table_info` reports this as `pk`.
    pub pk_seq: u8,
    /// True only for an explicit `NOT NULL` constraint (NOT a PK-implied
    /// one). `PRAGMA table_info.notnull` and
    /// `sqlite3_table_column_metadata` report THIS, matching SQLite's
    /// famous quirk: `id INTEGER PRIMARY KEY` reports notnull=0.
    pub explicit_not_null: bool,
    pub autoincrement: bool,
    pub unique: bool,
    pub collation: String,
    /// For generated columns: the expression and whether it is STORED.
    pub generated: Option<(crate::sql::ast::Expr, bool)>,
}

impl Column {
    /// Apply this column's affinity (and, for declared DECIMAL(p,s) /
    /// NUMERIC(p,s) columns, PostgreSQL-style scale rounding) to a value.
    pub fn coerce(&self, v: Value) -> Value {
        coerce_with(self.affinity, self.decimal, v)
    }
}

/// Affinity + optional DECIMAL(p,s) scale enforcement — the write-path
/// coercion for callers that hold the pieces separately (e.g. the INSERT
/// chain fast path, which pre-extracts per-column affinities). This is
/// the exact equivalent of [`Column::coerce`].
pub fn coerce_with(aff: Affinity, decimal: Option<(u8, u8)>, v: Value) -> Value {
    let v = aff.coerce(v);
    match (decimal, &v) {
        (Some((_, s)), Value::Real(f)) => Value::Real(round_scale(*f, s)),
        (Some((_, 0)), Value::Integer(_)) => v,
        _ => v,
    }
}

/// Round to `s` decimal places, half away from zero (PG numeric rule;
/// Rust's f64::round already rounds halves away from zero).
fn round_scale(f: f64, s: u8) -> f64 {
    let m = 10f64.powi(s as i32);
    // guard the multiply against overflow to inf
    if f.is_finite() && (f * m).is_finite() {
        (f * m).round() / m
    } else {
        f
    }
}

/// Parse `DECIMAL(p,s)` / `NUMERIC(p,s)` / `DEC(p,s)` / `DECIMAL(p)` from
/// a declared type. Returns (precision, scale); scale defaults to 0 like
/// PostgreSQL. Plain `DECIMAL` (no parens) is NOT enforced (SQLite
/// semantics — nothing to round to).
pub fn parse_decimal_spec(declared: &str) -> Option<(u8, u8)> {
    let d = declared.trim().to_ascii_uppercase();
    let base = d
        .strip_prefix("DECIMAL")
        .or_else(|| d.strip_prefix("NUMERIC"))
        .or_else(|| d.strip_prefix("DEC"))
        .or_else(|| d.strip_prefix("FIXED"))?;
    let base = base.trim_start();
    let rest = base.strip_prefix('(')?;
    let inner = rest.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    let p: u8 = parts.first()?.parse().ok()?;
    let s: u8 = match parts.get(1) {
        Some(sp) => sp.parse().ok()?,
        None => 0,
    };
    if s > p {
        return None; // PG rejects s > p; treat as unenforced
    }
    Some((p, s))
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
    /// Cached column affinities, parallel to `col_names` — the comparison
    /// affinity rules (a column operand lends its affinity to a
    /// literal/param operand: `text_col > 0` compares TEXT-to-TEXT, and
    /// `num_col > '5'` converts '5' to 5) resolve through this.
    pub col_affinities: std::sync::Arc<[crate::types::Affinity]>,
    /// Virtual-table instance when this is a `CREATE VIRTUAL TABLE` entry
    /// (root_page is 0 — there is no B+tree; all access goes through the
    /// module callbacks).
    pub vtab: Option<std::sync::Arc<crate::plugin::vtab::VtabInstance>>,
}

impl Table {
    /// Rebuild the `col_names` / `qualified_col_names` / `col_affinities`
    /// caches after a structural change (used by the vtab schema bridge).
    pub fn rebuild_name_caches(&mut self) {
        self.col_names = self
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<String>>()
            .into();
        self.qualified_col_names = self
            .columns
            .iter()
            .map(|c| format!("{}.{}", self.name, c.name))
            .collect::<Vec<String>>()
            .into();
        self.col_affinities = self
            .columns
            .iter()
            .map(|c| c.affinity)
            .collect::<Vec<crate::types::Affinity>>()
            .into();
    }

    /// Look up a column by name (case-insensitive). Returns its index.
    pub fn find_column(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
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
    /// Where this index came from — filters the engine-internal WITHOUT
    /// ROWID PK index (uniqueness backing for the internal rowid-table
    /// storage; never dumped to the SQLite-format file, where the
    /// table b-tree IS the PK index, and never listed in sqlite_master).
    pub origin: IndexOrigin,
    /// Access method (PostgreSQL's `USING` clause, borrowed):
    /// `btree` (default) keys whole column values; `gin` stores one
    /// (lexeme, rowid) entry PER TERM of the indexed tsvector
    /// expression; `gist` (spatial) stores one (level, cell-x, cell-y,
    /// rowid) entry per grid cell covered by the geometry's bounding
    /// box. GIN/GIST indexes are NEVER unique.
    pub kind: IndexKind,
}

/// The index access method family (see `Index::kind`).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum IndexKind {
    /// The ordinary B+tree over whole column/expression values.
    #[default]
    Btree,
    /// PostgreSQL-GIN-style inverted index over a tsvector: the indexed
    /// expression (or column) must evaluate to tsvector text; each
    /// lexeme becomes one key with the row's rowid.
    Inverted,
    /// GiST-borrowed spatial grid over a geometry column: entries are
    /// the grid cells covered by each geometry's bounding box, at a
    /// per-geometry zoom level (coarsened for huge bboxes). The
    /// resolution is the cell edge in coordinate units at level 0.
    Spatial {
        /// Cell edge length at level 0 (level j cells are
        /// `resolution * 2^j`). Default 0.01 (≈1.1 km for lon/lat).
        resolution: f64,
    },
}

/// Derive the [`IndexKind`] from a parsed `CREATE INDEX` statement's
/// `USING` clause + column list — shared by CREATE-time execution and
/// the schema-load path (which re-parses the persisted `create_sql`).
/// Validates the specialized kinds' shape; special AMs are never unique.
pub fn index_kind_from_using(
    using: Option<&crate::sql::ast::IndexMethod>,
    columns: &[crate::sql::ast::IndexedColumn],
    unique: bool,
) -> Result<IndexKind> {
    let method = match using {
        None => return Ok(IndexKind::Btree),
        Some(m) => m,
    };
    match method {
        crate::sql::ast::IndexMethod::Gin => {
            if unique {
                return Err(crate::error::Error::semantic(
                    "UNIQUE is not supported for gin indexes",
                ));
            }
            if columns.len() != 1 {
                return Err(crate::error::Error::semantic(
                    "gin index takes exactly one indexed expression",
                ));
            }
            // The key must be a tsvector-valued expression:
            // `to_tsvector(...)` or a plain column holding tsvector text.
            let is_tsvector_expr = columns[0].expr.as_ref().map_or(true, |e| {
                matches!(&**e, crate::sql::ast::Expr::Function { ref name, .. }
                    if name.eq_ignore_ascii_case("to_tsvector"))
            });
            if !is_tsvector_expr {
                return Err(crate::error::Error::semantic(
                    "gin index requires to_tsvector(...) or a tsvector column",
                ));
            }
            Ok(IndexKind::Inverted)
        }
        crate::sql::ast::IndexMethod::Gist => {
            if unique {
                return Err(crate::error::Error::semantic(
                    "UNIQUE is not supported for gist/spatial indexes",
                ));
            }
            if columns.is_empty() || columns.len() > 2 {
                return Err(crate::error::Error::semantic(
                    "gist index takes a geometry column and an optional resolution",
                ));
            }
            let resolution = if columns.len() == 2 {
                match columns[1].expr.as_deref() {
                    Some(crate::sql::ast::Expr::Literal(crate::types::Value::Real(r)))
                        if *r > 0.0 && r.is_finite() =>
                    {
                        *r
                    }
                    Some(crate::sql::ast::Expr::Literal(crate::types::Value::Integer(i)))
                        if *i > 0 =>
                    {
                        *i as f64
                    }
                    _ => {
                        return Err(crate::error::Error::semantic(
                            "gist resolution must be a positive number literal",
                        ));
                    }
                }
            } else {
                0.01
            };
            Ok(IndexKind::Spatial { resolution })
        }
    }
}

/// Provenance of a catalog index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IndexOrigin {
    /// A `CREATE INDEX` statement (explicit, listed in sqlite_master).
    #[default]
    CreateIndex,
    /// An implicit `sqlite_autoindex_<table>_<n>` of a ROWID table's
    /// UNIQUE / non-alias PK constraint (listed in sqlite_master with
    /// NULL sql, real in the file format).
    Autoindex,
    /// The ENGINE-INTERNAL PK index of a WITHOUT ROWID table: the
    /// internal storage is a rowid table, so the PK's uniqueness needs
    /// an index backing it — but SQLite files never carry one (the
    /// table b-tree is keyed by the PK there). Hidden from sqlite_master
    /// and every file-format dump; rebuilt from the table DDL on reopen.
    WithoutRowidPk,
}

#[derive(Clone, Debug)]
pub struct IndexColumn {
    pub name: String,
    pub order: Order,
    pub collation: String,
    /// Expression index key (SQLite 3.9+): when set, the key value is
    /// evaluated from this expression per row instead of reading the
    /// named column. `name` holds the rendered expression text.
    pub expr: Option<crate::sql::ast::Expr>,
}

/// The WITHOUT ROWID table's PRIMARY KEY columns in PK order, as
/// indexed columns (collation + order carried) — the column set of the
/// engine-internal PK uniqueness index (`IndexOrigin::WithoutRowidPk`).
pub fn without_rowid_pk_columns(table: &Table) -> Vec<crate::sql::ast::IndexedColumn> {
    let mut pk: Vec<(u8, usize)> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.pk_seq > 0)
        .map(|(i, c)| (c.pk_seq, i))
        .collect();
    pk.sort_by_key(|(seq, _)| *seq);
    pk.into_iter()
        .map(|(_, i)| {
            let c = &table.columns[i];
            crate::sql::ast::IndexedColumn {
                name: c.name.clone(),
                order: c.primary_key_order,
                collation: if c.collation.is_empty() {
                    None
                } else {
                    Some(c.collation.clone())
                },
                expr: None,
            }
        })
        .collect()
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
#[derive(Debug)]
pub struct Trigger {
    pub name: String,
    pub table: String,
    pub when: crate::sql::ast::TriggerWhen,
    pub events: Vec<crate::sql::ast::TriggerEvent>,
    pub for_each_row: bool,
    pub when_clause: Option<crate::sql::ast::Expr>,
    pub body: Vec<crate::sql::ast::Statement>,
    pub create_sql: String,
    /// First-fire validation gate (SQLite validates trigger bodies
    /// lazily — at FIRE time, not CREATE time): flipped once the WHEN
    /// clause and body statements have passed prepare-time name
    /// resolution with NEW/OLD in scope. DDL replaces the whole Arc,
    /// so the flag can never go stale against the schema it checked.
    pub validated: std::sync::atomic::AtomicBool,
}

impl Clone for Trigger {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            table: self.table.clone(),
            when: self.when,
            events: self.events.clone(),
            for_each_row: self.for_each_row,
            when_clause: self.when_clause.clone(),
            body: self.body.clone(),
            create_sql: self.create_sql.clone(),
            // A clone is a fresh registration: re-validate at first fire.
            validated: std::sync::atomic::AtomicBool::new(
                self.validated.load(std::sync::atomic::Ordering::Acquire),
            ),
        }
    }
}

/// The in-memory catalog: maps names to tables, indexes, views, triggers.
#[derive(Default)]
pub struct Catalog {
    tables: HashMap<String, Arc<Table>>,
    indexes: HashMap<String, Arc<Index>>,
    views: HashMap<String, Arc<View>>,
    triggers: HashMap<String, Arc<Trigger>>,
    /// Table creation sequence (name-lc -> order of addition). The
    /// HashMap above iterates in arbitrary hash order; this preserves
    /// DDL order for anything whose observable behavior depends on it
    /// (SQLite's FK actions fire in REVERSE declaration order — pinned
    /// by the preupdate differential suite).
    table_seq: HashMap<String, u64>,
    next_table_seq: u64,
    /// Indexes grouped by table name (for fast lookup during query planning).
    indexes_by_table: HashMap<String, Vec<Arc<Index>>>,
    /// Triggers grouped by table name.
    triggers_by_table: HashMap<String, Vec<Arc<Trigger>>>,
    /// Schema cookie — bumped whenever the schema changes.
    pub schema_cookie: u32,
    /// `sqlite_stat1` rows, keyed by index name (lowercase) — the
    /// planner's cost model. Empty until `ANALYZE` runs (or a
    /// stat1-bearing database opens). Written by ANALYZE's refresh and
    /// by `load_schema`'s stat-table walk; read on every indexed lookup
    /// plan decision.
    stat1: HashMap<String, IndexStats>,
    /// TEMP-object names (lowercased) — `CREATE TEMP TABLE/INDEX/VIEW/
    /// TRIGGER` (and indexes/triggers on temp tables). Connection-scoped
    /// exactly like SQLite's temp schema: fully usable this session,
    /// NEVER persisted to the schema b-tree or any dump/image, gone
    /// after close/reopen. Kept as a side registry (not a struct field)
    /// so `load_schema` needs no changes — persisted rows are never temp.
    temp_objects: std::collections::HashSet<String>,
}

/// One `sqlite_stat1` row: the planner's estimate inputs for an index.
/// `stat` text parses as `"rows D1 D2 …"` — `rows` is the OWNING table's
/// row count, `Dk` the distinct-value count over the index's first `k`
/// columns (SQLite's exact format).
#[derive(Clone, Debug, Default)]
pub struct IndexStats {
    /// Owning table's row count at ANALYZE time.
    pub rows: i64,
    /// `D1..Dk` — distinct counts for the index's column prefixes.
    /// Empty when the stat row carried only the table row count.
    pub distinct_prefix: Vec<i64>,
}

impl IndexStats {
    /// Parse the `stat` column text: `"N"` or `"N D1 D2 …"`.
    pub fn parse(stat: &str) -> Self {
        let mut it = stat.split_whitespace();
        let rows = it.next().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let distinct_prefix = it.filter_map(|s| s.parse::<i64>().ok()).collect::<Vec<_>>();
        Self {
            rows,
            distinct_prefix,
        }
    }

    /// SQLite's row-estimate model for an equality lookup over the first
    /// `k` bound columns: `rows / (D1 × … × Dk)`, floored at 1. Missing
    /// stats degrade to 0 (caller treats unknown as "no estimate").
    pub fn estimate_eq(&self, k: usize) -> i64 {
        if self.rows <= 0 {
            return 0;
        }
        let mut est = self.rows as f64;
        for d in self.distinct_prefix.iter().take(k) {
            if *d <= 0 {
                return 1;
            }
            est /= *d as f64;
        }
        est.max(1.0) as i64
    }

    /// Render back to the `stat` column text.
    pub fn to_stat_text(&self) -> String {
        let mut s = self.rows.to_string();
        for d in &self.distinct_prefix {
            s.push(' ');
            s.push_str(&d.to_string());
        }
        s
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Statistics for one index (from the in-memory stat1 map). `None`
    /// when ANALYZE has not covered this index.
    pub fn index_stats(&self, index_name: &str) -> Option<IndexStats> {
        self.stat1.get(&index_name.to_ascii_lowercase()).cloned()
    }

    /// Replace the whole in-memory stat map (ANALYZE's refresh path).
    pub fn set_stat1(&mut self, rows: Vec<(String, IndexStats)>) {
        self.stat1.clear();
        for (name, s) in rows {
            self.stat1.insert(name.to_ascii_lowercase(), s);
        }
    }

    /// Current stat rows (index name, stats) — the targeted-ANALYZE merge
    /// path reads the surviving rows before replacing the map.
    pub fn stat1_snapshot(&self) -> Vec<(String, IndexStats)> {
        self.stat1
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Drop all stats (schema invalidation, e.g. a table dropped).
    pub fn clear_stat1(&mut self) {
        self.stat1.clear();
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
        self.tables
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// All indexes (name, index).
    pub fn all_indexes(&self) -> Vec<(String, Arc<Index>)> {
        self.indexes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// All views (name, view).
    pub fn all_views(&self) -> Vec<(String, Arc<View>)> {
        self.views
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// All triggers (name, trigger).
    pub fn all_triggers(&self) -> Vec<(String, Arc<Trigger>)> {
        self.triggers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Replace an index entry in place (ALTER TABLE column rewrites).
    /// The per-table index list is refreshed too — the planner resolves
    /// indexes through it.
    pub fn replace_index(&mut self, name: &str, index: Index) -> Option<()> {
        let key = name.to_ascii_lowercase();
        if !self.indexes.contains_key(&key) {
            return None;
        }
        let arc = Arc::new(index);
        // Refresh indexes_by_table (holds its own Arc of the old entry).
        let tbl_key = arc.table.to_ascii_lowercase();
        if let Some(list) = self.indexes_by_table.get_mut(&tbl_key) {
            for slot in list.iter_mut() {
                if slot.name.eq_ignore_ascii_case(&arc.name) {
                    *slot = Arc::clone(&arc);
                }
            }
        }
        self.indexes.insert(key, arc);
        Some(())
    }

    /// Replace a view entry in place (ALTER TABLE column rewrites).
    pub fn replace_view(&mut self, name: &str, view: View) -> Option<()> {
        let key = name.to_ascii_lowercase();
        if !self.views.contains_key(&key) {
            return None;
        }
        self.views.insert(key, Arc::new(view));
        Some(())
    }

    /// Replace a trigger entry in place (ALTER TABLE column rewrites).
    /// The per-table trigger list is refreshed too — trigger firing
    /// resolves through it.
    pub fn replace_trigger(&mut self, name: &str, trigger: Trigger) -> Option<()> {
        let key = name.to_ascii_lowercase();
        if !self.triggers.contains_key(&key) {
            return None;
        }
        let arc = Arc::new(trigger);
        let tbl_key = arc.table.to_ascii_lowercase();
        if let Some(list) = self.triggers_by_table.get_mut(&tbl_key) {
            for slot in list.iter_mut() {
                if slot.name.eq_ignore_ascii_case(&arc.name) {
                    *slot = Arc::clone(&arc);
                }
            }
        }
        self.triggers.insert(key, arc);
        Some(())
    }

    pub fn get_view(&self, name: &str) -> Option<Arc<View>> {
        self.views.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn get_trigger(&self, name: &str) -> Option<Arc<Trigger>> {
        self.triggers.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn indexes_on_table(&self, table: &str) -> Vec<Arc<Index>> {
        // Fast path: the name is already lowercase (the common case) — the
        // map key is stored lowercase, so probe the borrowed name first.
        // A miss on an all-lowercase name is FINAL (no allocation for the
        // lowercase copy); only mixed-case names pay the conversion.
        // This lookup runs on every DML statement.
        if let Some(v) = self.indexes_by_table.get(table) {
            return v.clone();
        }
        if table.bytes().any(|b| b.is_ascii_uppercase()) {
            return self
                .indexes_by_table
                .get(&table.to_ascii_lowercase())
                .cloned()
                .unwrap_or_default();
        }
        Vec::new()
    }

    pub fn triggers_on_table(&self, table: &str) -> Vec<Arc<Trigger>> {
        // Same borrowed-name fast path as `indexes_on_table`.
        if let Some(v) = self.triggers_by_table.get(table) {
            return v.clone();
        }
        if table.bytes().any(|b| b.is_ascii_uppercase()) {
            return self
                .triggers_by_table
                .get(&table.to_ascii_lowercase())
                .cloned()
                .unwrap_or_default();
        }
        Vec::new()
    }

    pub fn add_table(&mut self, table: Table) {
        let key = table.name.to_ascii_lowercase();
        let idx_key = table.name.to_ascii_lowercase();
        let table_arc = Arc::new(table);
        self.next_table_seq = self.next_table_seq.wrapping_add(1);
        self.table_seq.insert(key.clone(), self.next_table_seq);
        // IndexesByTable entry for the table (will be populated by add_index).
        self.indexes_by_table.entry(idx_key).or_default();
        self.tables.insert(key, table_arc);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    /// Creation sequence of a table (0 = unknown). DDL order.
    pub fn table_creation_seq(&self, name_lc: &str) -> u64 {
        self.table_seq.get(name_lc).copied().unwrap_or(0)
    }

    pub fn add_index(&mut self, index: Index) {
        let key = index.name.to_ascii_lowercase();
        let table_key = index.table.to_ascii_lowercase();
        let idx_arc = Arc::new(index);
        self.indexes_by_table
            .entry(table_key)
            .or_default()
            .push(idx_arc.clone());
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
        self.triggers_by_table
            .entry(table_key)
            .or_default()
            .push(trig_arc.clone());
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
        // TEMP scope capture BEFORE the moves: autoindexes get NEW names
        // on rename (sqlite_autoindex_<table>_<n>), so old marks are
        // collected now and re-keyed to the moved entries afterwards.
        let was_temp = self.temp_objects.remove(&old_key);
        let temp_moved_idx: Vec<String> = self
            .indexes_by_table
            .get(&old_key)
            .map(|l| {
                l.iter()
                    .map(|i| i.name.to_ascii_lowercase())
                    .filter(|n| self.temp_objects.remove(n))
                    .collect()
            })
            .unwrap_or_default();
        let temp_moved_trig: Vec<String> = self
            .triggers_by_table
            .get(&old_key)
            .map(|l| {
                l.iter()
                    .map(|t| t.name.to_ascii_lowercase())
                    .filter(|n| self.temp_objects.remove(n))
                    .collect()
            })
            .unwrap_or_default();
        // Move index registrations.
        if let Some(idx_list) = self.indexes_by_table.remove(&old_key) {
            for idx in &idx_list {
                self.indexes.remove(&idx.name.to_ascii_lowercase());
                // Re-key the index's table field by cloning with the new
                // table name. Implicit autoindexes also carry the table
                // name IN their name (sqlite_autoindex_<table>_<n>) —
                // follow the rename (SQLite does).
                let mut i2 = (**idx).clone();
                i2.table = new_name.to_string();
                let old_prefix = format!("sqlite_autoindex_{}_", old_name);
                if idx.name.starts_with(&old_prefix) {
                    i2.name = format!(
                        "sqlite_autoindex_{}_{}",
                        new_name,
                        &idx.name[old_prefix.len()..]
                    );
                }
                let i2 = Arc::new(i2);
                self.indexes
                    .insert(i2.name.to_ascii_lowercase(), i2.clone());
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
                self.triggers
                    .insert(t2.name.to_ascii_lowercase(), t2.clone());
                self.triggers_by_table
                    .entry(new_key.clone())
                    .or_default()
                    .push(t2);
            }
        }
        // Re-key the TEMP marks under the new names.
        if was_temp {
            self.temp_objects.insert(new_key.clone());
            // Every index on a temp table is temp (create-time invariant:
            // autoindexes are marked at CREATE, explicit indexes follow
            // the table's scope) — mark by the MOVED entries' new names.
            if let Some(l) = self.indexes_by_table.get(&new_key) {
                for i in l {
                    self.temp_objects.insert(i.name.to_ascii_lowercase());
                }
            }
        } else {
            // A table rename doesn't change explicit index names —
            // restore the individually-temp-marked ones (CREATE TEMP
            // INDEX ON <permanent table> keeps its scope).
            for n in temp_moved_idx {
                self.temp_objects.insert(n);
            }
        }
        for n in temp_moved_trig {
            // Trigger names never change on table rename.
            self.temp_objects.insert(n);
        }
        self.tables.insert(new_key, t);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(())
    }

    /// Mark an object as TEMP: connection-scoped, never persisted.
    /// Called by the CREATE paths for `CREATE TEMP ...` (and for objects
    /// that follow a temp table's scope — indexes, triggers).
    pub fn mark_temp(&mut self, name: &str) {
        self.temp_objects.insert(name.to_ascii_lowercase());
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
    }

    /// Is this object TEMP (connection-scoped, not persisted)?
    pub fn is_temp(&self, name: &str) -> bool {
        self.temp_objects.contains(&name.to_ascii_lowercase())
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
        self.temp_objects.remove(&key);
        // Remove all indexes on this table.
        if let Some(idx_list) = self.indexes_by_table.remove(&key) {
            for idx in idx_list {
                self.indexes.remove(&idx.name.to_ascii_lowercase());
                self.temp_objects.remove(&idx.name.to_ascii_lowercase());
            }
        }
        // Remove triggers on this table.
        if let Some(trig_list) = self.triggers_by_table.remove(&key) {
            for trig in trig_list {
                self.triggers.remove(&trig.name.to_ascii_lowercase());
                self.temp_objects.remove(&trig.name.to_ascii_lowercase());
            }
        }
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(t)
    }

    pub fn drop_index(&mut self, name: &str) -> Option<Arc<Index>> {
        let key = name.to_ascii_lowercase();
        let idx = self.indexes.remove(&key)?;
        self.temp_objects.remove(&key);
        if let Some(list) = self
            .indexes_by_table
            .get_mut(&idx.table.to_ascii_lowercase())
        {
            list.retain(|i| i.name.to_ascii_lowercase() != key);
        }
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(idx)
    }

    pub fn drop_view(&mut self, name: &str) -> Option<Arc<View>> {
        let key = name.to_ascii_lowercase();
        let v = self.views.remove(&key)?;
        self.temp_objects.remove(&key);
        self.schema_cookie = self.schema_cookie.wrapping_add(1);
        Some(v)
    }

    pub fn drop_trigger(&mut self, name: &str) -> Option<Arc<Trigger>> {
        let key = name.to_ascii_lowercase();
        let t = self.triggers.remove(&key)?;
        self.temp_objects.remove(&key);
        if let Some(list) = self
            .triggers_by_table
            .get_mut(&t.table.to_ascii_lowercase())
        {
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
        // STRICT tables: the declared type must be exactly one of SQLite's
        // six strict names (no affinity coercion rules apply inside them).
        // Generated columns are exempt (their type is the expression's).
        if strict
            && !col
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::GeneratedAs { .. }))
        {
            let t = col.type_name.trim().to_ascii_uppercase();
            let ok = matches!(
                t.as_str(),
                "INT" | "INTEGER" | "TEXT" | "REAL" | "BLOB" | "ANY"
            );
            if !ok {
                return Err(Error::semantic(format!(
                    "unknown datatype for {}.{}: \"{}\"",
                    name, col.name, col.type_name
                )));
            }
        }
        let affinity = if col.type_name.is_empty() {
            Affinity::Blob
        } else {
            Affinity::from_declared_type(&col.type_name)
        };
        let decimal = parse_decimal_spec(&col.type_name);
        let mut nullable = true;
        let mut primary_key = false;
        let mut primary_key_order = Order::Asc;
        let mut autoincrement = false;
        let mut unique = false;
        let mut default = None;
        let mut collation = "BINARY".to_string();
        let mut generated = None;
        let mut explicit_not_null = false;

        for constraint in &col.constraints {
            match constraint {
                ColumnConstraint::PrimaryKey {
                    autoincrement: ai,
                    order,
                } => {
                    primary_key = true;
                    autoincrement = *ai;
                    primary_key_order = *order;
                    nullable = false;
                    // INTEGER PRIMARY KEY is a rowid alias — but ONLY with
                    // the exact declared type "INTEGER" (SQLite: "INT
                    // PRIMARY KEY" is NOT an alias, fileformat2 §2.6.1)
                    // and ONLY ascending ("INTEGER PRIMARY KEY DESC" is a
                    // real column with an implicit autoindex).
                    if affinity == Affinity::Integer
                        && col.type_name.trim().eq_ignore_ascii_case("INTEGER")
                        && *order == Order::Asc
                    {
                        rowid_alias = Some(i);
                    }
                }
                ColumnConstraint::NotNull => {
                    nullable = false;
                    explicit_not_null = true;
                }
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
            decimal,
            nullable,
            default,
            primary_key,
            primary_key_order,
            pk_seq: u8::from(primary_key),
            explicit_not_null,
            autoincrement,
            unique,
            collation,
            generated,
        });
    }

    // Handle table-level PRIMARY KEY: mark columns as PK.
    if !table_pk.is_empty() {
        for (seq, ic) in table_pk.iter().enumerate() {
            if let Some(idx) = table_columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(&ic.name))
            {
                table_columns[idx].primary_key = true;
                table_columns[idx].primary_key_order = ic.order;
                table_columns[idx].nullable = false;
                table_columns[idx].pk_seq = (seq + 1) as u8;
                // If a single-column table-level PRIMARY KEY names a column
                // declared exactly "INTEGER" (case-insensitive, trimmed),
                // it is also a rowid alias — SQLite build.c
                // sqlite3AddPrimaryKey. NOTE: the type must be exactly
                // "INTEGER" ("INT", "TINYINT", "UNSIGNED INTEGER" are NOT
                // aliases — affinity alone is not enough), and unlike the
                // column-level form, a DESC member does NOT block the
                // alias here (`PRIMARY KEY(x DESC)` still aliases x).
                // Pinned against real SQLite.
                if table_pk.len() == 1
                    && table_columns[idx]
                        .declared_type
                        .trim()
                        .eq_ignore_ascii_case("INTEGER")
                {
                    rowid_alias = Some(idx);
                }
            }
        }
    }

    // Mark UNIQUE columns from table-level UNIQUE constraints.
    for c in constraints {
        if let TableConstraint::Unique(cols) = c {
            for ic in cols {
                if let Some(idx) = table_columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&ic.name))
                {
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
            if let ColumnConstraint::References {
                table: rt,
                columns: rc,
                on_delete,
                on_update,
            } = constraint
            {
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
        if let TableConstraint::ForeignKey {
            columns: cols,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        } = c
        {
            let mut child_idx = Vec::with_capacity(cols.len());
            for cn in cols {
                match table_columns
                    .iter()
                    .position(|tc| tc.name.eq_ignore_ascii_case(cn))
                {
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
    let table_columns_affinities: Vec<crate::types::Affinity> =
        table_columns.iter().map(|c| c.affinity).collect();
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
        col_affinities: table_columns_affinities.into(),
        vtab: None,
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
        // Expression index: no column resolution — the key comes from the
        // expression. Collation may still be explicit (BINARY default).
        if let Some(e) = &c.expr {
            out.push(IndexColumn {
                name: c.name.clone(),
                order: c.order,
                collation: c.collation.clone().unwrap_or_else(|| "BINARY".to_string()),
                expr: Some((**e).clone()),
            });
            continue;
        }
        if table.find_column(&c.name).is_none() {
            return Err(Error::semantic(format!(
                "column {} not found in table {}",
                c.name, table.name
            )));
        }
        // Collation precedence (SQLite): an explicit COLLATE in the index
        // spec wins; otherwise the index INHERITS the table column's
        // declared collation — `CREATE INDEX ON t(email)` on a
        // `email TEXT COLLATE NOCASE` column is a NOCASE index.
        let inherited = table
            .find_column(&c.name)
            .and_then(|i| table.columns.get(i))
            .map(|col| col.collation.clone())
            .unwrap_or_else(|| "BINARY".to_string());
        out.push(IndexColumn {
            name: c.name.clone(),
            order: c.order,
            collation: c.collation.clone().unwrap_or(inherited),
            expr: None,
        });
    }
    Ok(out)
}

/// Encode a catalog entry as a row in the schema table (`sqlite_master`).
/// Columns: (type, name, tbl_name, rootpage, sql).
///
/// `rootpage` takes the ENGINE-INTERNAL page id (0-based: page 0 is the
/// schema b-tree itself) and is translated to SQLite's file convention
/// (1-based: page 1 is the schema b-tree, so every real b-tree root is
/// +1). Views and triggers carry no b-tree: internal 0 stays 0, exactly
/// SQLite's `rootpage = 0` for them.
pub fn encode_schema_row(
    kind: &str,
    name: &str,
    tbl_name: &str,
    rootpage: PageId,
    sql: &str,
) -> Vec<Value> {
    encode_schema_row_opt(kind, name, tbl_name, rootpage, Some(sql))
}

/// [`encode_schema_row`] with a NULL `sql` column. SQLite stores NULL sql
/// for auto-indexes (`sqlite_autoindex_*`) — tools that dump
/// `SELECT sql FROM sqlite_master` and re-apply it must not see (and
/// re-create) the implicit indexes. The reopen path rebuilds them from
/// the TABLE's DDL instead (see `load_schema`).
pub fn encode_schema_row_opt(
    kind: &str,
    name: &str,
    tbl_name: &str,
    rootpage: PageId,
    sql: Option<&str>,
) -> Vec<Value> {
    // Internal 0-based -> SQLite 1-based. Internal 0 is only ever the
    // schema b-tree itself (never a user object), so a 0 here means
    // "no b-tree" (view/trigger) and passes through untouched.
    let sqlite_rootpage: i64 = if rootpage >= 1 {
        rootpage as i64 + 1
    } else {
        0
    };
    vec![
        Value::Text(kind.to_string().into()),
        Value::Text(name.to_string().into()),
        Value::Text(tbl_name.to_string().into()),
        Value::Integer(sqlite_rootpage),
        match sql {
            Some(s) => Value::Text(s.to_string().into()),
            None => Value::Null,
        },
    ]
}

/// Inverse of [`encode_schema_row_opt`]'s translation: a rootpage read
/// back from a schema row (SQLite 1-based convention) to the engine's
/// internal 0-based page id. 0 (view/trigger) stays 0.
pub fn rootpage_to_internal(rootpage: u32) -> PageId {
    rootpage.saturating_sub(1)
}

/// The `sqlite_master` (aka `sqlite_schema`) table: a real, queryable view
/// over the schema B+tree at page 0. Columns match SQLite exactly:
/// (type, name, tbl_name, rootpage, sql).
///
/// This is what ORMs and tooling query for schema discovery:
/// `SELECT name, sql FROM sqlite_master WHERE type='table'`.
pub fn sqlite_master_table() -> Table {
    let cols = ["type", "name", "tbl_name", "rootpage", "sql"];
    let types = ["TEXT", "TEXT", "TEXT", "INTEGER", "TEXT"];
    let mut columns = Vec::with_capacity(5);
    for (c, t) in cols.iter().zip(types.iter()) {
        columns.push(Column {
            name: (*c).to_string(),
            affinity: crate::types::Affinity::from_declared_type(t),
            declared_type: (*t).to_string(),
            decimal: None, // TEXT/INTEGER schema columns: no decimal spec
            nullable: true,
            default: None,
            primary_key: false,
            primary_key_order: crate::sql::ast::Order::default(),
            pk_seq: 0,
            explicit_not_null: false,
            autoincrement: false,
            unique: false,
            collation: "BINARY".to_string(),
            generated: None,
        });
    }
    Table {
        name: "sqlite_master".to_string(),
        columns,
        root_page: 0,
        without_rowid: false,
        strict: false,
        rowid_alias: None,
        create_sql: "CREATE TABLE sqlite_master(type text, name text, tbl_name text, rootpage int, sql text)".to_string(),
        check_exprs: Vec::new(),
        foreign_keys: Vec::new(),
        col_names: std::sync::Arc::from(
            cols.iter().map(|c| c.to_string()).collect::<Vec<String>>(),
        ),
        qualified_col_names: std::sync::Arc::from(
            cols.iter()
                .map(|c| format!("sqlite_master.{}", c))
                .collect::<Vec<String>>(),
        ),
        col_affinities: std::sync::Arc::from(vec![
            crate::types::Affinity::Text,
            crate::types::Affinity::Text,
            crate::types::Affinity::Text,
            crate::types::Affinity::Integer,
            crate::types::Affinity::Text,
        ]),
        vtab: None,
    }
}

/// `sqlite_schema` — SQLite's alternate spelling of sqlite_master (same
/// B+tree, same columns). Registered alongside the master name.
pub fn sqlite_schema_table() -> Table {
    let mut t = sqlite_master_table();
    t.name = "sqlite_schema".to_string();
    t.qualified_col_names = std::sync::Arc::from(
        (0..t.columns.len())
            .map(|i| format!("sqlite_schema.{}", t.columns[i].name))
            .collect::<Vec<String>>(),
    );
    t
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

    fn parse_table(sql: &str) -> (String, Vec<ColumnDef>, Vec<TableConstraint>, bool, bool) {
        let stmt = crate::sql::parse(sql).unwrap();
        match stmt {
            crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
                name,
                columns,
                constraints,
                without_rowid,
                strict,
                ..
            }) => (name.name, columns, constraints, without_rowid, strict),
            _ => panic!("not a CREATE TABLE"),
        }
    }

    #[test]
    fn build_simple_table() {
        let (name, cols, cons, wo, st) = parse_table(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE)",
        );
        let table =
            build_table(&name, &cols, &cons, 1, wo, st, "CREATE TABLE users (...)").unwrap();
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
        let (name, cols, cons, wo, st) =
            parse_table("CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY(a, b))");
        let table = build_table(&name, &cols, &cons, 1, wo, st, "").unwrap();
        assert!(table.columns[0].primary_key);
        assert!(table.columns[1].primary_key);
        // Composite PK is NOT a rowid alias.
        assert_eq!(table.rowid_alias, None);
    }
}
