//! Core types: SQL values, type affinities, rows.

pub mod text;
pub mod value;

pub use text::Text;
pub use value::{format_real, values_sql_equal, Affinity, GroupKey, Row, Value};
