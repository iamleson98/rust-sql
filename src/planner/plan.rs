//! Logical plan: the IR between the parser/AST and the executor.
//!
//! Plans are tree-shaped. Each node is a relational operator. The executor
//! walks the tree, pulling rows from children in a Volcano-style iterator
//! model.

use crate::sql::ast::{Expr, JoinType, OrderTerm};
use std::sync::Arc;

use crate::schema::{Index, Table};
use crate::types::Row;

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
    /// A range scan via the table's rowid (used for `WHERE rowid BETWEEN ? AND ?`,
    /// `rowid > ?`, `rowid < ?`, `rowid >= ?`, `rowid <= ?`, or any AND-chain
    /// of these on the rowid-alias column). Starts and ends are inclusive.
    /// If either bound is `None`, it means -∞ or +∞ respectively.
    /// A batched rowid multi-lookup (used for `WHERE rowid IN (?, ?, ...)`
    /// and `WHERE id IN (...)` on the INTEGER PRIMARY KEY alias). Evaluates
    /// the list, sorts + dedups the rowids, and fetches each row with ONE
    /// shared B+tree handle — N point seeks instead of a full table scan.
    RowidIn {
        table: Arc<Table>,
        alias: Option<String>,
        /// The IN-list member expressions (evaluated with the statement's
        /// parameters, so `IN (?, ?, ?)` works).
        values: Vec<Expr>,
        /// Remaining predicates for a top-level Filter (e.g.
        /// `id IN (1,2,3) AND name = 'x'`).
        residual: Option<Expr>,
    },
    RowidRange {
        table: Arc<Table>,
        alias: Option<String>,
        start: Option<Expr>,
        end: Option<Expr>,
        /// Remaining predicates that can't be expressed as a range bound
        /// (e.g. `id > 5 AND id < 100 AND name = 'foo'` → start=Some(5),
        /// end=Some(100), residual=Some(name='foo')).
        residual: Option<Expr>,
    },
    /// Batched secondary-index multi-lookup (`WHERE indexed_col IN (?, ?, ...)`
    /// where indexed_col is the first column of an index). Each key seeks
    /// the index, and the matching rows are fetched by rowid — no full
    /// table scan.
    IndexIn {
        table: Arc<Table>,
        alias: Option<String>,
        index: Arc<Index>,
        /// The IN-list member expressions (one per index seek).
        key_exprs: Vec<Expr>,
        /// Remaining predicates for a top-level Filter.
        residual: Option<Expr>,
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
    /// A range scan via a secondary index (used for `WHERE indexed_col > ?`,
    /// `indexed_col BETWEEN ? AND ?`, etc.). Scans the index B+tree from the
    /// start bound to the end bound, then fetches matching rows by rowid.
    /// Each bound is (value expression, inclusive).
    IndexRange {
        table: Arc<Table>,
        alias: Option<String>,
        index: Arc<Index>,
        /// (lower bound, inclusive?) — None means -infinity.
        start: Option<(Expr, bool)>,
        /// (upper bound, inclusive?) — None means +infinity.
        end: Option<(Expr, bool)>,
        /// Remaining predicates that can't be expressed as a range bound.
        residual: Option<Expr>,
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
    /// Index nested-loop join: for each outer row, look up matching inner
    /// rows via a secondary index on the inner table's join key. Only
    /// applicable to INNER joins where the inner side is a base table with
    /// an index on the join key. Falls back to Hash join otherwise.
    ///
    /// This is the single biggest perf win for filtered joins: the
    /// `SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id
    /// WHERE u.id = ?` case goes from 240× slower than SQLite to within
    /// 2-3×, because we only fetch ~10 inner rows instead of decoding all
    /// 10k of them.
    IndexNestedLoopJoin {
        /// Outer (probe) side. Typically already filtered by an
        /// `apply_where` pass that pushed `u.id = ?` down to a RowidLookup.
        outer: Box<Plan>,
        /// Inner table (the one with the index).
        inner_table: Arc<Table>,
        /// Inner alias (for column-name resolution in the output).
        inner_alias: Option<String>,
        /// Index on `inner_table` whose first column is the join key.
        inner_index: Arc<Index>,
        /// Column index in the OUTER row that supplies the join key value.
        /// E.g. for `JOIN orders o ON u.id = o.user_id`, this is the index
        /// of `u.id` in the outer (users) row.
        outer_key_col: usize,
    },
    /// Subquery (materialized).
    Subquery { plan: Box<Plan> },
    /// A materialized CTE result (WITH clause). The rows were computed
    /// BEFORE planning (see api.rs's CTE materialization); references to
    /// the CTE name in FROM scan these rows. Recomputed per statement
    /// execution — statements with CTEs bypass the statement cache.
    CteRows {
        rows: std::sync::Arc<Vec<Row>>,
        columns: Arc<[String]>,
    },
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
        /// `ON CONFLICT ... DO NOTHING / DO UPDATE SET ...` (UPSERT).
        upsert: Option<crate::sql::ast::UpsertClause>,
        /// `RETURNING <cols>` — if present, output one row per affected row.
        returning: Option<Vec<crate::sql::ast::ResultColumn>>,
    },
    Update {
        table: Arc<Table>,
        source: Box<Plan>,
        assignments: Vec<(usize, Expr)>,
        returning: Option<Vec<crate::sql::ast::ResultColumn>>,
    },
    Delete {
        table: Arc<Table>,
        source: Box<Plan>,
        returning: Option<Vec<crate::sql::ast::ResultColumn>>,
    },
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
