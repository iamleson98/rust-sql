//! Logical plan: the IR between the parser/AST and the executor.
//!
//! Plans are tree-shaped. Each node is a relational operator. The executor
//! walks the tree, pulling rows from children in a Volcano-style iterator
//! model.

use crate::sql::ast::{Expr, OrderTerm};
use std::sync::Arc;

use crate::schema::{Index, Table};

/// A logical plan node.
#[derive(Clone, Debug)]
pub enum Plan {
    /// Scan a table. If `index` is Some, scan via that index.
    Scan {
        table: Arc<Table>,
        alias: Option<String>,
        index: Option<Arc<Index>>,
        /// Optional predicate that can be evaluated during the scan (pushed-down).
        predicate: Option<Expr>,
    },
    /// A point lookup via the table's rowid (used for `WHERE rowid = ?`).
    RowidLookup {
        table: Arc<Table>,
        alias: Option<String>,
        rowid: Expr,
    },
    /// A point lookup via a secondary index (used for `WHERE indexed_col = ?`).
    /// Returns matching rows from the table by rowid.
    IndexLookup {
        table: Arc<Table>,
        alias: Option<String>,
        index: Arc<Index>,
        /// The encoded key to look up.
        key_exprs: Vec<Expr>,
    },
    /// A constant single-row source. Columns are filled with the given expressions.
    /// Used for `SELECT 1+1` without a FROM clause.
    Values { rows: Vec<Vec<Expr>> },
    /// Filter rows by a predicate.
    Filter { input: Box<Plan>, predicate: Expr },
    /// Project columns.
    Project { input: Box<Plan>, columns: Vec<ProjectExpr> },
    /// Sort rows.
    Sort { input: Box<Plan>, terms: Vec<OrderTerm> },
    /// Limit rows.
    Limit { input: Box<Plan>, count: Expr, offset: Expr },
    /// Aggregate rows (with optional grouping).
    Aggregate {
        input: Box<Plan>,
        group_by: Vec<Expr>,
        aggregates: Vec<AggExpr>,
    },
    /// Window functions.
    Window {
        input: Box<Plan>,
        windows: Vec<WindowExpr>,
    },
    /// Join two plans.
    Join {
        left: Box<Plan>,
        right: Box<Plan>,
        join_type: JoinType,
        condition: Option<Expr>,
        /// For INNER HASH join: pre-build a hash on the left side.
        algorithm: JoinAlgorithm,
    },
    /// Subquery (materialized).
    Subquery { plan: Box<Plan> },
    /// Distinct.
    Distinct { input: Box<Plan> },
    /// Set operations.
    Union { left: Box<Plan>, right: Box<Plan>, all: bool },
    Intersect { left: Box<Plan>, right: Box<Plan> },
    Except { left: Box<Plan>, right: Box<Plan> },
    /// INSERT / UPDATE / DELETE wrappers.
    Insert {
        table: Arc<Table>,
        source: Box<Plan>,
        columns: Option<Vec<usize>>,
        on_conflict: crate::sql::ast::ConflictResolution,
    },
    Update {
        table: Arc<Table>,
        source: Box<Plan>,
        assignments: Vec<(usize, Expr)>,
    },
    Delete {
        table: Arc<Table>,
        source: Box<Plan>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinAlgorithm {
    /// Nested-loop join. Best for small outer relations.
    NestedLoop,
    /// Hash join. Best for equi-joins between medium/large relations.
    Hash,
    /// Merge join. Best when both sides are sorted on the join key.
    Merge,
}

/// A projected expression with optional alias.
#[derive(Clone, Debug)]
pub struct ProjectExpr {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// An aggregate expression.
#[derive(Clone, Debug)]
pub struct AggExpr {
    pub func: String,
    pub arg: Option<Expr>,
    pub distinct: bool,
    pub alias: Option<String>,
    /// Original expression text for output column naming.
    pub display_name: String,
}

/// A window function expression.
#[derive(Clone, Debug)]
pub struct WindowExpr {
    pub func: String,
    pub arg: Option<Expr>,
    pub distinct: bool,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderTerm>,
    pub frame: Option<crate::sql::ast::WindowFrame>,
    pub alias: Option<String>,
    pub display_name: String,
}
