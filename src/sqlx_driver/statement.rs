//! Statement, row, column, and query-result types for the native sqlx
//! driver.

use std::sync::Arc;

use sqlx_core::column::{Column, ColumnIndex};
use sqlx_core::error::Error as SqlxError;
use sqlx_core::row::{debug_row, Row};
use sqlx_core::sql_str::SqlStr;
use sqlx_core::statement::Statement;
use sqlx_core::HashMap;

use crate::sqlx_driver::types::{DataType, RustqliteTypeInfo};
use crate::sqlx_driver::{Rustqlite, RustqliteArguments, RustqliteValue, RustqliteValueRef};

// ---------------------------------------------------------------------------
// Query result
// ---------------------------------------------------------------------------

/// The result of a successful query execution.
#[derive(Debug, Default)]
pub struct RustqliteQueryResult {
    pub(crate) rows_affected: u64,
    pub(crate) last_insert_rowid: i64,
}

impl RustqliteQueryResult {
    /// Get the total number of rows affected.
    pub fn rows_affected(&self) -> u64 {
        self.rows_affected
    }

    /// Get the id of the last row that was inserted into the database
    /// (SQLite's `last_insert_rowid`).
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }
}

impl Extend<RustqliteQueryResult> for RustqliteQueryResult {
    fn extend<T: IntoIterator<Item = RustqliteQueryResult>>(&mut self, iter: T) {
        for elem in iter {
            self.rows_affected += elem.rows_affected;
            self.last_insert_rowid = elem.last_insert_rowid;
        }
    }
}

// ---------------------------------------------------------------------------
// Column
// ---------------------------------------------------------------------------

/// A single column of a rustqlite query result.
#[derive(Debug, Clone)]
pub struct RustqliteColumn {
    pub(crate) name: Arc<str>,
    pub(crate) ordinal: usize,
    pub(crate) type_info: RustqliteTypeInfo,
}

impl RustqliteColumn {
    pub(crate) fn new(name: &str, ordinal: usize, type_info: RustqliteTypeInfo) -> Self {
        Self {
            name: Arc::from(name),
            ordinal,
            type_info,
        }
    }
}

impl Column for RustqliteColumn {
    type Database = Rustqlite;

    fn ordinal(&self) -> usize {
        self.ordinal
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn type_info(&self) -> &RustqliteTypeInfo {
        &self.type_info
    }
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

/// A single row from the database.
///
/// All data is owned (cloned out of the engine statement) — rows are
/// freely `Send + Sync` and outlive the connection, with no unsafe.
pub struct RustqliteRow {
    pub(crate) values: Box<[RustqliteValue]>,
    pub(crate) columns: Arc<Vec<RustqliteColumn>>,
    pub(crate) column_names: Arc<HashMap<Arc<str>, usize>>,
}

impl RustqliteRow {
    pub(crate) fn new(
        values: Vec<crate::types::Value>,
        columns: &Arc<Vec<RustqliteColumn>>,
        column_names: &Arc<HashMap<Arc<str>, usize>>,
    ) -> Self {
        // Column type info refined by the first row's runtime types
        // (SQLite's sqlite3_column_type is per-value as well).
        let values: Box<[RustqliteValue]> = values
            .into_iter()
            .map(|v| RustqliteValue { value: v })
            .collect();
        Self {
            values,
            columns: Arc::clone(columns),
            column_names: Arc::clone(column_names),
        }
    }

    /// Number of columns.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True if the row has no columns.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl Row for RustqliteRow {
    type Database = Rustqlite;

    fn columns(&self) -> &[RustqliteColumn] {
        &self.columns
    }

    fn try_get_raw<I>(&self, index: I) -> Result<RustqliteValueRef<'_>, SqlxError>
    where
        I: ColumnIndex<Self>,
    {
        let index = index.index(self)?;
        Ok(RustqliteValueRef::value(&self.values[index]))
    }
}

impl ColumnIndex<RustqliteRow> for &'_ str {
    fn index(&self, row: &RustqliteRow) -> Result<usize, SqlxError> {
        row.column_names
            .get(*self)
            .ok_or_else(|| SqlxError::ColumnNotFound((*self).into()))
            .copied()
    }
}

// ---------------------------------------------------------------------------
// Statement
// ---------------------------------------------------------------------------

/// An explicitly prepared statement.
#[derive(Debug, Clone)]
pub struct RustqliteStatement {
    pub(crate) sql: SqlStr,
    pub(crate) param_count: usize,
    pub(crate) columns: Arc<Vec<RustqliteColumn>>,
    pub(crate) column_names: Arc<HashMap<Arc<str>, usize>>,
}

impl Statement for RustqliteStatement {
    type Database = Rustqlite;

    fn into_sql(self) -> SqlStr {
        self.sql
    }

    fn sql(&self) -> &SqlStr {
        &self.sql
    }

    fn parameters(&self) -> Option<sqlx_core::Either<&[RustqliteTypeInfo], usize>> {
        Some(sqlx_core::Either::Right(self.param_count))
    }

    fn columns(&self) -> &[RustqliteColumn] {
        &self.columns
    }

    sqlx_core::impl_statement_query!(RustqliteArguments);
}

impl ColumnIndex<RustqliteStatement> for &'_ str {
    fn index(&self, statement: &RustqliteStatement) -> Result<usize, SqlxError> {
        statement
            .column_names
            .get(*self)
            .ok_or_else(|| SqlxError::ColumnNotFound((*self).into()))
            .copied()
    }
}

impl std::fmt::Debug for RustqliteRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_row(self, f)
    }
}

// ---------------------------------------------------------------------------
// Shared column-metadata construction
// ---------------------------------------------------------------------------

/// Shared per-statement column list.
pub(crate) type Columns = Arc<Vec<RustqliteColumn>>;
/// Shared column name → ordinal map.
pub(crate) type NameMap = Arc<HashMap<Arc<str>, usize>>;
/// Both halves of a statement's column metadata.
pub(crate) type ColumnMeta = (Columns, NameMap);

/// Build `(columns, name_map)` from a set of column names.
pub(crate) fn columns_from_names(names: &[String]) -> ColumnMeta {
    let mut columns = Vec::with_capacity(names.len());
    let mut map = HashMap::with_capacity(names.len());
    for (ordinal, name) in names.iter().enumerate() {
        map.insert(Arc::from(name.as_str()), ordinal);
        columns.push(RustqliteColumn::new(
            name,
            ordinal,
            // SQLite reports NULL-typed columns before the first step;
            // runtime typing comes from the row values themselves.
            RustqliteTypeInfo(DataType::Null),
        ));
    }
    (Arc::new(columns), Arc::new(map))
}
