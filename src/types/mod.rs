//! Core types: SQL values, type affinities, rows.

pub mod value;

pub use value::{format_real, values_sql_equal, Affinity, GroupKey, Row, Value};
