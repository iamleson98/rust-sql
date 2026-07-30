//! Core types: SQL values, type affinities, rows.

pub mod value;

pub use value::{format_real, Affinity, Row, Value};
