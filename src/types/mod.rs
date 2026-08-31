//! Core types: SQL values, type affinities, rows.

pub mod text;
pub mod value;

pub use text::Text;
pub use value::{format_real, values_sql_equal, Affinity, GroupKey, Row, Value};

use std::ops::Range;
use std::sync::Arc;

/// A materialized CTE: (rows, column names). CTE bodies are executed once
/// up front and handed to the planner/executor as plain row sets; the
/// column names are qualified (`cte.col`) for name resolution.
pub type CteMaterialization = (Arc<Vec<Row>>, Arc<[String]>);

/// A bare-column projection mapping produced by the planner: which source
/// columns feed the projection (`None` = all, in declared order) plus the
/// output column names.
pub type ProjectionMapping = (Option<Vec<usize>>, Arc<[String]>);

/// One in-place cell payload update during index maintenance:
/// (rowid, byte range in the index key, replacement bytes).
pub type CellUpdate = (i64, Range<usize>, Option<Vec<u8>>);

