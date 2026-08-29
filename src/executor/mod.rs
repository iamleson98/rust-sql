//! Executor: evaluates logical plans and produces rows.
//!
//! The executor walks a logical plan and returns a list of rows (and column
//! names). We use a recursive evaluator (collect-all model) rather than the
//! Volcano iterator model: this is simpler to reason about with Rust's borrow
//! checker and avoids the lifetime gymnastics that streaming iterators would
//! require when the same `&Pager` is shared between operators.
//!
//! For large result sets, the executor materializes everything in memory. A
//! production engine would use a pull-based streaming model with `Rc<RefCell<>>`
//! for shared state, but that adds complexity that doesn't pay off until you
//! have working code in the first place.

pub mod datetime;
pub mod expr;

pub use expr::{apply_binary, evaluate, EvalContext};

use crate::error::{Error, Result};
use crate::planner::plan::*;
use crate::schema::Table;
use crate::sql::ast::*;
use crate::storage::btree::{Btree, LookupResult};
use crate::storage::pager::Pager;
use crate::storage::row_codec::{decode_row, decode_row_into, decode_row_selective, encode_row, encode_row_into};
use crate::types::{Row, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Shared execution state.
pub struct ExecContext<'a> {
    pub pager: &'a Pager,
    /// Positional bound parameters (`?` placeholders), indexed 0..N.
    /// Pushed by `bind_positional`. This is the common case — virtually
    /// all real-world queries use `?` placeholders.
    ///
    /// Previously this was a `HashMap<String, Value>` keyed by the param
    /// name (e.g. "0", "1"). The HashMap allocated a bucket array on first
    /// insert (~200-500 ns per query) and required a hash + lookup per
    /// evaluation. The Vec is allocated once per query (cheap) and indexed
    /// by usize (single memory load).
    pub params: Vec<Value>,
    /// Named parameters (:name, @col, $var). Allocated lazily — empty for
    /// the 99% case of purely positional `?` placeholders.
    pub named_params: HashMap<String, Value>,
    pub last_insert_rowid: i64,
    pub changes: i64,
    /// When true (inside BEGIN..COMMIT), DML operators skip per-statement flushes.
    pub in_transaction: bool,
    /// When true (Database::deferred_flush), DML operators skip per-statement
    /// flushes even outside an explicit transaction. Mirrors SQLite's WAL+
    /// synchronous=NORMAL behaviour. The caller (Database::execute) is
    /// responsible for forcing a flush on the next SELECT, on reaching the
    /// dirty-page threshold, or on an explicit `Database::flush()` call.
    pub deferred_flush: bool,
    /// Snapshot taken at BEGIN, used by ROLLBACK to restore the pager.
    pub txn_snapshot: Option<crate::storage::pager::PagerSnapshot>,
    /// Catalog snapshot taken before the statement started. Used to look up
    /// tables and indexes for DML. This is a raw pointer to avoid lifetime
    /// conflicts with the mutable catalog borrow in api.rs. The caller
    /// (api.rs) guarantees the catalog outlives the context.
    pub catalog_ptr: *const crate::schema::Catalog,
    /// Overrides for table root pages (table_name -> current root page).
    /// Updated when a B+tree split changes the root. This avoids the stale
    /// root_page problem when the catalog's Arc<Table> can't be mutated.
    pub root_overrides: HashMap<String, u32>,
    /// Max rowid per table (avoids O(n) scan on every INSERT).
    pub max_rowids: HashMap<String, i64>,
    /// Marker to keep the lifetime.
    _marker: std::marker::PhantomData<&'a crate::schema::Catalog>,
}

impl<'a> ExecContext<'a> {
    pub fn new(pager: &'a Pager, catalog: *const crate::schema::Catalog) -> Self {
        Self {
            pager,
            params: Vec::new(),
            named_params: HashMap::new(),
            last_insert_rowid: 0,
            changes: 0,
            in_transaction: false,
            deferred_flush: false,
            txn_snapshot: None,
            catalog_ptr: catalog,
            root_overrides: HashMap::new(),
            max_rowids: HashMap::new(),
            _marker: std::marker::PhantomData,
        }
    }

    /// Get a reference to the catalog. Safe because the caller guarantees
    /// the catalog outlives the context and no mutable access happens during
    /// execution (DDL is handled separately).
    pub fn catalog(&self) -> &'a crate::schema::Catalog {
        unsafe { &*self.catalog_ptr }
    }

    /// Get the current root page for a table, checking overrides first.
    pub fn table_root(&self, table: &Table) -> u32 {
        self.root_overrides
            .get(&table.name.to_ascii_lowercase())
            .copied()
            .unwrap_or(table.root_page)
    }

    /// Update the root page override for a table.
    pub fn set_table_root(&mut self, table_name: &str, root: u32) {
        self.root_overrides.insert(table_name.to_ascii_lowercase(), root);
    }

    /// Fast-path set_table_root for callers that have already lower-cased
    /// the table name (e.g. exec_insert hoists this out of the per-row
    /// loop). Avoids the per-call `to_ascii_lowercase()` String allocation.
    pub fn set_table_root_lc(&mut self, table_name_lc: &str, root: u32) {
        self.root_overrides.insert(table_name_lc.to_string(), root);
    }

    /// Get the cached max rowid for a table, or scan if not cached.
    pub fn get_or_scan_max_rowid(&mut self, table: &Table) -> Result<i64> {
        let key = table.name.to_ascii_lowercase();
        if let Some(&max) = self.max_rowids.get(&key) {
            return Ok(max);
        }
        let root = self.table_root(table);
        let max = find_max_rowid(self.pager, root)?;
        self.max_rowids.insert(key, max);
        Ok(max)
    }

    /// Update the cached max rowid for a table.
    pub fn set_max_rowid(&mut self, table_name: &str, rowid: i64) {
        self.max_rowids.insert(table_name.to_ascii_lowercase(), rowid);
    }

    /// Fast-path set_max_rowid for callers that have already lower-cased
    /// the table name. Avoids the per-call String allocation.
    pub fn set_max_rowid_lc(&mut self, table_name_lc: &str, rowid: i64) {
        // max_rowids is keyed by String, so we'd need to_string() anyway.
        // But we still save the to_ascii_lowercase() work.
        self.max_rowids.insert(table_name_lc.to_string(), rowid);
    }

    /// Bind a positional parameter (the common `?` placeholder case).
    /// Pushes to the params Vec. Cheaper than `bind(name, value)` because
    /// it skips the String key allocation and HashMap insert.
    pub fn bind_positional(&mut self, value: Value) {
        self.params.push(value);
    }

    /// Bind a parameter by name. For numeric names (e.g. "0", "1"), pushes
    /// to the positional Vec at the parsed index (extending with Nulls if
    /// needed). For named params (:name, @col, $var), inserts into the
    /// named_params HashMap.
    ///
    /// Kept for backwards compatibility with existing callers that pass
    /// `&format!("{}", i)` as the name. New code should prefer
    /// `bind_positional` for the positional case.
    pub fn bind(&mut self, name: &str, value: Value) {
        if let Ok(idx) = name.parse::<usize>() {
            // Positional — extend the Vec with Nulls if needed.
            while self.params.len() <= idx {
                self.params.push(Value::Null);
            }
            self.params[idx] = value;
        } else {
            self.named_params.insert(name.to_string(), value);
        }
    }
}

/// Result of executing a plan: column names + rows.
pub struct ExecResult {
    /// Output column names. `Arc<[String]>` so base-table operators can
    /// return the cached names from `Table::col_names` / `qualified_col_names`
    /// with a single refcount bump — no per-query `String` allocations.
    pub columns: Arc<[String]>,
    pub rows: Vec<Row>,
}

impl ExecResult {
    pub fn empty() -> Self {
        Self { columns: Arc::from(Vec::new()), rows: Vec::new() }
    }
}

/// Execute a plan and return all rows.
pub fn execute(plan: &Plan, ctx: &mut ExecContext<'_>) -> Result<ExecResult> {
    match plan {
        Plan::Scan { table, alias, .. } => exec_scan(ctx, table.clone(), alias.clone()),
        Plan::Values { rows } => exec_values(ctx, rows),
        Plan::Filter { input, predicate } => exec_filter(ctx, input, predicate),
        Plan::Project { input, columns } => exec_project(ctx, input, columns),
        Plan::Sort { input, terms } => exec_sort(ctx, input, terms),
        Plan::Limit { input, count, offset } => exec_limit(ctx, input, count, offset),
        Plan::Aggregate { input, group_by, aggregates } => exec_aggregate(ctx, input, group_by, aggregates),
        Plan::Window { input, windows } => exec_window(ctx, input, windows),
        Plan::Join { left, right, join_type, condition, algorithm } => {
            if *algorithm == crate::planner::plan::JoinAlgorithm::Hash {
                exec_hash_join(ctx, left, right, *join_type, condition)
            } else {
                exec_join(ctx, left, right, *join_type, condition)
            }
        }
        Plan::IndexNestedLoopJoin { outer, inner_table, inner_alias, inner_index, outer_key_col } => {
            exec_index_nested_loop_join(ctx, outer, inner_table.clone(), inner_alias.clone(), inner_index.clone(), *outer_key_col)
        }
        Plan::Distinct { input } => exec_distinct(ctx, input),
        Plan::Union { left, right, all } => exec_union(ctx, left, right, *all),
        Plan::Intersect { left, right } => exec_intersect(ctx, left, right),
        Plan::Except { left, right } => exec_except(ctx, left, right),
        Plan::Subquery { plan } => execute(plan, ctx),
        Plan::RowidLookup { table, rowid, .. } => exec_rowid_lookup(ctx, table.clone(), rowid),
        Plan::RowidRange { table, alias: _, start, end, residual } => exec_rowid_range(ctx, table.clone(), start.as_ref(), end.as_ref(), residual.as_ref()),
        Plan::IndexLookup { table, alias: _, index, key_exprs } => exec_index_lookup(ctx, table.clone(), index.clone(), key_exprs),
        Plan::IndexRange { table, alias, index, start, end, residual } => exec_index_range(ctx, table.clone(), alias.clone(), index.clone(), start.as_ref(), end.as_ref(), residual.as_ref()),
        Plan::Insert { table, source, columns, on_conflict, upsert, returning } => exec_insert(ctx, table.clone(), source, columns.clone(), *on_conflict, upsert.as_ref(), returning.as_deref()),
        Plan::Update { table, source, assignments, returning } => exec_update(ctx, table.clone(), source, assignments, returning.as_deref()),
        Plan::Delete { table, source, returning } => exec_delete(ctx, table.clone(), source, returning.as_deref()),
    }
}

// Helper: evaluate an expression against a single row.
fn eval_row(expr: &Expr, row: &[Value], col_names: &[String], params: &[Value], named_params: &HashMap<String, Value>) -> Result<Value> {
    let ctx = EvalContext::new(row, col_names, params, named_params);
    evaluate(expr, &ctx)
}

/// Enforce NOT NULL and CHECK constraints on a (new or updated) row.
/// Returns a semantic error naming the offending column / table.
///
/// `col_names` is only needed when the table has CHECK constraints
/// (NOT NULL checks work purely positionally) — pass an empty slice for
/// tables without CHECKs to avoid building the name Vec on hot paths.
fn enforce_row_constraints(
    table: &Table,
    row: &[Value],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<()> {
    for (i, col) in table.columns.iter().enumerate() {
        if !col.nullable && row.get(i).map(|v| v.is_null()).unwrap_or(true) {
            return Err(Error::semantic(format!(
                "NOT NULL constraint failed: {}.{}",
                table.name, col.name
            )));
        }
    }
    for expr in &table.check_exprs {
        let v = eval_row(expr, row, col_names, params, named_params)?;
        if !v.is_truthy() {
            return Err(Error::semantic(format!(
                "CHECK constraint failed: {}",
                table.name
            )));
        }
    }
    Ok(())
}

/// Project the RETURNING clause for one affected row.
/// `Star` expands to all columns; `Expr` is evaluated against the row.
fn project_returning_row(
    returning: &[crate::sql::ast::ResultColumn],
    row: &[Value],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(returning.len());
    for rc in returning {
        match rc {
            crate::sql::ast::ResultColumn::Star => out.extend_from_slice(row),
            crate::sql::ast::ResultColumn::TableStar(_) => out.extend_from_slice(row),
            crate::sql::ast::ResultColumn::Expr { expr, .. } => {
                out.push(eval_row(expr, row, col_names, params, named_params)?);
            }
        }
    }
    Ok(out)
}

/// Column names for a RETURNING result (used for `query_with_columns`).
fn returning_column_names(
    returning: &[crate::sql::ast::ResultColumn],
    col_names: &[String],
) -> Vec<String> {
    let mut out = Vec::with_capacity(returning.len());
    for rc in returning {
        match rc {
            crate::sql::ast::ResultColumn::Star => out.extend_from_slice(col_names),
            crate::sql::ast::ResultColumn::TableStar(_) => out.extend_from_slice(col_names),
            crate::sql::ast::ResultColumn::Expr { expr, alias } => {
                if let Some(a) = alias {
                    out.push(a.clone());
                } else {
                    out.push(expr_display_name(expr, col_names));
                }
            }
        }
    }
    out
}

// ============================================================================
// Subquery substitution (uncorrelated scalar / IN / EXISTS subqueries)
// ============================================================================

/// Does this expression tree contain any subquery expression nodes?
pub fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::Exists(_) => true,
        Expr::In { source: InSource::Subquery(_), .. } => true,
        _ => {
            let mut found = false;
            map_expr_children(e, &mut |child| {
                if expr_has_subquery(child) {
                    found = true;
                }
                Ok(child.clone())
            })
            .ok();
            found
        }
    }
}

/// Apply a function to each immediate child expression of `e`, rebuilding
/// the node with the (possibly replaced) children.
fn map_expr_children(e: &Expr, f: &mut dyn FnMut(&Expr) -> Result<Expr>) -> Result<Expr> {
    macro_rules! r {
        ($x:expr) => {
            f($x)?
        };
    }
    Ok(match e {
        Expr::Literal(v) => Expr::Literal(v.clone()),
        Expr::Parameter(p) => Expr::Parameter(p.clone()),
        Expr::Column { table, name } => Expr::Column { table: table.clone(), name: name.clone() },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(r!(left)),
            right: Box::new(r!(right)),
        },
        Expr::Unary { op, expr } => Expr::Unary { op: *op, expr: Box::new(r!(expr)) },
        Expr::Between { expr, low, high, negated } => Expr::Between {
            expr: Box::new(r!(expr)),
            low: Box::new(r!(low)),
            high: Box::new(r!(high)),
            negated: *negated,
        },
        Expr::In { expr, source, negated } => {
            let new_source = match source {
                InSource::List(list) => {
                    let mut new_list = Vec::with_capacity(list.len());
                    for item in list {
                        new_list.push(r!(item));
                    }
                    InSource::List(new_list)
                }
                other => other.clone(),
            };
            Expr::In { expr: Box::new(r!(expr)), source: new_source, negated: *negated }
        }
        Expr::Like { op, expr, pattern, escape, negated } => {
            let new_escape = match escape.as_ref() {
                Some(es) => Some(Box::new(r!(es.as_ref()))),
                None => None,
            };
            Expr::Like {
                op: *op,
                expr: Box::new(r!(expr)),
                pattern: Box::new(r!(pattern)),
                escape: new_escape,
                negated: *negated,
            }
        }
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: Box::new(r!(expr)), negated: *negated },
        Expr::Is { left, right, negated } => Expr::Is {
            left: Box::new(r!(left)),
            right: Box::new(r!(right)),
            negated: *negated,
        },
        Expr::Function { name, distinct, args, filter, over } => {
            let mut new_args = Vec::with_capacity(args.len());
            for a in args {
                new_args.push(r!(a));
            }
            let new_filter = match filter.as_ref() {
                Some(ftr) => Some(Box::new(r!(ftr.as_ref()))),
                None => None,
            };
            Expr::Function {
                name: name.clone(),
                distinct: *distinct,
                args: new_args,
                filter: new_filter,
                over: over.clone(),
            }
        }
        Expr::Case { operand, whens, else_ } => {
            let new_operand = match operand.as_ref() {
                Some(o) => Some(Box::new(r!(o.as_ref()))),
                None => None,
            };
            let mut new_whens = Vec::with_capacity(whens.len());
            for (w, v) in whens {
                new_whens.push((r!(w), r!(v)));
            }
            let new_else = match else_.as_ref() {
                Some(el) => Some(Box::new(r!(el.as_ref()))),
                None => None,
            };
            Expr::Case { operand: new_operand, whens: new_whens, else_: new_else }
        }
        Expr::Row(items) => {
            let mut new_items = Vec::with_capacity(items.len());
            for i in items {
                new_items.push(r!(i));
            }
            Expr::Row(new_items)
        }
        Expr::Subquery(sel) => Expr::Subquery(sel.clone()),
        Expr::Exists(sel) => Expr::Exists(sel.clone()),
        Expr::Cast { expr, type_name } => Expr::Cast { expr: Box::new(r!(expr)), type_name: type_name.clone() },
        Expr::Collate { expr, collation } => Expr::Collate { expr: Box::new(r!(expr)), collation: collation.clone() },
        Expr::Raise { action, message } => {
            let new_message = match message.as_ref() {
                Some(m) => Some(Box::new(r!(m.as_ref()))),
                None => None,
            };
            Expr::Raise {
                action: *action,
                message: new_message,
            }
        }
    })
}

/// Rewrite all UNCORRELATED subquery expressions in a plan into literals /
/// IN-lists by executing them once (mirrors SQLite's OP_Once caching).
/// Correlated subqueries are left untouched (they error at eval time).
pub fn rewrite_plan_subqueries(plan: &Plan, ctx: &mut ExecContext<'_>) -> Result<Plan> {
    let mut new_plan = plan.clone();
    rewrite_plan_subqueries_in_place(&mut new_plan, ctx)?;
    Ok(new_plan)
}

/// Quick scan: does this plan contain any subquery expressions?
pub fn plan_has_subqueries(plan: &Plan) -> bool {
    fn exprs_in_plan<'a>(p: &'a Plan, out: &mut Vec<&'a Expr>) {
        match p {
            Plan::Scan { predicate, .. } => {
                if let Some(e) = predicate.as_ref() {
                    out.push(e);
                }
            }
            Plan::RowidLookup { rowid, .. } => out.push(rowid),
            Plan::RowidRange { start, end, residual, .. } => {
                if let Some(s) = start.as_ref() {
                    out.push(s);
                }
                if let Some(e) = end.as_ref() {
                    out.push(e);
                }
                if let Some(r) = residual.as_ref() {
                    out.push(r);
                }
            }
            Plan::IndexLookup { key_exprs, .. } => {
                for k in key_exprs {
                    out.push(k);
                }
            }
            Plan::IndexRange { start, end, residual, .. } => {
                if let Some((s, _)) = start.as_ref() {
                    out.push(s);
                }
                if let Some((e, _)) = end.as_ref() {
                    out.push(e);
                }
                if let Some(r) = residual.as_ref() {
                    out.push(r);
                }
            }
            Plan::Values { rows } => {
                for row in rows {
                    for e in row {
                        out.push(e);
                    }
                }
            }
            Plan::Filter { input, predicate } => {
                out.push(predicate);
                exprs_in_plan(input, out);
            }
            Plan::Project { input, columns } => {
                for c in columns {
                    out.push(&c.expr);
                }
                exprs_in_plan(input, out);
            }
            Plan::Sort { input, terms } => {
                for t in terms {
                    out.push(&t.expr);
                }
                exprs_in_plan(input, out);
            }
            Plan::Limit { input, count, offset } => {
                out.push(count);
                out.push(offset);
                exprs_in_plan(input, out);
            }
            Plan::Aggregate { input, group_by, aggregates } => {
                for g in group_by {
                    out.push(g);
                }
                for a in aggregates {
                    if let Some(arg) = a.arg.as_ref() {
                        out.push(arg);
                    }
                }
                exprs_in_plan(input, out);
            }
            Plan::Window { input, windows } => {
                for w in windows {
                    if let Some(arg) = w.arg.as_ref() {
                        out.push(arg);
                    }
                    for p in &w.partition_by {
                        out.push(p);
                    }
                    for t in &w.order_by {
                        out.push(&t.expr);
                    }
                }
                exprs_in_plan(input, out);
            }
            Plan::Join { left, right, condition, .. } => {
                if let Some(c) = condition.as_ref() {
                    out.push(c);
                }
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::IndexNestedLoopJoin { outer, .. } => exprs_in_plan(outer, out),
            Plan::Subquery { plan } => exprs_in_plan(plan, out),
            Plan::Distinct { input } => exprs_in_plan(input, out),
            Plan::Union { left, right, .. } => {
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::Intersect { left, right } => {
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::Except { left, right } => {
                exprs_in_plan(left, out);
                exprs_in_plan(right, out);
            }
            Plan::Insert { source, .. } => exprs_in_plan(source, out),
            Plan::Update { source, assignments, .. } => {
                for (_i, e) in assignments {
                    out.push(e);
                }
                exprs_in_plan(source, out);
            }
            Plan::Delete { source, .. } => exprs_in_plan(source, out),
        }
    }
    let mut exprs = Vec::new();
    exprs_in_plan(plan, &mut exprs);
    exprs.iter().any(|e| expr_has_subquery(e))
}

fn rewrite_plan_subqueries_in_place(plan: &mut Plan, ctx: &mut ExecContext<'_>) -> Result<()> {
    match plan {
        Plan::Scan { predicate, .. } => {
            if let Some(p) = predicate.as_mut() {
                rewrite_expr_in_place(p, ctx)?;
            }
        }
        Plan::RowidLookup { rowid, .. } => {
            rewrite_expr_in_place(rowid, ctx)?;
        }
        Plan::RowidRange { start, end, residual, .. } => {
            if let Some(s) = start.as_mut() {
                rewrite_expr_in_place(s, ctx)?;
            }
            if let Some(e) = end.as_mut() {
                rewrite_expr_in_place(e, ctx)?;
            }
            if let Some(r) = residual.as_mut() {
                rewrite_expr_in_place(r, ctx)?;
            }
        }
        Plan::IndexLookup { key_exprs, .. } => {
            for k in key_exprs.iter_mut() {
                rewrite_expr_in_place(k, ctx)?;
            }
        }
        Plan::IndexRange { start, end, residual, .. } => {
            if let Some((s, _)) = start.as_mut() {
                rewrite_expr_in_place(s, ctx)?;
            }
            if let Some((e, _)) = end.as_mut() {
                rewrite_expr_in_place(e, ctx)?;
            }
            if let Some(r) = residual.as_mut() {
                rewrite_expr_in_place(r, ctx)?;
            }
        }
        Plan::Values { rows } => {
            for row in rows.iter_mut() {
                for e in row.iter_mut() {
                    rewrite_expr_in_place(e, ctx)?;
                }
            }
        }
        Plan::Filter { input, predicate } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            rewrite_expr_in_place(predicate, ctx)?;
        }
        Plan::Project { input, columns } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for c in columns.iter_mut() {
                rewrite_expr_in_place(&mut c.expr, ctx)?;
            }
        }
        Plan::Sort { input, terms } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for t in terms.iter_mut() {
                rewrite_expr_in_place(&mut t.expr, ctx)?;
            }
        }
        Plan::Limit { input, count, offset } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            rewrite_expr_in_place(count, ctx)?;
            rewrite_expr_in_place(offset, ctx)?;
        }
        Plan::Aggregate { input, group_by, aggregates } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for g in group_by.iter_mut() {
                rewrite_expr_in_place(g, ctx)?;
            }
            for a in aggregates.iter_mut() {
                if let Some(arg) = a.arg.as_mut() {
                    rewrite_expr_in_place(arg, ctx)?;
                }
            }
        }
        Plan::Window { input, windows } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
            for w in windows.iter_mut() {
                if let Some(arg) = w.arg.as_mut() {
                    rewrite_expr_in_place(arg, ctx)?;
                }
                for p in w.partition_by.iter_mut() {
                    rewrite_expr_in_place(p, ctx)?;
                }
                for t in w.order_by.iter_mut() {
                    rewrite_expr_in_place(&mut t.expr, ctx)?;
                }
            }
        }
        Plan::Join { left, right, condition, .. } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
            if let Some(c) = condition.as_mut() {
                rewrite_expr_in_place(c, ctx)?;
            }
        }
        Plan::IndexNestedLoopJoin { outer, .. } => {
            rewrite_plan_subqueries_in_place(outer, ctx)?;
        }
        Plan::Subquery { plan } => {
            rewrite_plan_subqueries_in_place(plan, ctx)?;
        }
        Plan::Distinct { input } => {
            rewrite_plan_subqueries_in_place(input, ctx)?;
        }
        Plan::Union { left, right, .. } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
        }
        Plan::Intersect { left, right } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
        }
        Plan::Except { left, right } => {
            rewrite_plan_subqueries_in_place(left, ctx)?;
            rewrite_plan_subqueries_in_place(right, ctx)?;
        }
        Plan::Insert { source, .. } => {
            rewrite_plan_subqueries_in_place(source, ctx)?;
        }
        Plan::Update { source, assignments, .. } => {
            rewrite_plan_subqueries_in_place(source, ctx)?;
            for (_idx, e) in assignments.iter_mut() {
                rewrite_expr_in_place(e, ctx)?;
            }
        }
        Plan::Delete { source, .. } => {
            rewrite_plan_subqueries_in_place(source, ctx)?;
        }
    }
    Ok(())
}

/// Rewrite subqueries inside a single expression, in place.
fn rewrite_expr_in_place(expr: &mut Expr, ctx: &mut ExecContext<'_>) -> Result<()> {
    if !expr_has_subquery(expr) {
        return Ok(());
    }
    let rewritten = rewrite_subqueries_rec(expr, ctx)?;
    *expr = rewritten;
    Ok(())
}

/// Bottom-up rewrite of subquery nodes in an expression.
fn rewrite_subqueries_rec(e: &Expr, ctx: &mut ExecContext<'_>) -> Result<Expr> {
    // First, rewrite children.
    let e = map_expr_children(e, &mut |child| rewrite_subqueries_rec(child, ctx))?;
    // Then, handle this node if it is a subquery node.
    match e {
        Expr::Subquery(sel) => {
            // Rewrite any NESTED subqueries inside this subquery first.
            let mut sel = *sel;
            rewrite_select_subqueries(&mut sel, ctx)?;
            if subquery_is_correlated(&sel, ctx.catalog()) {
                Ok(Expr::Subquery(Box::new(sel)))
            } else {
                let res = exec_select_statement(&sel, ctx)?;
                let v = res
                    .rows
                    .first()
                    .and_then(|r| r.first().cloned())
                    .unwrap_or(Value::Null);
                Ok(Expr::Literal(v))
            }
        }
        Expr::Exists(sel) => {
            let mut sel = *sel;
            rewrite_select_subqueries(&mut sel, ctx)?;
            if subquery_is_correlated(&sel, ctx.catalog()) {
                Ok(Expr::Exists(Box::new(sel)))
            } else {
                let res = exec_select_statement(&sel, ctx)?;
                Ok(Expr::Literal(Value::Integer(if res.rows.is_empty() { 0 } else { 1 })))
            }
        }
        Expr::In { expr, source, negated } => {
            let inner = match source {
                InSource::Subquery(sel) => {
                    let mut sel = *sel;
                    rewrite_select_subqueries(&mut sel, ctx)?;
                    if subquery_is_correlated(&sel, ctx.catalog()) {
                        InSource::Subquery(Box::new(sel))
                    } else {
                        let res = exec_select_statement(&sel, ctx)?;
                        let list: Vec<Expr> = res
                            .rows
                            .iter()
                            .map(|r| Expr::Literal(r.first().cloned().unwrap_or(Value::Null)))
                            .collect();
                        InSource::List(list)
                    }
                }
                other => other,
            };
            Ok(Expr::In { expr, source: inner, negated })
        }
        other => Ok(other),
    }
}

/// Rewrite subquery expressions inside a SELECT statement (its WHERE,
/// projection, HAVING, GROUP BY, ORDER BY, LIMIT, join conditions —
/// including nested subqueries).
fn rewrite_select_subqueries(sel: &mut SelectStatement, ctx: &mut ExecContext<'_>) -> Result<()> {
    rewrite_body_subqueries(&mut sel.body, ctx)?;
    for t in sel.order_by.iter_mut() {
        rewrite_expr_in_place(&mut t.expr, ctx)?;
    }
    if let Some(l) = sel.limit.as_mut() {
        rewrite_expr_in_place(l, ctx)?;
    }
    if let Some(o) = sel.offset.as_mut() {
        rewrite_expr_in_place(o, ctx)?;
    }
    Ok(())
}

fn rewrite_body_subqueries(body: &mut SelectBody, ctx: &mut ExecContext<'_>) -> Result<()> {
    match body {
        SelectBody::Simple(s) => {
            for c in s.columns.iter_mut() {
                if let ResultColumn::Expr { expr, .. } = c {
                    rewrite_expr_in_place(expr, ctx)?;
                }
            }
            if let Some(w) = s.where_clause.as_mut() {
                rewrite_expr_in_place(w, ctx)?;
            }
            for g in s.group_by.iter_mut() {
                rewrite_expr_in_place(g, ctx)?;
            }
            if let Some(h) = s.having.as_mut() {
                rewrite_expr_in_place(h, ctx)?;
            }
            if let Some(from) = s.from.as_mut() {
                rewrite_table_expression_subqueries(from, ctx)?;
            }
        }
        SelectBody::Binary { left, right, .. } => {
            rewrite_body_subqueries(left, ctx)?;
            rewrite_body_subqueries(right, ctx)?;
        }
    }
    Ok(())
}

fn rewrite_table_expression_subqueries(te: &mut TableExpression, ctx: &mut ExecContext<'_>) -> Result<()> {
    match te {
        TableExpression::Table { .. } => {}
        TableExpression::Subquery { select, .. } => {
            rewrite_select_subqueries(select, ctx)?;
        }
        TableExpression::Join { left, right, constraint, .. } => {
            rewrite_table_expression_subqueries(left, ctx)?;
            rewrite_table_expression_subqueries(right, ctx)?;
            if let JoinConstraint::On(e) = constraint {
                rewrite_expr_in_place(e, ctx)?;
            }
        }
    }
    Ok(())
}

/// Plan + execute a SELECT statement (used by subquery substitution).
fn exec_select_statement(sel: &SelectStatement, ctx: &mut ExecContext<'_>) -> Result<ExecResult> {
    let catalog = ctx.catalog();
    let mut planner = crate::planner::Planner::new(catalog);
    let plan = planner.plan_select(sel)?;
    execute(&plan, ctx)
}

/// Conservative correlated-subquery detector: collect every column ref in
/// the subquery (including nested subqueries) and every source name (also
/// including nested sources). If any ref's qualifier isn't a local source,
/// or any unqualified ref doesn't match a local source column, treat the
/// subquery as correlated (outer refs present).
fn subquery_is_correlated(sel: &SelectStatement, catalog: &crate::schema::Catalog) -> bool {
    let mut sources: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut source_tables: Vec<Arc<Table>> = Vec::new();
    collect_select_sources(sel, catalog, &mut sources, &mut source_tables);
    let refs = collect_select_refs(sel);
    for (qual, name) in refs {
        if let Some(q) = qual {
            if !sources.contains(&q.to_ascii_lowercase()) {
                return true;
            }
        } else {
            // Unqualified: must match a local source column.
            let known = source_tables
                .iter()
                .any(|t| t.find_column(&name).is_some());
            if !known {
                return true;
            }
        }
    }
    false
}

/// Collect source aliases/table names from a SELECT's FROM clause,
/// including join trees and nested subqueries-in-FROM.
fn collect_select_sources(
    sel: &SelectStatement,
    catalog: &crate::schema::Catalog,
    sources: &mut std::collections::HashSet<String>,
    source_tables: &mut Vec<Arc<Table>>,
) {
    collect_body_sources(&sel.body, catalog, sources, source_tables);
    // Nested subqueries inside expressions of this SELECT also bring their
    // own sources into scope for their own refs — collect them too so a
    // ref to a nested alias isn't misclassified as outer.
    let mut refs_and_subs: Vec<&SelectStatement> = Vec::new();
    collect_nested_selects(sel, &mut refs_and_subs);
    for s in refs_and_subs {
        collect_body_sources(&s.body, catalog, sources, source_tables);
    }
}

fn collect_body_sources(
    body: &SelectBody,
    catalog: &crate::schema::Catalog,
    sources: &mut std::collections::HashSet<String>,
    source_tables: &mut Vec<Arc<Table>>,
) {
    match body {
        SelectBody::Simple(s) => {
            if let Some(from) = &s.from {
                collect_table_expression_sources(from, catalog, sources, source_tables);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_sources(left, catalog, sources, source_tables);
            collect_body_sources(right, catalog, sources, source_tables);
        }
    }
}

fn collect_table_expression_sources(
    te: &TableExpression,
    catalog: &crate::schema::Catalog,
    sources: &mut std::collections::HashSet<String>,
    source_tables: &mut Vec<Arc<Table>>,
) {
    match te {
        TableExpression::Table { name, alias, .. } => {
            sources.insert(
                alias
                    .clone()
                    .unwrap_or_else(|| name.clone())
                    .to_ascii_lowercase(),
            );
            if let Some(t) = catalog.get_table(name) {
                source_tables.push(t);
            }
        }
        TableExpression::Subquery { alias, select, .. } => {
            if let Some(a) = alias {
                sources.insert(a.to_ascii_lowercase());
            }
            // The derived table's columns come from its own SELECT —
            // collect its output names so unqualified refs can match.
            collect_body_sources(&select.body, catalog, sources, source_tables);
        }
        TableExpression::Join { left, right, .. } => {
            collect_table_expression_sources(left, catalog, sources, source_tables);
            collect_table_expression_sources(right, catalog, sources, source_tables);
        }
    }
}

/// Collect every column reference in a SELECT statement (all clauses,
/// including nested subqueries).
fn collect_select_refs(sel: &SelectStatement) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    collect_body_refs(&sel.body, &mut out);
    for t in &sel.order_by {
        collect_expr_refs(&t.expr, &mut out);
    }
    if let Some(l) = &sel.limit {
        collect_expr_refs(l, &mut out);
    }
    if let Some(o) = &sel.offset {
        collect_expr_refs(o, &mut out);
    }
    out
}

fn collect_body_refs(body: &SelectBody, out: &mut Vec<(Option<String>, String)>) {
    match body {
        SelectBody::Simple(s) => {
            for c in &s.columns {
                if let ResultColumn::Expr { expr, .. } = c {
                    collect_expr_refs(expr, out);
                }
            }
            if let Some(w) = &s.where_clause {
                collect_expr_refs(w, out);
            }
            for g in &s.group_by {
                collect_expr_refs(g, out);
            }
            if let Some(h) = &s.having {
                collect_expr_refs(h, out);
            }
            if let Some(from) = &s.from {
                collect_table_expression_refs(from, out);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_refs(left, out);
            collect_body_refs(right, out);
        }
    }
}

fn collect_table_expression_refs(te: &TableExpression, out: &mut Vec<(Option<String>, String)>) {
    match te {
        TableExpression::Table { .. } => {}
        TableExpression::Subquery { select, .. } => {
            collect_body_refs(&select.body, out);
            for t in &select.order_by {
                collect_expr_refs(&t.expr, out);
            }
            if let Some(l) = &select.limit {
                collect_expr_refs(l, out);
            }
        }
        TableExpression::Join { left, right, constraint, .. } => {
            collect_table_expression_refs(left, out);
            collect_table_expression_refs(right, out);
            if let JoinConstraint::On(e) = constraint {
                collect_expr_refs(e, out);
            }
        }
    }
}

fn collect_expr_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => {
            out.push((table.clone(), name.clone()));
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_refs(left, out);
            collect_expr_refs(right, out);
        }
        Expr::Unary { expr, .. } => collect_expr_refs(expr, out),
        Expr::Between { expr, low, high, .. } => {
            collect_expr_refs(expr, out);
            collect_expr_refs(low, out);
            collect_expr_refs(high, out);
        }
        Expr::In { expr, source, .. } => {
            collect_expr_refs(expr, out);
            match source {
                InSource::List(list) => {
                    for item in list {
                        collect_expr_refs(item, out);
                    }
                }
                InSource::Subquery(sel) => {
                    collect_body_refs(&sel.body, out);
                }
                InSource::Table(_) => {}
            }
        }
        Expr::Like { expr, pattern, escape, .. } => {
            collect_expr_refs(expr, out);
            collect_expr_refs(pattern, out);
            if let Some(es) = escape {
                collect_expr_refs(es, out);
            }
        }
        Expr::IsNull { expr, .. } => collect_expr_refs(expr, out),
        Expr::Is { left, right, .. } => {
            collect_expr_refs(left, out);
            collect_expr_refs(right, out);
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                collect_expr_refs(a, out);
            }
            if let Some(f) = filter {
                collect_expr_refs(f, out);
            }
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand {
                collect_expr_refs(o, out);
            }
            for (w, v) in whens {
                collect_expr_refs(w, out);
                collect_expr_refs(v, out);
            }
            if let Some(e) = else_ {
                collect_expr_refs(e, out);
            }
        }
        Expr::Row(items) => {
            for i in items {
                collect_expr_refs(i, out);
            }
        }
        Expr::Subquery(sel) => {
            collect_body_refs(&sel.body, out);
        }
        Expr::Exists(sel) => {
            collect_body_refs(&sel.body, out);
        }
        Expr::Cast { expr, .. } => collect_expr_refs(expr, out),
        Expr::Collate { expr, .. } => collect_expr_refs(expr, out),
        Expr::Raise { message, .. } => {
            if let Some(m) = message {
                collect_expr_refs(m, out);
            }
        }
        _ => {}
    }
}

/// Collect nested SELECT statements inside a SELECT's expressions.
fn collect_nested_selects<'a>(sel: &'a SelectStatement, out: &mut Vec<&'a SelectStatement>) {
    let mut body_stack: Vec<&'a SelectBody> = vec![&sel.body];
    while let Some(body) = body_stack.pop() {
        match body {
            SelectBody::Simple(s) => {
                for c in &s.columns {
                    if let ResultColumn::Expr { expr, .. } = c {
                        collect_expr_selects(expr, out);
                    }
                }
                if let Some(w) = &s.where_clause {
                    collect_expr_selects(w, out);
                }
                for g in &s.group_by {
                    collect_expr_selects(g, out);
                }
                if let Some(h) = &s.having {
                    collect_expr_selects(h, out);
                }
            }
            SelectBody::Binary { left, right, .. } => {
                body_stack.push(left);
                body_stack.push(right);
            }
        }
    }
}

fn collect_expr_selects<'a>(e: &'a Expr, out: &mut Vec<&'a SelectStatement>) {
    match e {
        Expr::Subquery(sel) | Expr::Exists(sel) => {
            out.push(sel);
            collect_nested_selects(sel, out);
        }
        Expr::In { source, .. } => {
            if let InSource::Subquery(sel) = source {
                out.push(sel);
                collect_nested_selects(sel, out);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_selects(left, out);
            collect_expr_selects(right, out);
        }
        Expr::Unary { expr, .. } => collect_expr_selects(expr, out),
        Expr::Function { args, .. } => {
            for a in args {
                collect_expr_selects(a, out);
            }
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand {
                collect_expr_selects(o, out);
            }
            for (w, v) in whens {
                collect_expr_selects(w, out);
                collect_expr_selects(v, out);
            }
            if let Some(e) = else_ {
                collect_expr_selects(e, out);
            }
        }
        _ => {}
    }
}

// ============================================================================
// Scan
// ============================================================================

fn exec_scan(ctx: &mut ExecContext<'_>, table: Arc<Table>, alias: Option<String>) -> Result<ExecResult> {
    let mut rows = Vec::new();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    bt.scan_table_borrowed(|_rowid, payload| {
        if let Ok(row) = decode_row(payload, table.n_columns()) {
            rows.push(row);
        }
        true
    })?;
    // Column names: if there's an alias, prefix with "alias." so qualified
    // references in the planner/evaluator can find them. We also include
    // the unqualified name for backward compat.
    //
    // FAST PATH: when the effective prefix equals the table name (no alias,
    // or an alias identical to the table), return the cached
    // `qualified_col_names` built once in `build_table` — one refcount bump
    // instead of N `format!()` allocations per query.
    let prefix = alias.as_deref().unwrap_or(&table.name);
    let columns: Arc<[String]> = if prefix == table.name {
        table.qualified_col_names.clone()
    } else {
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect::<Vec<String>>()
            .into()
    };
    Ok(ExecResult {
        columns,
        rows,
    })
}

// ============================================================================
// Values
// ============================================================================

fn exec_values(ctx: &mut ExecContext<'_>, rows: &[Vec<Expr>]) -> Result<ExecResult> {
    let n = rows.first().map(|r| r.len()).unwrap_or(0);
    let columns: Arc<[String]> = (0..n)
        .map(|i| format!("column{}", i + 1))
        .collect::<Vec<String>>()
        .into();
    let mut out = Vec::with_capacity(rows.len());
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    for exprs in rows {
        let mut row = Vec::with_capacity(exprs.len());
        for e in exprs {
            row.push(evaluate(e, &EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params))?);
        }
        out.push(row);
    }
    Ok(ExecResult { columns, rows: out })
}

// ============================================================================
// Filter
// ============================================================================

fn exec_filter(ctx: &mut ExecContext<'_>, input: &Plan, predicate: &Expr) -> Result<ExecResult> {
    let inner = execute(input, ctx)?;
    let mut rows = Vec::new();
    for row in inner.rows {
        let v = eval_row(predicate, &row, &inner.columns, &ctx.params, &ctx.named_params)?;
        if v.is_truthy() {
            rows.push(row);
        }
    }
    Ok(ExecResult { columns: inner.columns, rows })
}

// ============================================================================
// Project
// ============================================================================

fn exec_project(ctx: &mut ExecContext<'_>, input: &Plan, columns: &[ProjectExpr]) -> Result<ExecResult> {
    let inner = execute(input, ctx)?;
    // Compute output columns, expanding `*` and `table.*` to the underlying
    // input column names.
    let mut out_columns: Vec<String> = Vec::new();
    let mut star_expansions: Vec<Vec<String>> = Vec::new(); // for each column, the list of expanded names (or empty)
    for c in columns {
        if let Expr::Column { name, .. } = &c.expr {
            if name == "*" {
                // Expand to all input columns.
                let expanded: Vec<String> = inner.columns.iter().map(|c| {
                    if let Some(pos) = c.rfind('.') {
                        c[pos + 1..].to_string()
                    } else {
                        c.clone()
                    }
                }).collect();
                out_columns.extend(expanded.clone());
                star_expansions.push(expanded);
                continue;
            }
        }
        let display = if let Some(a) = &c.alias {
            a.clone()
        } else {
            expr_display_name(&c.expr, &inner.columns)
        };
        out_columns.push(display);
        star_expansions.push(Vec::new());
    }

    // ---- FAST PATH: pre-resolve bare column references to row indices ----
    //
    // For the overwhelmingly common `SELECT a, b, c FROM ...` shape, every
    // projected expression is a plain `Expr::Column`. Resolving each one
    // through `eval_row` → `EvalContext::lookup` costs a linear scan with
    // case-insensitive string compares PER ROW PER COLUMN (~50-150 ns).
    // We resolve each column to its index in `inner.columns` ONCE, then
    // per row it's a Vec index + a Value clone (~2-5 ns).
    //
    // The resolution order exactly mirrors `EvalContext::lookup`:
    // qualified match first (when the ref carries a table qualifier), then
    // unqualified exact match, then suffix match on "prefix.name" columns.
    let resolved: Vec<Option<usize>> = columns
        .iter()
        .map(|c| match &c.expr {
            Expr::Column { table, name } if name != "*" => {
                resolve_column_index(&inner.columns, table.as_deref(), name)
            }
            _ => None,
        })
        .collect();
    let all_resolved = resolved.iter().all(|r| r.is_some())
        && columns.iter().all(|c| !matches!(&c.expr, Expr::Column { name, .. } if name == "*"));

    let mut out_rows = Vec::with_capacity(inner.rows.len());
    if all_resolved {
        for row in &inner.rows {
            let mut out = Vec::with_capacity(out_columns.len());
            for r in &resolved {
                // unwrap is safe: all_resolved guarantees Some
                out.push(row[r.unwrap()].clone());
            }
            out_rows.push(out);
        }
    } else {
        for row in &inner.rows {
            let mut out = Vec::with_capacity(out_columns.len());
            for (i, c) in columns.iter().enumerate() {
                if let Some(Some(idx)) = resolved.get(i) {
                    out.push(row[*idx].clone());
                    continue;
                }
                if let Expr::Column { name, .. } = &c.expr {
                    if name == "*" {
                        out.extend(row.iter().cloned());
                        continue;
                    }
                }
                out.push(eval_row(&c.expr, row, &inner.columns, &ctx.params, &ctx.named_params)?);
            }
            out_rows.push(out);
        }
    }
    Ok(ExecResult { columns: out_columns.into(), rows: out_rows })
}

/// Resolve a column reference to an index in `col_names`, mirroring
/// `EvalContext::lookup`'s resolution order:
///
/// 1. Qualified (`Some(table)`): look for `"table.column"` (case-insensitive),
///    falling back to the unqualified rules.
/// 2. Unqualified: exact case-insensitive match, then suffix match on
///    `"prefix.name"` columns (e.g. ref `id` matches column `u.id`).
///
/// Returns `None` when the reference can't be statically resolved (caller
/// falls back to the general evaluator, which yields NULL for unknown cols).
fn resolve_column_index(
    col_names: &[String],
    table: Option<&str>,
    name: &str,
) -> Option<usize> {
    if let Some(t) = table {
        let tl = t.to_ascii_lowercase();
        let nl = name.to_ascii_lowercase();
        // Qualified exact match: "table.column".
        for (i, n) in col_names.iter().enumerate() {
            if n.len() == tl.len() + 1 + nl.len() {
                let n_lower = n.to_ascii_lowercase();
                if n_lower.as_str().get(tl.len()..tl.len() + 1) == Some(".")
                    && n_lower[..tl.len()] == tl
                    && &n_lower[tl.len() + 1..] == &nl
                {
                    return Some(i);
                }
            }
        }
        // Fall through to unqualified resolution (mirrors lookup()).
    }
    // Exact match first.
    for (i, n) in col_names.iter().enumerate() {
        if n.eq_ignore_ascii_case(name) {
            return Some(i);
        }
    }
    // Qualified match by suffix (e.g. "u.id" matches ref "id").
    for (i, n) in col_names.iter().enumerate() {
        if let Some(pos) = n.rfind('.') {
            let suffix = &n[pos + 1..];
            if suffix.eq_ignore_ascii_case(name) {
                return Some(i);
            }
        }
    }
    None
}

/// Pretty-print an expression for column header display.
/// For aggregate references (rewritten to `__agg_N`), we look up the
/// original column name from the input's column list.
fn expr_display_name(e: &Expr, input_cols: &[String]) -> String {
    match e {
        Expr::Column { table: _, name } => {
            if name.starts_with("__agg_") {
                // Look up the actual column name from the input — but the input
                // column for an aggregate is also __agg_N. We can't recover the
                // original function name here without more context. The Project's
                // caller should supply an alias.
                name.clone()
            } else if let Some(pos) = name.rfind('.') {
                name[pos + 1..].to_string()
            } else {
                name.clone()
            }
        }
        Expr::Literal(v) => format!("{}", v),
        Expr::Function { name, .. } => format!("{}(...)", name),
        _ => "?".to_string(),
    }
}

// ============================================================================
// Sort
// ============================================================================

fn exec_sort(ctx: &mut ExecContext<'_>, input: &Plan, terms: &[OrderTerm]) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    // Borrow params directly from ctx — `inner` is a local value, and the
    // sort comparator only reads it, so no clone (1-2 allocs) is needed.
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let columns = inner.columns.clone();
    inner.rows.sort_by(|a, b| {
        for term in terms {
            let va = eval_row(&term.expr, a, &columns, params, named_params).unwrap_or(Value::Null);
            let vb = eval_row(&term.expr, b, &columns, params, named_params).unwrap_or(Value::Null);
            let ord = va.cmp(&vb);
            let ord = if term.order == Order::Desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(inner)
}

// ============================================================================
// Limit
// ============================================================================

fn exec_limit(ctx: &mut ExecContext<'_>, input: &Plan, count: &Expr, offset: &Expr) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let count_val = evaluate(count, &eval_ctx)?.as_integer();
    let offset_val = evaluate(offset, &eval_ctx)?.as_integer();
    let offset = offset_val.max(0) as usize;
    if offset >= inner.rows.len() {
        inner.rows.clear();
    } else {
        inner.rows.drain(0..offset);
        if count_val >= 0 {
            let count = count_val as usize;
            if inner.rows.len() > count {
                inner.rows.truncate(count);
            }
        }
    }
    Ok(inner)
}

// ============================================================================
// Aggregate
// ============================================================================

#[derive(Clone, Debug)]
struct AggState {
    count: i64,
    sum: f64,
    sum_is_int: bool,
    int_sum: i64,
    min: Option<Value>,
    max: Option<Value>,
    distinct: std::collections::HashSet<String>,
    concat: String,
    seen_value: bool,
}

impl Default for AggState {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            sum_is_int: true,  // Optimistic: assume int until we see a Real.
            int_sum: 0,
            min: None,
            max: None,
            distinct: std::collections::HashSet::new(),
            concat: String::new(),
            seen_value: false,
        }
    }
}

/// Streaming-aggregate fast path: when the input plan is a bare `Scan`
/// (no Filter, no Project, no Join), iterate the B+tree directly and
/// decode each row into a single reusable buffer.
///
/// This avoids materializing `Vec<Vec<Value>>` in `exec_scan` — which
/// for a 10k-row table is 10k `Vec<Value>` allocations + 10k+ `Value`
/// allocations (one per Text/Blob column per row). The reusable buffer
/// means we allocate ONCE for the row Vec and reuse the inner `Value`s
/// by overwriting them (which is free for Integer/Real/Null; for
/// Text/Blob the old Value gets dropped and the new one allocated
/// in-place, same total allocation count as exec_scan but no per-row
/// Vec allocation overhead).
///
/// If `filter_predicate` is `Some(pred)`, applies the predicate inline
/// (skipping rows that don't match) — handles the
/// `SELECT COUNT(*) FROM t WHERE x > 0` pattern without materializing.
///
/// Returns the same `ExecResult` shape as `exec_aggregate` so the rest
/// of the executor pipeline (Sort, Project, Limit) doesn't notice.
fn exec_aggregate_streaming_scan(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    filter_predicate: Option<&Expr>,
    group_by: &[Expr],
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    // Fast path: no GROUP BY. We can skip the per-row key computation,
    // the per-row key String formatting, and the HashMap lookup. There is
    // exactly one group, so we accumulate directly into a Vec<AggState>.
    //
    // Additionally, if all aggregate args are bare Column references
    // (the common case for `SELECT SUM(col), MAX(col), COUNT(*) FROM t`),
    // we resolve the column index ONCE upfront and read directly from the
    // row buffer at that index — no per-row `eval_row` call (which does a
    // name lookup, type coercion, etc.).
    if group_by.is_empty() {
        return exec_aggregate_no_group_by(ctx, table, alias, filter_predicate, aggregates);
    }

    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();
    let n_cols = table.n_columns();
    // Column names — we have to build these once for the eval_row calls.
    // (We can't avoid this allocation without threading column-name
    // references through EvalContext; that's future work.)
    let prefix = alias.as_deref().unwrap_or(&table.name);
    let columns: Vec<String> = table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect();

    let mut groups: HashMap<String, (Vec<Value>, Vec<AggState>)> = HashMap::new();
    let mut group_order: Vec<String> = Vec::new();

    // Reusable row buffer — avoids per-row Vec allocation.
    let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);

    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    // Borrowed scan — skip per-row Cell::decode Vec allocation.
    bt.scan_table_borrowed(|_rowid, payload| {
        // Decode into the reusable buffer.
        row_buf.clear();
        if decode_row_into(payload, n_cols, &mut row_buf).is_err() {
            return true; // skip corrupt rows
        }
        // Apply the filter predicate inline (if any). Skips rows that
        // don't match — no materialization.
        if let Some(pred) = filter_predicate {
            match eval_row(pred, &row_buf, &columns, &params, &named_params) {
                Ok(v) => {
                    if !v.is_truthy() {
                        return true; // skip — predicate false
                    }
                }
                Err(_) => return true,
            }
        }
        // Compute the group-by key (if any).
        let key: Vec<Value> = match group_by.iter().map(|e| eval_row(e, &row_buf, &columns, &params, &named_params)).collect::<Result<Vec<_>>>() {
            Ok(v) => v,
            Err(_) => return true,
        };
        let key_str = key.iter().map(|v| format!("{:?}", v)).collect::<Vec<_>>().join("|");
        let entry = groups.entry(key_str.clone()).or_insert_with(|| {
            group_order.push(key_str.clone());
            (key.clone(), vec![AggState::default(); aggregates.len()])
        });
        for (i, agg) in aggregates.iter().enumerate() {
            let arg_val = if let Some(arg) = &agg.arg {
                match eval_row(arg, &row_buf, &columns, &params, &named_params) {
                    Ok(v) => v,
                    Err(_) => Value::Null,
                }
            } else {
                Value::Integer(1)
            };
            update_agg_state(&mut entry.1[i], &agg.func, &arg_val, agg.distinct);
        }
        true
    })?;

    // SQLite semantics: empty-table aggregate emits one row.
    if group_by.is_empty() && groups.is_empty() && !aggregates.is_empty() {
        let empty_key: Vec<Value> = Vec::new();
        let empty_states = vec![AggState::default(); aggregates.len()];
        groups.insert(String::new(), (empty_key, empty_states));
        group_order.push(String::new());
    }

    let mut out_rows = Vec::with_capacity(group_order.len());
    for k in group_order {
        if let Some((key, states)) = groups.remove(&k) {
            let mut row = key;
            for (i, agg) in aggregates.iter().enumerate() {
                row.push(finalize_agg(&states[i], &agg.func));
            }
            out_rows.push(row);
        }
    }

    let mut out_cols = Vec::new();
    for (i, g) in group_by.iter().enumerate() {
        let name = match g {
            Expr::Column { table: None, name } => name.clone(),
            Expr::Column { table: Some(t), name } => format!("{}.{}", t, name),
            _ => format!("col{}", i + 1),
        };
        out_cols.push(name);
    }
    for (i, _agg) in aggregates.iter().enumerate() {
        out_cols.push(format!("__agg_{}", i));
    }

    Ok(ExecResult { columns: out_cols.into(), rows: out_rows })
}

/// Vectorized fast path for `SELECT <aggregates> FROM t [WHERE pred]`
/// (no GROUP BY). Key optimizations vs the generic streaming-scan path:
///
/// 1. **No HashMap of groups** — only one group, so we accumulate directly
///    into a `Vec<AggState>`. Saves the per-row String key formatting
///    (which was ~200 ns/row on a 4-column table — 4× Debug-formatted
///    Values + a join("|") allocation).
///
/// 2. **Column index resolution upfront** — if every aggregate's arg is a
///    bare `Expr::Column`, we resolve the column index ONCE before the
///    scan, and during the scan we read `row_buf[idx]` directly (a Vec
///    index, ~1 ns) instead of calling `eval_row` (which does name lookup
///    + type coercion + Result wrapping, ~100 ns/row).
///
/// 3. **No per-row column-name formatting** — we only build the column-name
///    Vec if we actually need it (filter predicate or non-Column aggregate
///    args). For `SELECT SUM(x), COUNT(*) FROM t` (no predicate), we never
///    build it.
///
/// Together these optimizations cut the per-row overhead by ~10x for the
/// common OLAP case, bringing aggregate scan within ~2x of SQLite (from
/// the previous ~6x gap).
fn exec_aggregate_no_group_by(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    filter_predicate: Option<&Expr>,
    aggregates: &[AggExpr],
) -> Result<ExecResult> {
    let n_cols = table.n_columns();
    let prefix = alias.as_deref().unwrap_or(&table.name);

    // Try to resolve each aggregate's arg as a bare Column index.
    // If ALL of them are Columns (or COUNT(*), which has no arg),
    // we can use the index-based fast path. Otherwise, fall back to eval_row.
    let mut agg_col_indices: Vec<Option<usize>> = Vec::with_capacity(aggregates.len());
    let mut all_columns = filter_predicate.is_none();
    for agg in aggregates {
        if let Some(arg) = &agg.arg {
            if let Expr::Column { table: ref_t, name } = arg {
                // Verify the table matches (or is None).
                let matches = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
                if matches {
                    if let Some(idx) = table.find_column(name) {
                        agg_col_indices.push(Some(idx));
                    } else {
                        agg_col_indices.push(None);
                        all_columns = false;
                    }
                } else {
                    agg_col_indices.push(None);
                    all_columns = false;
                }
            } else {
                agg_col_indices.push(None);
                all_columns = false;
            }
        } else {
            // COUNT(*) — no arg, no index needed.
            agg_col_indices.push(None);
        }
    }

    // Build column names only if we need them for eval_row calls.
    let columns: Option<Vec<String>> = if all_columns {
        None
    } else {
        Some(table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect())
    };
    let columns_ref = columns.as_ref();

    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();

    let mut states: Vec<AggState> = (0..aggregates.len()).map(|_| AggState::default()).collect();
    let mut saw_any_row = false;

    // === Selective-decode fast path ===
    // When ALL aggregate args are bare Column refs (or COUNT(*)) AND the
    // filter predicate also only references bare Columns, we can decode
    // only the referenced columns per row instead of the entire row.
    // For `SELECT SUM(val) FROM t` on a 3-col table, we skip decoding 2 cols
    // per row — directly cutting the dominant cost of `decode_row_into`.
    //
    // The filter predicate is allowed to be None (no WHERE clause) — in that
    // case the set of needed columns is just the aggregate args.
    let selective_eligible = agg_col_indices.iter().all(|x| x.is_some())
        && filter_predicate
            .map(|p| expr_only_columns(p, &table, prefix))
            .unwrap_or(true);

    if selective_eligible {
        // Build the sorted, deduped list of column indices we need to decode.
        let mut wanted: Vec<usize> = agg_col_indices.iter().filter_map(|x| *x).collect();
        if let Some(pred) = filter_predicate {
            collect_column_indices(pred, &table, prefix, &mut wanted);
        }
        wanted.sort_unstable();
        wanted.dedup();

        // Build column names only for the wanted columns (used by filter eval).
        let wanted_names: Option<Vec<String>> = if let Some(_) = filter_predicate {
            // The filter eval path needs `col_names` to match row indices.
            // Build the full col_names vec since eval_row expects all cols.
            Some(table.columns.iter().map(|c| format!("{}.{}", prefix, c.name)).collect())
        } else {
            None
        };

        let mut sel_buf: Vec<Value> = Vec::with_capacity(wanted.len());
        let mut full_row_buf: Vec<Value> = Vec::with_capacity(n_cols);

        let root = ctx.table_root(&table);
        let mut bt = Btree::new(ctx.pager, root, false);
        // Use scan_table_borrowed — bypasses Cell::decode's per-row Vec<u8>
        // allocation by passing &[u8] borrows directly into the page buffer.
        // For 10k rows, this saves 10k malloc+free pairs.
        bt.scan_table_borrowed(|_rowid, payload| {
            // Decode only the wanted columns.
            if decode_row_selective(payload, n_cols, &wanted, &mut sel_buf).is_err() {
                return true;
            }
            // Apply the filter predicate, if any. We need a full row buffer
            // because eval_row indexes by column position. The cheap path
            // is to expand sel_buf back into a full row (NULLs for un-wanted
            // columns). For aggregates only on a few cols, this is still
            // cheaper than decode_row_into because we skip the heavy Text/Blob
            // allocations on un-wanted cols (only Integer/Real decoded).
            if let Some(pred) = filter_predicate {
                let cols = wanted_names.as_ref().expect("col_names built when filter is present");
                // Expand into full_row_buf at the correct positions.
                full_row_buf.clear();
                full_row_buf.resize(n_cols, Value::Null);
                for (i, &col_idx) in wanted.iter().enumerate() {
                    if i < sel_buf.len() {
                        full_row_buf[col_idx] = sel_buf[i].clone();
                    }
                }
                match eval_row(pred, &full_row_buf, cols, &params, &named_params) {
                    Ok(v) => {
                        if !v.is_truthy() {
                            return true;
                        }
                    }
                    Err(_) => return true,
                }
            }
            saw_any_row = true;
            for (i, agg) in aggregates.iter().enumerate() {
                let arg_val = if let Some(wanted_pos) = agg_col_indices[i]
                    .as_ref()
                    .and_then(|widx| wanted.iter().position(|x| x == widx))
                {
                    sel_buf[wanted_pos].clone()
                } else if let Some(arg) = &agg.arg {
                    let cols = wanted_names.as_ref().expect("col_names built when not all are Column");
                    // Fall back to eval_row for non-Column args.
                    let mut full_row = vec![Value::Null; n_cols];
                    for (j, &col_idx) in wanted.iter().enumerate() {
                        if j < sel_buf.len() {
                            full_row[col_idx] = sel_buf[j].clone();
                        }
                    }
                    match eval_row(arg, &full_row, cols, &params, &named_params) {
                        Ok(v) => v,
                        Err(_) => Value::Null,
                    }
                } else {
                    Value::Integer(1)
                };
                update_agg_state(&mut states[i], &agg.func, &arg_val, agg.distinct);
            }
            true
        })?;
    } else {
        // Fallback: decode the entire row.
        let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
        let root = ctx.table_root(&table);
        let mut bt = Btree::new(ctx.pager, root, false);
        bt.scan_table_borrowed(|_rowid, payload| {
            row_buf.clear();
            if decode_row_into(payload, n_cols, &mut row_buf).is_err() {
                return true; // skip corrupt rows
            }
            // Apply the filter predicate inline (if any).
            if let Some(pred) = filter_predicate {
                let cols = columns_ref.expect("columns were built because predicate is Some");
                match eval_row(pred, &row_buf, cols, &params, &named_params) {
                    Ok(v) => {
                        if !v.is_truthy() {
                            return true;
                        }
                    }
                    Err(_) => return true,
                }
            }
            saw_any_row = true;
            for (i, agg) in aggregates.iter().enumerate() {
                let arg_val = if let Some(idx) = agg_col_indices[i] {
                    // Fast path: pre-resolved column index.
                    row_buf[idx].clone()
                } else if let Some(arg) = &agg.arg {
                    // Slow path: eval_row (e.g. SUM(x + 1), AVG(x * 2)).
                    let cols = columns_ref.expect("columns were built because not all are Column");
                    match eval_row(arg, &row_buf, cols, &params, &named_params) {
                        Ok(v) => v,
                        Err(_) => Value::Null,
                    }
                } else {
                    Value::Integer(1)
                };
                update_agg_state(&mut states[i], &agg.func, &arg_val, agg.distinct);
            }
            true
        })?;
    }

    // SQLite semantics: empty-table aggregate emits one row with NULLs.
    if !saw_any_row {
        // states remain at default — finalize_agg handles the empty case.
    }

    let mut out_row: Vec<Value> = Vec::with_capacity(aggregates.len());
    for (i, agg) in aggregates.iter().enumerate() {
        out_row.push(finalize_agg(&states[i], &agg.func));
    }
    let out_cols: Vec<String> = aggregates.iter().enumerate().map(|(i, _)| format!("__agg_{}", i)).collect();
    Ok(ExecResult { columns: out_cols.into(), rows: vec![out_row] })
}

/// True iff `expr` only references `Column` references that resolve
/// against `table` (so we can use the selective-decode fast path
/// instead of decoding the entire row). If the expression contains
/// `Parameter`, `Literal`, or other constructs, that's fine — we just
/// need every Column to resolve.
///
/// Used by `exec_aggregate_no_group_by`'s selective-decode fast path:
/// when the WHERE clause only references a few bare columns, we decode
/// only those columns per row instead of the full row.
fn expr_only_columns(expr: &Expr, table: &Table, prefix: &str) -> bool {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Subquery(_) => true,
        Expr::Column { table: ref_t, name } => {
            let matches_t = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
            matches_t && table.find_column(name).is_some()
        }
        Expr::Binary { left, right, .. } => {
            expr_only_columns(left, table, prefix) && expr_only_columns(right, table, prefix)
        }
        Expr::Unary { expr, .. } => expr_only_columns(expr, table, prefix),
        Expr::Between { expr, low, high, .. } => {
            expr_only_columns(expr, table, prefix)
                && expr_only_columns(low, table, prefix)
                && expr_only_columns(high, table, prefix)
        }
        Expr::IsNull { expr, .. } => expr_only_columns(expr, table, prefix),
        Expr::Is { left, right, .. } => {
            expr_only_columns(left, table, prefix) && expr_only_columns(right, table, prefix)
        }
        Expr::Like { expr, pattern, escape, .. } => {
            expr_only_columns(expr, table, prefix)
                && expr_only_columns(pattern, table, prefix)
                && escape.as_ref().map(|e| expr_only_columns(e, table, prefix)).unwrap_or(true)
        }
        Expr::Function { args, .. } => args.iter().all(|a| expr_only_columns(a, table, prefix)),
        Expr::Case { operand, whens, else_ } => {
            operand.as_ref().map(|o| expr_only_columns(o, table, prefix)).unwrap_or(true)
                && whens.iter().all(|(w, t)| expr_only_columns(w, table, prefix) && expr_only_columns(t, table, prefix))
                && else_.as_ref().map(|e| expr_only_columns(e, table, prefix)).unwrap_or(true)
        }
        Expr::Row(exprs) => exprs.iter().all(|e| expr_only_columns(e, table, prefix)),
        // Conservatively, anything else (subqueries with complex sources,
        // IN (SELECT...), EXISTS, etc.) disqualifies the fast path.
        _ => false,
    }
}

/// Collect every column index referenced by `expr` into `out`.
/// Only resolves bare Column refs against `table` (matching name or alias).
/// Used by `exec_aggregate_no_group_by`'s selective-decode fast path.
fn collect_column_indices(expr: &Expr, table: &Table, prefix: &str, out: &mut Vec<usize>) {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Subquery(_) => {}
        Expr::Column { table: ref_t, name } => {
            let matches_t = ref_t.as_ref().map(|t| t == &table.name || t == prefix).unwrap_or(true);
            if matches_t {
                if let Some(idx) = table.find_column(name) {
                    out.push(idx);
                }
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_column_indices(left, table, prefix, out);
            collect_column_indices(right, table, prefix, out);
        }
        Expr::Unary { expr, .. } => collect_column_indices(expr, table, prefix, out),
        Expr::Between { expr, low, high, .. } => {
            collect_column_indices(expr, table, prefix, out);
            collect_column_indices(low, table, prefix, out);
            collect_column_indices(high, table, prefix, out);
        }
        Expr::IsNull { expr, .. } => collect_column_indices(expr, table, prefix, out),
        Expr::Is { left, right, .. } => {
            collect_column_indices(left, table, prefix, out);
            collect_column_indices(right, table, prefix, out);
        }
        Expr::Like { expr, pattern, escape, .. } => {
            collect_column_indices(expr, table, prefix, out);
            collect_column_indices(pattern, table, prefix, out);
            if let Some(e) = escape {
                collect_column_indices(e, table, prefix, out);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_column_indices(a, table, prefix, out);
            }
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand { collect_column_indices(o, table, prefix, out); }
            for (w, t) in whens {
                collect_column_indices(w, table, prefix, out);
                collect_column_indices(t, table, prefix, out);
            }
            if let Some(e) = else_ { collect_column_indices(e, table, prefix, out); }
        }
        Expr::Row(exprs) => {
            for e in exprs { collect_column_indices(e, table, prefix, out); }
        }
        _ => {}
    }
}

fn exec_aggregate(ctx: &mut ExecContext<'_>, input: &Plan, group_by: &[Expr], aggregates: &[AggExpr]) -> Result<ExecResult> {
    // Fast path #0: SELECT COUNT(*) FROM t  (no WHERE, no GROUP BY, single COUNT(*)).
    // Uses `Btree::count_rows` which skips decoding every cell payload —
    // just sums `n_cells` across all leaf pages. For a 10k-row table this is
    // ~10x faster than the streaming scan + decode path.
    if group_by.is_empty()
        && aggregates.len() == 1
        && matches!(input, Plan::Scan { predicate: None, .. })
    {
        if let Plan::Scan { table, .. } = input {
            let agg = &aggregates[0];
            // COUNT(*) — arg is None (the planner emits COUNT(*) with no arg).
            // COUNT(col) — arg is Some(Column). We can't use the fast path
            // for COUNT(col) because we need to skip NULLs, which requires
            // decoding.
            if agg.func == "count" && agg.arg.is_none() && !agg.distinct {
                let root = ctx.table_root(table);
                let mut bt = Btree::new(ctx.pager, root, false);
                let n = bt.count_rows()?;
                let mut row = Vec::with_capacity(1);
                row.push(Value::Integer(n as i64));
                return Ok(ExecResult {
                    columns: Arc::from(vec!["__agg_0".to_string()]),
                    rows: vec![row],
                });
            }
        }
    }
    // Fast path #1: input is a bare Scan.
    // Handles: `SELECT SUM/AVG/MIN/MAX/COUNT(*) FROM t`
    //          `SELECT col, COUNT(*) FROM t GROUP BY col`
    if let Plan::Scan { table, alias, index: None, predicate: None } = input {
        return exec_aggregate_streaming_scan(ctx, table.clone(), alias.clone(), None, group_by, aggregates);
    }
    // Fast path #2: input is Filter(Scan, predicate).
    // Handles: `SELECT COUNT(*) FROM t WHERE val > 5000`
    //          `SELECT col, COUNT(*) FROM t WHERE x > 0 GROUP BY col`
    if let Plan::Filter { input: inner, predicate } = input {
        if let Plan::Scan { table, alias, index: None, predicate: None } = inner.as_ref() {
            return exec_aggregate_streaming_scan(ctx, table.clone(), alias.clone(), Some(predicate), group_by, aggregates);
        }
    }
    let inner = execute(input, ctx)?;
    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();
    let mut groups: HashMap<String, (Vec<Value>, Vec<AggState>)> = HashMap::new();
    let mut group_order: Vec<String> = Vec::new();

    for row in &inner.rows {
        let key: Vec<Value> = group_by.iter().map(|e| eval_row(e, row, &inner.columns, &params, &named_params)).collect::<Result<_>>()?;
        let key_str = key.iter().map(|v| format!("{:?}", v)).collect::<Vec<_>>().join("|");
        let entry = groups.entry(key_str.clone()).or_insert_with(|| {
            group_order.push(key_str.clone());
            (key.clone(), vec![AggState::default(); aggregates.len()])
        });
        for (i, agg) in aggregates.iter().enumerate() {
            let arg_val = if let Some(arg) = &agg.arg {
                eval_row(arg, row, &inner.columns, &params, &named_params)?
            } else {
                Value::Integer(1)
            };
            update_agg_state(&mut entry.1[i], &agg.func, &arg_val, agg.distinct);
        }
    }

    // SQLite semantics: if there is no GROUP BY clause AND no rows were
    // produced by the input, the aggregate still emits ONE row (with
    // COUNT=0, SUM=NULL, AVG=NULL, MIN=NULL, MAX=NULL). This handles the
    // common `SELECT COUNT(*) FROM empty_table` case.
    if group_by.is_empty() && groups.is_empty() && !aggregates.is_empty() {
        let empty_key: Vec<Value> = Vec::new();
        let empty_states = vec![AggState::default(); aggregates.len()];
        groups.insert(String::new(), (empty_key, empty_states));
        group_order.push(String::new());
    }

    let mut out_rows = Vec::with_capacity(group_order.len());
    for k in group_order {
        if let Some((key, states)) = groups.remove(&k) {
            let mut row = key;
            for (i, agg) in aggregates.iter().enumerate() {
                row.push(finalize_agg(&states[i], &agg.func));
            }
            out_rows.push(row);
        }
    }

    let mut out_cols = Vec::new();
    for (i, g) in group_by.iter().enumerate() {
        // Name the group-by output column after the source expression so
        // that downstream Sort / Filter operators can find it by name. For
        // `GROUP BY cat` we name the column "cat"; for `GROUP BY t.cat` we
        // name it "t.cat". For non-column group-by exprs (e.g.
        // `GROUP BY x+y`), fall back to "colN" since there's no obvious name.
        // Without this, ORDER BY cat on an Aggregate plan would fail to
        // resolve the column and sort by NULLs only — surfacing as
        // "GROUP BY with NULL keys puts NULL group in the wrong order".
        let name = match g {
            Expr::Column { table: None, name } => name.clone(),
            Expr::Column { table: Some(t), name } => format!("{}.{}", t, name),
            _ => format!("col{}", i + 1),
        };
        out_cols.push(name);
    }
    for (i, agg) in aggregates.iter().enumerate() {
        // Use a synthetic name that the planner's rewrite_aggregates() can find.
        // The alias (if any) is still used as the display name in the Project.
        let _ = agg.alias;
        let _ = i;
        out_cols.push(format!("__agg_{}", i));
    }

    Ok(ExecResult { columns: out_cols.into(), rows: out_rows })
}

fn update_agg_state(state: &mut AggState, func: &str, v: &Value, distinct: bool) {
    // Only compute the distinct key if we're actually doing a DISTINCT
    // aggregate. For non-DISTINCT aggregates, this skips a per-row
    // `format!("{:?}", v)` String allocation that was the dominant cost
    // of `exec_aggregate_no_group_by` — for a 10k-row scan, that's 10k
    // saved heap allocations, ~3-5 ms on a hot CPU.
    if distinct {
        let key = format!("{:?}", v);
        if !state.distinct.insert(key) {
            return;
        }
    }
    // Only mark "seen_value" for non-NULL inputs. This makes SUM of all
    // NULLs return NULL (matching SQLite), rather than the previous
    // behavior of returning Integer(0) because seen_value was set
    // unconditionally before the per-func match. Bug surfaced by the
    // differential test 'sum_of_all_nulls_is_null'.
    if !v.is_null() {
        state.seen_value = true;
    }
    match func {
        // SQLite semantics: COUNT(*) counts all rows; COUNT(col) counts
        // non-NULL values of col. For COUNT(*) the planner passes a
        // non-NULL placeholder (Value::Integer(1)) as the arg, so checking
        // v.is_null() here correctly skips NULLs for COUNT(col) while
        // still counting every row for COUNT(*). This bug was caught by
        // the SLT test suite (which expected COUNT(val) to be 5 over a
        // 6-row table where one row had val=NULL).
        "count" => {
            if v.is_null() {
                return;
            }
            state.count += 1
        }
        "sum" | "total" => {
            if !v.is_null() {
                if matches!(v, Value::Real(_)) {
                    state.sum_is_int = false;
                    state.sum += v.as_real();
                } else if state.sum_is_int {
                    state.int_sum = state.int_sum.saturating_add(v.as_integer());
                    state.sum = state.int_sum as f64;
                } else {
                    state.sum += v.as_real();
                }
            }
        }
        "avg" => {
            if !v.is_null() {
                state.count += 1;
                state.sum += v.as_real();
            }
        }
        "min" => {
            if !v.is_null() {
                if state.min.is_none() || v < state.min.as_ref().unwrap() {
                    state.min = Some(v.clone());
                }
            }
        }
        "max" => {
            if !v.is_null() {
                if state.max.is_none() || v > state.max.as_ref().unwrap() {
                    state.max = Some(v.clone());
                }
            }
        }
        "group_concat" => {
            if !v.is_null() {
                if !state.concat.is_empty() {
                    state.concat.push(',');
                }
                state.concat.push_str(&v.as_text());
            }
        }
        _ => {}
    }
}

fn finalize_agg(state: &AggState, func: &str) -> Value {
    match func {
        "count" => Value::Integer(state.count),
        "sum" => {
            if !state.seen_value {
                Value::Null
            } else if state.sum_is_int {
                Value::Integer(state.int_sum)
            } else {
                Value::Real(state.sum)
            }
        }
        "total" => Value::Real(state.sum),
        "avg" => {
            if state.count == 0 {
                Value::Null
            } else {
                // Round to a reasonable precision to avoid IEEE-754 noise.
                Value::Real((state.sum / state.count as f64 * 1e10).round() / 1e10)
            }
        }
        "min" => state.min.clone().unwrap_or(Value::Null),
        "max" => state.max.clone().unwrap_or(Value::Null),
        "group_concat" => Value::Text(state.concat.clone()),
        _ => Value::Null,
    }
}

// ============================================================================
// Window functions
// ============================================================================

fn exec_window(ctx: &mut ExecContext<'_>, input: &Plan, windows: &[WindowExpr]) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let params: &[Value] = &ctx.params;
    let named_params = &ctx.named_params;
    let n_rows = inner.rows.len();

    // For each window, compute values for each row.
    let mut extra_cols: Vec<Vec<Value>> = vec![Vec::new(); n_rows];

    for (w_idx, w) in windows.iter().enumerate() {
        // Group rows by partition key, preserving original order.
        let mut partitions: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
        let mut partition_map: HashMap<String, usize> = HashMap::new();
        for (i, row) in inner.rows.iter().enumerate() {
            let key: Vec<Value> = w.partition_by.iter().map(|e| eval_row(e, row, &inner.columns, &params, &named_params)).collect::<Result<_>>()?;
            let key_str = key.iter().map(|v| format!("{:?}", v)).collect::<Vec<_>>().join("|");
            if let Some(&idx) = partition_map.get(&key_str) {
                partitions[idx].1.push(i);
            } else {
                partition_map.insert(key_str, partitions.len());
                partitions.push((key, vec![i]));
            }
        }

        for (_key, indices) in &partitions {
            let mut sorted_indices = indices.clone();
            if !w.order_by.is_empty() {
                sorted_indices.sort_by(|a, b| {
                    let va = eval_row(&w.order_by[0].expr, &inner.rows[*a], &inner.columns, &params, &named_params).unwrap_or(Value::Null);
                    let vb = eval_row(&w.order_by[0].expr, &inner.rows[*b], &inner.columns, &params, &named_params).unwrap_or(Value::Null);
                    let ord = va.cmp(&vb);
                    if w.order_by[0].order == Order::Desc { ord.reverse() } else { ord }
                });
            }

            let mut row_num = 0i64;
            let mut rank = 0i64;
            let mut dense_rank = 0i64;
            let mut prev_key: Option<Vec<Value>> = None;
            let mut count_in_rank = 0i64;
            for (pos_in_partition, &row_idx) in sorted_indices.iter().enumerate() {
                row_num = (pos_in_partition + 1) as i64;
                let row = &inner.rows[row_idx];
                let key: Vec<Value> = w.order_by.iter().map(|t| eval_row(&t.expr, row, &inner.columns, &params, &named_params)).collect::<Result<_>>()?;
                if prev_key.as_ref() != Some(&key) {
                    rank += count_in_rank + 1;
                    count_in_rank = 0;
                    dense_rank += 1;
                }
                count_in_rank += 1;
                prev_key = Some(key);

                let val = compute_window_value(w, row_num, rank, dense_rank, row, &inner.columns, &params, &named_params)?;
                if extra_cols[row_idx].is_empty() {
                    extra_cols[row_idx] = vec![Value::Null; windows.len()];
                }
                extra_cols[row_idx][w_idx] = val;
            }
        }
    }

    // Append window columns to each row.
    for (i, row) in inner.rows.iter_mut().enumerate() {
        if !extra_cols[i].is_empty() {
            row.extend(extra_cols[i].drain(..));
        }
    }

    // Build column names: original + window aliases.
    let mut out_cols: Vec<String> = inner.columns.to_vec();
    for w in windows {
        out_cols.push(w.alias.clone().unwrap_or_else(|| w.display_name.clone()));
    }
    inner.columns = out_cols.into();

    Ok(inner)
}

fn compute_window_value(
    w: &WindowExpr,
    row_num: i64,
    rank: i64,
    dense_rank: i64,
    row: &Row,
    column_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
) -> Result<Value> {
    let eval_ctx = EvalContext::new(row, column_names, params, named_params);
    match w.func.as_str() {
        "row_number" => Ok(Value::Integer(row_num)),
        "rank" => Ok(Value::Integer(rank)),
        "dense_rank" => Ok(Value::Integer(dense_rank)),
        "percent_rank" | "cume_dist" => Ok(Value::Real(0.0)),
        "sum" | "avg" | "min" | "max" | "count" => {
            if let Some(arg) = &w.arg {
                evaluate(arg, &eval_ctx)
            } else {
                Ok(Value::Integer(row_num))
            }
        }
        "lag" | "lead" | "first_value" | "last_value" | "nth_value" => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

// ============================================================================
// Join (nested-loop)
// ============================================================================

fn exec_join(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan, join_type: crate::planner::plan::JoinType, condition: &Option<Expr>) -> Result<ExecResult> {
    let left_res = execute(left, ctx)?;
    let right_res = execute(right, ctx)?;
    let mut combined_cols: Vec<String> = left_res.columns.to_vec();
    combined_cols.extend(right_res.columns.iter().cloned());
    let n_left = left_res.columns.len();
    let n_right = right_res.columns.len();
    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();

    let mut out_rows = Vec::new();
    let mut right_matched = vec![false; right_res.rows.len()];

    for left_row in &left_res.rows {
        let mut matched = false;
        for (ri, right_row) in right_res.rows.iter().enumerate() {
            let mut combined = left_row.clone();
            combined.extend(right_row.clone());
            let ok = if let Some(cond) = condition {
                let v = eval_row(cond, &combined, &combined_cols, &params, &named_params)?;
                v.is_truthy()
            } else {
                true
            };
            if ok {
                out_rows.push(combined);
                matched = true;
                right_matched[ri] = true;
            }
        }
        if !matched && matches!(join_type, crate::planner::plan::JoinType::Left | crate::planner::plan::JoinType::Full) {
            let mut combined = left_row.clone();
            combined.extend(vec![Value::Null; n_right]);
            out_rows.push(combined);
        }
    }

    // RIGHT and FULL: emit unmatched right rows with NULL left.
    if matches!(join_type, crate::planner::plan::JoinType::Right | crate::planner::plan::JoinType::Full) {
        for (ri, right_row) in right_res.rows.iter().enumerate() {
            if !right_matched[ri] {
                let mut combined = vec![Value::Null; n_left];
                combined.extend(right_row.clone());
                out_rows.push(combined);
            }
        }
    }

    Ok(ExecResult { columns: combined_cols.into(), rows: out_rows })
}

/// Hash join: build a hash table on the smaller (right) side, then probe
/// with the left side. Only works for equi-joins where the condition is
/// `left.col = right.col`.
fn exec_hash_join(
    ctx: &mut ExecContext<'_>,
    left: &Plan,
    right: &Plan,
    join_type: crate::planner::plan::JoinType,
    condition: &Option<Expr>,
) -> Result<ExecResult> {
    let left_res = execute(left, ctx)?;
    let right_res = execute(right, ctx)?;
    let mut combined_cols: Vec<String> = left_res.columns.to_vec();
    combined_cols.extend(right_res.columns.iter().cloned());
    let n_left = left_res.columns.len();
    let n_right = right_res.columns.len();
    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();

    // Extract the equi-join keys from the condition.
    // We expect `left.col = right.col` (a single equality or an AND of equalities).
    let eq_pairs = extract_equi_join_keys(condition, &left_res.columns, &right_res.columns);

    if eq_pairs.is_empty() {
        // No equi-join keys — fall back to nested-loop.
        return exec_join(ctx, left, right, join_type, condition);
    }

    // Build a hash table on the smaller side to minimize build cost.
    // For INNER joins we can freely pick which side to build on; for outer
    // joins we must preserve the side that's preserved by the join type, so
    // we fall back to the (correct-but-slower) right-side-build path. This
    // is the common case (real OLTP joins are overwhelmingly inner).
    let is_inner = matches!(join_type, crate::planner::plan::JoinType::Inner | crate::planner::plan::JoinType::Cross);
    let left_is_smaller = left_res.rows.len() <= right_res.rows.len();
    let build_left = is_inner && left_is_smaller;

    // Build the hash on the chosen build side. Key = concatenated `Value::encode()`
    // bytes of the build-side key columns. Vec<u8> implements Hash+Eq natively,
    // and `Value::encode()` produces a stable, collision-resistant form that
    // respects SQL type semantics (Integer/Real/Text/Blob each get a distinct
    // leading tag byte). This replaces the prior `format!("{:?}", v)` approach
    // which allocated a String per value per row — ~30× slower than byte encode
    // for the common Integer case.
    use std::collections::HashMap as StdHashMap;
    let mut hash: StdHashMap<Vec<u8>, Vec<usize>> = StdHashMap::new();

    let (build_rows, build_key_indices): (&Vec<Row>, Vec<usize>) = if build_left {
        // Build on left; eq_pairs gives (left_idx, right_idx), take left_idx.
        let idxs: Vec<usize> = eq_pairs.iter().map(|(l, _)| *l).collect();
        (&left_res.rows, idxs)
    } else {
        // Build on right; take right_idx.
        let idxs: Vec<usize> = eq_pairs.iter().map(|(_, r)| *r).collect();
        (&right_res.rows, idxs)
    };

    for (row_idx, row) in build_rows.iter().enumerate() {
        let mut key: Vec<u8> = Vec::with_capacity(24);
        for &ci in &build_key_indices {
            if let Some(v) = row.get(ci) {
                key.extend_from_slice(&v.encode());
            } else {
                // Missing column — treat as NULL.
                key.push(0);
            }
            // Separator byte to disambiguate adjacent variable-length encodings.
            key.push(0xff);
        }
        hash.entry(key).or_default().push(row_idx);
    }

    let mut out_rows: Vec<Row> = Vec::new();
    let mut build_matched = vec![false; build_rows.len()];

    // Probe with the other (probe) side. Determine which rows to iterate and
    // which key indices to extract from them.
    let (probe_rows, probe_key_indices, probe_is_left): (&Vec<Row>, Vec<usize>, bool) = if build_left {
        // Build was left; probe is right. Take right_idx from eq_pairs.
        let idxs: Vec<usize> = eq_pairs.iter().map(|(_, r)| *r).collect();
        (&right_res.rows, idxs, false)
    } else {
        // Build was right; probe is left. Take left_idx from eq_pairs.
        let idxs: Vec<usize> = eq_pairs.iter().map(|(l, _)| *l).collect();
        (&left_res.rows, idxs, true)
    };

    // Allocate NULL rows for unmatched emission (LEFT/RIGHT/FULL).
    let nulls_right = vec![Value::Null; n_right];
    let nulls_left = vec![Value::Null; n_left];

    // Reuse a single key buffer across all probes to avoid allocating a
    // fresh Vec<u8> per probe row. Previously this allocated ~N times for
    // N probe rows (10000 allocs in the join benchmark → ~1ms of pure alloc
    // overhead). The buffer is cleared and refilled per iteration; capacity
    // is retained so subsequent iterations don't reallocate.
    let mut key_buf: Vec<u8> = Vec::with_capacity(32);

    for probe_row in probe_rows.iter() {
        key_buf.clear();
        for &ci in &probe_key_indices {
            if let Some(v) = probe_row.get(ci) {
                key_buf.extend_from_slice(&v.encode());
            } else {
                key_buf.push(0);
            }
            key_buf.push(0xff);
        }
        let mut matched = false;
        if let Some(candidates) = hash.get(&key_buf) {
            for &bi in candidates {
                let build_row = &build_rows[bi];
                // Emit combined row in [left, right] order regardless of which
                // side was the build side.
                let combined: Row = if probe_is_left {
                    // probe = left, build = right → [probe, build]
                    let mut c = probe_row.clone();
                    c.extend(build_row.clone());
                    c
                } else {
                    // probe = right, build = left → [build, probe]
                    let mut c = build_row.clone();
                    c.extend(probe_row.clone());
                    c
                };
                // Verify the full condition (in case there are non-equi predicates).
                let ok = if let Some(cond) = condition {
                    let v = eval_row(cond, &combined, &combined_cols, &params, &named_params)?;
                    v.is_truthy()
                } else {
                    true
                };
                if ok {
                    out_rows.push(combined);
                    matched = true;
                    build_matched[bi] = true;
                }
            }
        }
        // Unmatched handling for LEFT/RIGHT/FULL joins.
        // If probe is left and the join preserves left (LEFT/FULL), emit [probe, NULLs].
        // If probe is right and the join preserves right (RIGHT/FULL), emit [NULLs, probe].
        if !matched {
            if probe_is_left && matches!(join_type, crate::planner::plan::JoinType::Left | crate::planner::plan::JoinType::Full) {
                let mut c = probe_row.clone();
                c.extend(nulls_right.clone());
                out_rows.push(c);
            } else if !probe_is_left && matches!(join_type, crate::planner::plan::JoinType::Right | crate::planner::plan::JoinType::Full) {
                let mut c = nulls_left.clone();
                c.extend(probe_row.clone());
                out_rows.push(c);
            }
            // For INNER/CROSS, unmatched probe rows are dropped.
        }
    }

    // Emit unmatched build-side rows for the outer-join case where the build
    // side is the preserved side (LEFT preserved by LEFT/FULL if build was left;
    // RIGHT preserved by RIGHT/FULL if build was right).
    if build_left && matches!(join_type, crate::planner::plan::JoinType::Left | crate::planner::plan::JoinType::Full) {
        // Build was left; unmatched left rows are [left, NULLs].
        for (bi, build_row) in build_rows.iter().enumerate() {
            if !build_matched[bi] {
                let mut c = build_row.clone();
                c.extend(nulls_right.clone());
                out_rows.push(c);
            }
        }
    } else if !build_left && matches!(join_type, crate::planner::plan::JoinType::Right | crate::planner::plan::JoinType::Full) {
        // Build was right; unmatched right rows are [NULLs, right].
        for (bi, build_row) in build_rows.iter().enumerate() {
            if !build_matched[bi] {
                let mut c = nulls_left.clone();
                c.extend(build_row.clone());
                out_rows.push(c);
            }
        }
    }

    Ok(ExecResult { columns: combined_cols.into(), rows: out_rows })
}

/// Extract equi-join key column pairs from a join condition.
/// Returns a list of (left_col_index, right_col_index) pairs.
/// Handles a single `col = col` or an AND chain of equalities.
fn extract_equi_join_keys(
    condition: &Option<Expr>,
    left_cols: &[String],
    right_cols: &[String],
) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    if let Some(cond) = condition {
        collect_eq_pairs(cond, left_cols, right_cols, &mut pairs);
    }
    pairs
}

fn collect_eq_pairs(
    expr: &Expr,
    left_cols: &[String],
    right_cols: &[String],
    pairs: &mut Vec<(usize, usize)>,
) {
    match expr {
        Expr::Binary { op: BinaryOp::And, left, right } => {
            collect_eq_pairs(left, left_cols, right_cols, pairs);
            collect_eq_pairs(right, left_cols, right_cols, pairs);
        }
        Expr::Binary { op: BinaryOp::Eq, left, right } => {
            // Try left.col = right.col
            if let (Some(l_idx), Some(r_idx)) = (
                col_index(left, left_cols),
                col_index(right, right_cols),
            ) {
                pairs.push((l_idx, r_idx));
                return;
            }
            // Try right.col = left.col
            if let (Some(r_idx), Some(l_idx)) = (
                col_index(left, right_cols),
                col_index(right, left_cols),
            ) {
                pairs.push((l_idx, r_idx));
            }
        }
        _ => {}
    }
}

/// Find the index of a column reference in a column list.
/// Handles both qualified (`alias.col`) and unqualified (`col`) references.
///
/// **Qualifier semantics** (matches SQLite): when the expression carries a
/// table qualifier (e.g. `b.id`), we ONLY match columns whose prefix matches
/// that qualifier. This prevents `b.id` from spuriously matching `c.id` in a
/// 3-way join where both sides have a column named `id`.
fn col_index(expr: &Expr, cols: &[String]) -> Option<usize> {
    if let Expr::Column { table, name } = expr {
        match table {
            // Qualified reference: only consider columns whose table prefix
            // matches `table`.
            Some(t) => {
                for (i, c) in cols.iter().enumerate() {
                    // c is typically "table.column" — split off the prefix.
                    if let Some(pos) = c.rfind('.') {
                        let (prefix, col_name) = (&c[..pos], &c[pos + 1..]);
                        if prefix.eq_ignore_ascii_case(t)
                            && col_name.eq_ignore_ascii_case(name)
                        {
                            return Some(i);
                        }
                    }
                }
                None
            }
            // Unqualified: exact match first (handles names like "id" against
            // "id" — but our cols are usually "alias.id"). Then suffix match.
            None => {
                // Exact match on the whole column string.
                for (i, c) in cols.iter().enumerate() {
                    if c.eq_ignore_ascii_case(name) {
                        return Some(i);
                    }
                }
                // Suffix match (e.g. "u.id" matches "id"). On ambiguity, the
                // FIRST match wins — SQLite's behaviour is similar.
                for (i, c) in cols.iter().enumerate() {
                    if let Some(pos) = c.rfind('.') {
                        if c[pos + 1..].eq_ignore_ascii_case(name) {
                            return Some(i);
                        }
                    }
                }
                None
            }
        }
    } else {
        None
    }
}

// ============================================================================
// Index nested-loop join
// ============================================================================

/// Index nested-loop join: for each outer row, look up matching inner rows
/// via the inner table's secondary index. This is the optimal plan for
/// `JOIN inner ON outer.k = inner.k` when `inner.k` is indexed and the
/// outer side is highly selective (e.g. filtered by `WHERE outer.k = ?`).
///
/// Unlike `exec_hash_join`, this never materializes the full inner table —
/// it only fetches the inner rows that actually match an outer row. For
/// the canonical case `SELECT ... FROM users u JOIN orders o ON u.id =
/// o.user_id WHERE u.id = ?` with `idx_orders_user` on `orders(user_id)`,
/// this means ~10 inner-row lookups instead of decoding all 10k orders.
fn exec_index_nested_loop_join(
    ctx: &mut ExecContext<'_>,
    outer_plan: &Plan,
    inner_table: Arc<Table>,
    inner_alias: Option<String>,
    inner_index: Arc<crate::schema::Index>,
    outer_key_col: usize,
) -> Result<ExecResult> {
    let outer_res = execute(outer_plan, ctx)?;
    let n_inner_cols = inner_table.n_columns();

    // Output columns: outer.cols ++ inner.cols (with inner prefix from alias).
    let inner_prefix = inner_alias.as_deref().unwrap_or(&inner_table.name);
    let mut combined_cols: Vec<String> = outer_res.columns.to_vec();
    combined_cols.extend(
        inner_table.columns.iter().map(|c| format!("{}.{}", inner_prefix, c.name)),
    );

    let mut out_rows: Vec<Row> = Vec::new();
    let inner_root = ctx.table_root(&inner_table);

    for outer_row in &outer_res.rows {
        // Extract the join key from the outer row.
        let key_value = match outer_row.get(outer_key_col) {
            Some(v) => v.clone(),
            None => continue, // NULL join key — no matches in INNER join.
        };

        // Encode the key for index lookup (order-preserving form).
        let key_bytes = key_value.encode_order_key();

        // Look up matching rowids in the index B+tree.
        let mut index_bt = Btree::new(ctx.pager, inner_index.root_page, true);
        let rowids = index_bt.lookup_index(&key_bytes)?;

        // Fetch each matching row from the inner table. Deduplicate rowids
        // defensively — the index B+tree may (in pathological cases) contain
        // duplicate entries if an UPDATE's index-maintenance delete missed
        // the cell (older versions of the delete path didn't handle index
        // page types). Deduplication here guarantees correctness even when
        // the index is in an inconsistent state.
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for rowid in rowids {
            if !seen.insert(rowid) {
                continue;
            }
            let mut table_bt = Btree::new(ctx.pager, inner_root, false);
            if let LookupResult::Found(payload) = table_bt.lookup_table(rowid)? {
                if let Ok(inner_row) = decode_row(&payload, n_inner_cols) {
                    let mut combined = outer_row.clone();
                    combined.extend(inner_row);
                    out_rows.push(combined);
                }
            }
        }
    }

    Ok(ExecResult { columns: combined_cols.into(), rows: out_rows })
}

// ============================================================================
// Distinct
// ============================================================================

fn exec_distinct(ctx: &mut ExecContext<'_>, input: &Plan) -> Result<ExecResult> {
    let mut inner = execute(input, ctx)?;
    let mut seen: Vec<Row> = Vec::new();
    inner.rows.retain(|r| {
        if seen.contains(r) {
            false
        } else {
            seen.push(r.clone());
            true
        }
    });
    Ok(inner)
}

// ============================================================================
// Set operations
// ============================================================================

fn exec_union(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan, all: bool) -> Result<ExecResult> {
    let l = execute(left, ctx)?;
    let r = execute(right, ctx)?;
    let columns = l.columns.clone();
    let mut rows = l.rows;
    if all {
        rows.extend(r.rows);
    } else {
        let mut seen: Vec<Row> = Vec::new();
        for row in rows.iter().chain(r.rows.iter()) {
            if !seen.contains(row) {
                seen.push(row.clone());
            }
        }
        rows = seen;
    }
    Ok(ExecResult { columns, rows })
}

fn exec_intersect(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan) -> Result<ExecResult> {
    let l = execute(left, ctx)?;
    let r = execute(right, ctx)?;
    let columns = l.columns.clone();
    let mut seen: Vec<Row> = Vec::new();
    for row in &l.rows {
        if r.rows.contains(row) && !seen.contains(row) {
            seen.push(row.clone());
        }
    }
    Ok(ExecResult { columns, rows: seen })
}

fn exec_except(ctx: &mut ExecContext<'_>, left: &Plan, right: &Plan) -> Result<ExecResult> {
    let l = execute(left, ctx)?;
    let r = execute(right, ctx)?;
    let columns = l.columns.clone();
    let mut seen: Vec<Row> = Vec::new();
    for row in &l.rows {
        if !r.rows.contains(row) && !seen.contains(row) {
            seen.push(row.clone());
        }
    }
    Ok(ExecResult { columns, rows: seen })
}

// ============================================================================
// RowidLookup
// ============================================================================

fn exec_rowid_lookup(ctx: &mut ExecContext<'_>, table: Arc<Table>, rowid_expr: &Expr) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let rowid = evaluate(rowid_expr, &eval_ctx)?.as_integer();
    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let row = match bt.lookup_table(rowid)? {
        LookupResult::Found(payload) => decode_row(&payload, table.n_columns())?,
        LookupResult::NotFound => return Ok(ExecResult {
            columns: table.col_names.clone(),
            rows: Vec::new(),
        }),
    };
    Ok(ExecResult {
        columns: table.col_names.clone(),
        rows: vec![row],
    })
}

// ============================================================================
// RowidRange (WHERE id BETWEEN ? AND ?, id > ?, id < ?, id >= ?, id <= ?)
// ============================================================================

fn exec_rowid_range(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    start_expr: Option<&Expr>,
    end_expr: Option<&Expr>,
    residual: Option<&Expr>,
) -> Result<ExecResult> {
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);

    // Evaluate the bounds. None means unbounded (-∞ or +∞).
    let start: i64 = match start_expr {
        Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
        None => i64::MIN,
    };
    let end: i64 = match end_expr {
        Some(e) => evaluate(e, &eval_ctx)?.as_integer(),
        None => i64::MAX,
    };

    let root = ctx.table_root(&table);
    let mut bt = Btree::new(ctx.pager, root, false);
    let n_cols = table.n_columns();
    let mut rows: Vec<Row> = Vec::new();
    // `scan_table_range` is inclusive on both ends. For strict `>` / `<`
    // conjuncts, the planner kept a residual predicate that re-checks the
    // strict comparison; we apply it here so we don't accidentally include
    // the boundary.
    // Use borrowed scan — skip per-row Cell::decode Vec<u8> allocation.
    // decode_row itself still allocates a Vec<Value> per row (unavoidable
    // without restructuring the API to return iterators), but the payload
    // borrow eliminates one allocation per row.
    bt.scan_table_range_borrowed(start, end, |_rowid, payload| {
        if let Ok(row) = decode_row(payload, n_cols) {
            rows.push(row);
        }
        true
    })?;

    // Apply residual predicate (strict < / > bounds, or additional filters).
    // Cached plain column names — one refcount bump, no per-query clones.
    let columns: Arc<[String]> = table.col_names.clone();
    if let Some(res) = residual {
        let params: &[Value] = &ctx.params;
        let named_params = &ctx.named_params;
        rows.retain(|row| {
            match eval_row(res, row, &columns, params, named_params) {
                Ok(v) => v.is_truthy(),
                Err(_) => false,
            }
        });
    }

    Ok(ExecResult { columns, rows })
}

// ============================================================================
// IndexLookup (WHERE indexed_col = ?)
// ============================================================================

fn exec_index_lookup(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    index: Arc<crate::schema::Index>,
    key_exprs: &[Expr],
) -> Result<ExecResult> {
    // Evaluate the key expressions to get the lookup values.
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let key_values: Vec<Value> = key_exprs.iter()
        .map(|e| evaluate(e, &eval_ctx))
        .collect::<Result<_>>()?;

    // Encode the key: concatenate the order-preserving encoded form of each
    // indexed column value (must match encode_index_key's encoding).
    let mut key_bytes = Vec::new();
    for v in &key_values {
        v.encode_order_key_into(&mut key_bytes);
    }

    // Look up matching rowids in the index.
    let mut index_bt = Btree::new(ctx.pager, index.root_page, true);
    let rowids = index_bt.lookup_index(&key_bytes)?;

    // Fetch each row by rowid from the table B+tree.
    let mut rows = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        let mut table_bt = Btree::new(ctx.pager, table.root_page, false);
        if let LookupResult::Found(payload) = table_bt.lookup_table(rowid)? {
            rows.push(decode_row(&payload, table.n_columns())?);
        }
    }

    Ok(ExecResult {
        columns: table.columns.iter().map(|c| c.name.clone()).collect(),
        rows,
    })
}

/// Compare an index entry key against a bound's encoded value, considering
/// only the FIRST indexed column's worth of bytes (the bound constrains the
/// leading column; composite keys with a matching prefix compare Equal).
fn index_key_prefix_cmp(cell_key: &[u8], bound: &[u8]) -> std::cmp::Ordering {
    let n = cell_key.len().min(bound.len());
    match cell_key[..n].cmp(&bound[..n]) {
        std::cmp::Ordering::Equal if cell_key.len() >= bound.len() => std::cmp::Ordering::Equal,
        other => other,
    }
}

/// Execute an IndexRange: scan the index B+tree between the (encoded) bounds,
/// collect the matching rowids, fetch the rows by rowid, and apply any
/// residual predicate. Rows come back in index order (ascending by the
/// indexed value), matching SQLite's index-scan output order.
#[allow(clippy::type_complexity)]
fn exec_index_range(
    ctx: &mut ExecContext<'_>,
    table: Arc<Table>,
    alias: Option<String>,
    index: Arc<crate::schema::Index>,
    start: Option<&(Expr, bool)>,
    end: Option<&(Expr, bool)>,
    residual: Option<&Expr>,
) -> Result<ExecResult> {
    // Evaluate the bound expressions (they are constants / parameters).
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();
    let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
    let start_key: Option<(Vec<u8>, bool)> = match start {
        Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
        None => None,
    };
    let end_key: Option<(Vec<u8>, bool)> = match end {
        Some((e, inc)) => Some((evaluate(e, &eval_ctx)?.encode_order_key(), *inc)),
        None => None,
    };

    // Scan the index from the start bound.
    let scan_start: Vec<u8> = start_key.as_ref().map(|(k, _)| k.clone()).unwrap_or_default();
    let mut rowids: Vec<i64> = Vec::new();
    {
        let mut index_bt = Btree::new(ctx.pager, index.root_page, true);
        index_bt.scan_index_from(&scan_start, |rowid, cell_key| {
            // Exclusive lower bound: skip entries whose leading column
            // equals the bound (they share the bound's key prefix).
            if let Some((k, false)) = &start_key {
                if cell_key.starts_with(k) {
                    return true;
                }
            }
            // Upper bound: stop past it.
            if let Some((k, inc)) = &end_key {
                match index_key_prefix_cmp(cell_key, k) {
                    std::cmp::Ordering::Greater => return false,
                    std::cmp::Ordering::Equal if !*inc => return false,
                    _ => {}
                }
            }
            rowids.push(rowid);
            true
        })?;
    }

    // Fetch the rows by rowid and apply the residual predicate.
    // Cached qualified names — one refcount bump instead of N `format!()`s.
    let columns: Arc<[String]> = if alias.as_deref().unwrap_or(&table.name) == table.name {
        table.qualified_col_names.clone()
    } else {
        let prefix = alias.as_deref().unwrap_or(&table.name);
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect::<Vec<String>>()
            .into()
    };
    let plain_names: Arc<[String]> = table.col_names.clone();
    let mut rows = Vec::with_capacity(rowids.len());
    for rowid in rowids {
        let mut table_bt = Btree::new(ctx.pager, table.root_page, false);
        if let LookupResult::Found(payload) = table_bt.lookup_table(rowid)? {
            let row = decode_row(&payload, table.n_columns())?;
            if let Some(pred) = residual {
                let v = eval_row(pred, &row, &plain_names, &ctx.params, &ctx.named_params)?;
                if !v.is_truthy() {
                    continue;
                }
            }
            rows.push(row);
        }
    }

    Ok(ExecResult { columns, rows })
}

// ============================================================================
// INSERT
// ============================================================================

fn exec_insert(ctx: &mut ExecContext<'_>, table: Arc<Table>, source: &Plan, columns: Option<Vec<usize>>, on_conflict: ConflictResolution, upsert: Option<&crate::sql::ast::UpsertClause>, returning: Option<&[crate::sql::ast::ResultColumn]>) -> Result<ExecResult> {
    let target_indices: Vec<usize> = columns.unwrap_or_else(|| (0..table.n_columns()).collect());
    // Track the current root page — it may change if the B+tree splits.
    let mut current_root = ctx.table_root(&table);
    let mut max_rowid = ctx.get_or_scan_max_rowid(&table)?;
    let mut inserted = 0i64;

    // Look up indexes on this table once, up front.
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    // Track current root for each index too.
    let mut index_roots: Vec<(Arc<crate::schema::Index>, u32)> = indexes
        .iter().map(|idx| (idx.clone(), idx.root_page)).collect();

    // Pre-compute the lower-cased table name ONCE — set_table_root and
    // set_max_rowid both call to_ascii_lowercase() per call, which
    // allocates a fresh String per row. For a 1k-row INSERT batch, that's
    // 2k wasted String allocations.
    let table_name_lc = table.name.to_ascii_lowercase();

    // Reusable row + payload buffers. Hoisted outside the loop to avoid
    // a per-row Vec allocation. full_row is sized to table.n_columns() and
    // filled with NULL at the start of each iteration.
    let n_cols = table.n_columns();
    let mut full_row: Vec<Value> = vec![Value::Null; n_cols];
    let mut payload_buf: Vec<u8> = Vec::with_capacity(n_cols * 8);
    let empty_row: Vec<Value> = Vec::new();
    let empty_cols: Vec<String> = Vec::new();

    // Column names — needed only for CHECK constraints or RETURNING
    // (NOT NULL checks are purely positional, so the common case of a
    // table with a NOT NULL PK but no CHECKs skips the per-statement
    // Vec<String> allocation entirely).
    let needs_col_names = !table.check_exprs.is_empty() || returning.is_some();
    let col_names: Vec<String> = if needs_col_names {
        table.columns.iter().map(|c| c.name.clone()).collect()
    } else {
        Vec::new()
    };

    // RETURNING output buffer.
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();

    // Fast path: source is a Plan::Values. Skip exec_values' column-name
    // String allocations (which INSERT doesn't need) and the intermediate
    // Vec<Vec<Value>> allocation. For a 1k-statement INSERT batch (1 row
    // per statement), this saves 1k Vec<String> + 2k String allocations
    // (column1/column2) + 1k Vec<Vec<Value>> overheads.
    if let Plan::Values { rows: expr_rows } = source {
        for exprs in expr_rows {
            // Reset the row buffer to all-NULLs without releasing capacity.
            for v in full_row.iter_mut() {
                *v = Value::Null;
            }
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
            for (i, expr) in exprs.iter().enumerate() {
                if i < target_indices.len() {
                    let col_idx = target_indices[i];
                    let val = evaluate(expr, &eval_ctx)?;
                    let affinity = table.columns[col_idx].affinity;
                    full_row[col_idx] = affinity.coerce(val);
                }
            }
            // Apply column defaults.
            for (i, col) in table.columns.iter().enumerate() {
                if full_row[i].is_null() && col.default.is_some() {
                    if let Some(default_expr) = &col.default {
                        let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
                        full_row[i] = evaluate(default_expr, &eval_ctx)?;
                    }
                }
            }

            // Assign the rowid BEFORE constraint enforcement so that a NULL
            // rowid-alias (auto-assign) doesn't trip NOT NULL (SQLite treats
            // NULL on INTEGER PRIMARY KEY as "allocate a new rowid").
            let mut rowid_autogen = false;
            if let Some(idx) = table.rowid_alias {
                if full_row[idx].is_null() {
                    max_rowid += 1;
                    full_row[idx] = Value::Integer(max_rowid);
                    rowid_autogen = true;
                }
            }

            // NOT NULL + CHECK constraints. Always enforced: the NOT NULL
            // loop is purely positional (no names needed); col_names is
            // only consulted for CHECK expressions (empty when absent).
            enforce_row_constraints(&table, &full_row, &col_names, &ctx.params, &ctx.named_params)?;

            let outcome = exec_insert_one_row(
                ctx, &table, &table_name_lc, &mut current_root, &mut max_rowid,
                &mut full_row, &mut payload_buf, &mut index_roots, on_conflict, upsert, rowid_autogen,
            )?;
            match outcome {
                InsertOutcome::Inserted => {
                    inserted += 1;
                    if let Some(ret) = returning {
                        returning_rows.push(project_returning_row(ret, &full_row, &col_names, &ctx.params, &ctx.named_params)?);
                    }
                }
                InsertOutcome::UpdatedExisting => {
                    inserted += 1;
                    if let Some(ret) = returning {
                        returning_rows.push(project_returning_row(ret, &full_row, &col_names, &ctx.params, &ctx.named_params)?);
                    }
                }
                InsertOutcome::Skipped => {}
            }
        }
        if !ctx.in_transaction && !ctx.deferred_flush {
            ctx.pager.flush()?;
        }
        return Ok(finish_insert_result(inserted, returning, &col_names, returning_rows));
    }

    // Slow path: source is something else (subquery, etc.) — go through
    // the generic execute() path.
    let source_res = execute(source, ctx)?;
    for row in &source_res.rows {
        // Reset the row buffer to all-NULLs without releasing capacity.
        // `Value::Null` is a no-op to drop, so this is just a memset.
        for v in full_row.iter_mut() {
            *v = Value::Null;
        }
        for (i, val) in row.iter().enumerate() {
            if i < target_indices.len() {
                let col_idx = target_indices[i];
                let affinity = table.columns[col_idx].affinity;
                full_row[col_idx] = affinity.coerce(val.clone());
            }
        }

        // Apply column defaults.
        for (i, col) in table.columns.iter().enumerate() {
            if full_row[i].is_null() && col.default.is_some() {
                let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &ctx.params, &ctx.named_params);
                if let Some(default_expr) = &col.default {
                    full_row[i] = evaluate(default_expr, &eval_ctx)?;
                }
            }
        }

        // Assign the rowid BEFORE constraint enforcement (see fast path).
        let mut rowid_autogen = false;
        if let Some(idx) = table.rowid_alias {
            if full_row[idx].is_null() {
                max_rowid += 1;
                full_row[idx] = Value::Integer(max_rowid);
                rowid_autogen = true;
            }
        }

        // NOT NULL + CHECK constraints (see fast path).
        enforce_row_constraints(&table, &full_row, &col_names, &ctx.params, &ctx.named_params)?;

        let outcome = exec_insert_one_row(
            ctx, &table, &table_name_lc, &mut current_root, &mut max_rowid,
            &mut full_row, &mut payload_buf, &mut index_roots, on_conflict, upsert, rowid_autogen,
        )?;
        match outcome {
            InsertOutcome::Inserted | InsertOutcome::UpdatedExisting => {
                inserted += 1;
                if let Some(ret) = returning {
                    returning_rows.push(project_returning_row(ret, &full_row, &col_names, &ctx.params, &ctx.named_params)?);
                }
            }
            InsertOutcome::Skipped => {}
        }
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    Ok(finish_insert_result(inserted, returning, &col_names, returning_rows))
}

/// Build the final ExecResult for an INSERT (with or without RETURNING).
fn finish_insert_result(
    inserted: i64,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
    col_names: &[String],
    returning_rows: Vec<Vec<Value>>,
) -> ExecResult {
    if let Some(ret) = returning {
        ExecResult {
            columns: returning_column_names(ret, col_names).into(),
            rows: returning_rows,
        }
    } else {
        ExecResult {
            columns: Arc::from(vec!["inserted".to_string()]),
            rows: vec![vec![Value::Integer(inserted)]],
        }
    }
}

/// What happened to one row in an INSERT statement.
#[derive(Clone, Copy, PartialEq)]
enum InsertOutcome {
    /// A brand-new row was inserted.
    Inserted,
    /// UPSERT DO UPDATE modified an existing row.
    UpdatedExisting,
    /// The row was skipped (OR IGNORE / DO NOTHING).
    Skipped,
}

/// Insert a single row into the table B+tree + maintain indexes.
/// Returns the `InsertOutcome` describing what happened (inserted /
/// upsert-updated / skipped).
///
/// Extracted from exec_insert's loop body so the fast-path (Plan::Values)
/// and slow-path (generic source) can both call it.
///
/// On UPSERT DO UPDATE, `full_row` is REPLACED with the merged (updated)
/// row so the caller can project RETURNING from it.
fn exec_insert_one_row(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    table_name_lc: &str,
    current_root: &mut u32,
    max_rowid: &mut i64,
    full_row: &mut Vec<Value>,
    payload_buf: &mut Vec<u8>,
    index_roots: &mut Vec<(Arc<crate::schema::Index>, u32)>,
    on_conflict: ConflictResolution,
    upsert: Option<&crate::sql::ast::UpsertClause>,
    rowid_autogen_hint: bool,
) -> Result<InsertOutcome> {
    // Compute rowid. `rowid_was_autogenerated` tracks whether WE assigned
    // the rowid (vs. the user providing it explicitly). This is used below
    // to skip the redundant `lookup_table` check when we KNOW the rowid is
    // new, and to take the BTREE_APPEND fast path (right-most descent +
    // leaf append without a binary search) — the single biggest win for
    // bulk sequential inserts.
    //
    // The rowid may have been pre-assigned by the caller (for constraint
    // enforcement); `rowid_autogen_hint` says whether that happened.
    let rowid_was_autogenerated;
    let rowid = if let Some(idx) = table.rowid_alias {
        match &full_row[idx] {
            Value::Integer(i) => {
                rowid_was_autogenerated = rowid_autogen_hint;
                *i
            }
            Value::Null => {
                *max_rowid += 1;
                full_row[idx] = Value::Integer(*max_rowid);
                rowid_was_autogenerated = true;
                *max_rowid
            }
            _ => return Err(Error::semantic("rowid alias column must be an integer or NULL")),
        }
    } else {
        *max_rowid += 1;
        rowid_was_autogenerated = true;
        *max_rowid
    };

    // Determine which constraint the UPSERT target refers to:
    //  - `Some(i)` — the unique index at position i
    //  - `RowidPk` — the INTEGER PRIMARY KEY (rowid)
    //  - `Any`     — empty target list (any uniqueness constraint)
    enum UpsertTarget {
        Any,
        RowidPk,
        Index(usize),
    }
    let upsert_target = match upsert {
        Some(u) if u.target.is_empty() => UpsertTarget::Any,
        Some(u) => {
            let idx_pos = index_roots.iter().position(|(idx, _)| {
                idx.unique
                    && idx.columns.len() == u.target.len()
                    && u.target.iter().all(|t| {
                        idx.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&t.name))
                    })
            });
            match idx_pos {
                Some(i) => UpsertTarget::Index(i),
                None => {
                    // Single-column target on the rowid alias (INTEGER PK)?
                    if u.target.len() == 1 {
                        if let Some(col_idx) = table.find_column(&u.target[0].name) {
                            if table.rowid_alias == Some(col_idx) {
                                UpsertTarget::RowidPk
                            } else {
                                return Err(Error::semantic(
                                    "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
                                ));
                            }
                        } else {
                            return Err(Error::semantic(
                                "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
                            ));
                        }
                    } else {
                        return Err(Error::semantic(
                            "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
                        ));
                    }
                }
            }
        }
        None => UpsertTarget::Any,
    };

    // UNIQUE-index constraint check: before touching the table btree, look
    // up the new row's key in every UNIQUE index on this table. If any
    // index already contains the key, the row is a duplicate and we apply
    // the configured conflict resolution (IGNORE → skip, REPLACE → delete
    // the conflicting row first, ABORT/FAIL/ROLLBACK → error,
    // UPSERT → DO NOTHING / DO UPDATE).
    let mut conflict_rowid: Option<i64> = None;
    let mut conflict_on_target = false;
    for (i, (idx, idx_root)) in index_roots.iter().enumerate() {
        if !idx.unique {
            continue;
        }
        let key_bytes = encode_index_key(idx, table, full_row);
        let mut ibt = Btree::new(ctx.pager, *idx_root, true);
        let matches = ibt.lookup_index(&key_bytes)?;
        if !matches.is_empty() {
            conflict_rowid = Some(matches[0]);
            conflict_on_target = matches!(
                upsert_target,
                UpsertTarget::Any | UpsertTarget::Index(_)
            ) && (matches!(upsert_target, UpsertTarget::Any) || {
                if let UpsertTarget::Index(t) = &upsert_target { *t == i } else { false }
            });
            break;
        }
    }
    if let Some(existing_rowid) = conflict_rowid {
        // UPSERT path: conflicts on the targeted index take the upsert
        // action; conflicts on OTHER constraints are plain errors.
        if conflict_on_target {
            if let Some(u) = upsert {
                return exec_upsert_row(
                    ctx, table, table_name_lc, current_root, full_row, payload_buf,
                    index_roots, existing_rowid, u,
                );
            }
        }
        match on_conflict {
            ConflictResolution::Ignore => return Ok(InsertOutcome::Skipped),
            ConflictResolution::Replace => {
                // Delete the conflicting row from the table and from
                // all indexes (we'll re-insert the new row below).
                let mut bt = Btree::new(ctx.pager, *current_root, false);
                let old_payload_opt = match bt.lookup_table(existing_rowid)? {
                    LookupResult::Found(p) => Some(p),
                    LookupResult::NotFound => None,
                };
                bt.delete_table(existing_rowid)?;
                *current_root = bt.root;
                ctx.set_table_root_lc(table_name_lc, *current_root);
                if let Some(old_payload) = old_payload_opt {
                    if let Ok(old_row) = decode_row(&old_payload, table.n_columns()) {
                        for (idx, idx_root) in index_roots.iter_mut() {
                            let old_key = encode_index_key(idx, table, &old_row);
                            let mut ibt = Btree::new(ctx.pager, *idx_root, true);
                            ibt.delete_index(&old_key, existing_rowid)?;
                            *idx_root = ibt.root;
                        }
                    }
                }
            }
            _ => return Err(Error::semantic(format!(
                "UNIQUE constraint failed: {}",
                table.name
            ))),
        }
    }

    // Reuse the hoisted payload_buf. encode_row_into clears it first.
    encode_row_into(full_row, payload_buf);
    let payload: &[u8] = payload_buf;
    let old_payload_opt;
    {
        let mut bt = Btree::new(ctx.pager, *current_root, false);
        // Optimization: when the rowid was auto-generated (we just
        // assigned max_rowid+1 ourselves), it CANNOT collide with an
        // existing row — the table's rowids are a strict subset of
        // integers up to max_rowid, and max_rowid+1 is by definition not
        // in that set. So we can skip the lookup_table call entirely
        // (saving ~50% of exec_insert's per-row time on multi-row
        // VALUES batches) and go straight to insert_table.
        //
        // We still need the slow path when the user explicitly provided
        // a rowid (it might already exist), or when the conflict
        // resolution is REPLACE (we need to know whether to delete an
        // existing row first).
        if rowid_was_autogenerated && on_conflict != ConflictResolution::Replace {
            old_payload_opt = None;
            // BTREE_APPEND fast path: for sequential auto-rowids,
            // skip the binary search per insert. Falls back to the
            // normal path automatically if the leaf is full or the
            // rowid is not actually an append.
            bt.insert_table_append(rowid, payload)?;
        } else {
            // Single lookup_table call (was 2 before — redundant when
            // the rowid existed and we needed the old payload for the
            // REPLACE index cleanup path).
            let lookup = bt.lookup_table(rowid)?;
            match lookup {
                LookupResult::Found(existing_payload) => {
                    // Rowid conflict. UPSERT applies when the target is
                    // empty (any constraint) or the rowid PK itself.
                    if let Some(u) = upsert {
                        if matches!(upsert_target, UpsertTarget::Any | UpsertTarget::RowidPk) {
                            drop_payload(existing_payload);
                            return exec_upsert_row(
                                ctx, table, table_name_lc, current_root, full_row, payload_buf,
                                index_roots, rowid, u,
                            );
                        }
                    }
                    match on_conflict {
                        ConflictResolution::Replace => {
                            old_payload_opt = Some(existing_payload);
                            bt.delete_table(rowid)?;
                            bt.insert_table(rowid, payload)?;
                        }
                        ConflictResolution::Ignore => return Ok(InsertOutcome::Skipped),
                        _ => return Err(Error::semantic(format!("UNIQUE constraint failed: rowid={}", rowid))),
                    }
                }
                LookupResult::NotFound => {
                    old_payload_opt = None;
                    bt.insert_table(rowid, payload)?;
                }
            }
        }
        // Track the (possibly new) root page.
        *current_root = bt.root;
        ctx.set_table_root_lc(table_name_lc, *current_root);
    }
    // On conflict: delete the old row's index entries first.
    if let Some(old_payload) = old_payload_opt {
        if !index_roots.is_empty() {
            if let Ok(old_row) = decode_row(&old_payload, table.n_columns()) {
                for (idx, idx_root) in index_roots.iter_mut() {
                    let old_key = encode_index_key(idx, table, &old_row);
                    let mut ibt = Btree::new(ctx.pager, *idx_root, true);
                    ibt.delete_index(&old_key, rowid)?;
                    *idx_root = ibt.root;
                }
            }
        }
    }
    // Maintain indexes: insert an entry for each index on this table.
    for (idx, idx_root) in index_roots.iter_mut() {
        let key_bytes = encode_index_key(idx, table, full_row);
        let mut ibt = Btree::new(ctx.pager, *idx_root, true);
        ibt.insert_index(&key_bytes, rowid)?;
        *idx_root = ibt.root;
    }
    ctx.last_insert_rowid = rowid;
    ctx.changes += 1;
    ctx.set_max_rowid_lc(table_name_lc, rowid);
    Ok(InsertOutcome::Inserted)
}

/// Consume an unused payload (helper to make the control flow above clear).
#[inline]
fn drop_payload(_p: Vec<u8>) {}

/// Execute the UPSERT action (`ON CONFLICT ... DO NOTHING / DO UPDATE SET`).
///
/// Reads the existing row, evaluates the SET assignments with unqualified
/// column refs bound to the EXISTING row and `excluded.<col>` refs bound to
/// the proposed (new) row, applies the optional WHERE guard, rewrites the
/// row in the B+tree, and maintains indexes.
///
/// On success, `full_row` is replaced with the merged row (for RETURNING).
fn exec_upsert_row(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    table_name_lc: &str,
    current_root: &mut u32,
    full_row: &mut Vec<Value>,
    payload_buf: &mut Vec<u8>,
    index_roots: &mut Vec<(Arc<crate::schema::Index>, u32)>,
    existing_rowid: i64,
    upsert: &crate::sql::ast::UpsertClause,
) -> Result<InsertOutcome> {
    match &upsert.action {
        crate::sql::ast::UpsertAction::DoNothing => {
            return Ok(InsertOutcome::Skipped);
        }
        crate::sql::ast::UpsertAction::DoUpdate { set, where_clause } => {
            // Read the existing row.
            let mut bt = Btree::new(ctx.pager, *current_root, false);
            let old_payload = match bt.lookup_table(existing_rowid)? {
                LookupResult::Found(p) => p,
                LookupResult::NotFound => {
                    // Shouldn't happen (index said it exists) — treat as insert.
                    return Ok(InsertOutcome::Skipped);
                }
            };
            let n_cols = table.n_columns();
            let old_row = match decode_row(&old_payload, n_cols) {
                Ok(r) => r,
                Err(_) => return Ok(InsertOutcome::Skipped),
            };

            // Build a combined evaluation context: unqualified refs → the
            // existing row; `excluded.<col>` → the proposed row.
            let mut comb_names: Vec<String> = Vec::with_capacity(n_cols * 2);
            let mut comb_row: Vec<Value> = Vec::with_capacity(n_cols * 2);
            for (i, c) in table.columns.iter().enumerate() {
                comb_names.push(c.name.clone());
                comb_row.push(old_row.get(i).cloned().unwrap_or(Value::Null));
            }
            for (i, c) in table.columns.iter().enumerate() {
                comb_names.push(format!("excluded.{}", c.name));
                comb_row.push(full_row.get(i).cloned().unwrap_or(Value::Null));
            }

            // Apply SET assignments onto the old row.
            let mut new_row = old_row.clone();
            for (col_name, expr) in set {
                let col_idx = table
                    .find_column(col_name)
                    .ok_or_else(|| Error::semantic(format!("no such column: {}", col_name)))?;
                let v = eval_row(expr, &comb_row, &comb_names, &ctx.params, &ctx.named_params)?;
                let aff = table.columns[col_idx].affinity;
                new_row[col_idx] = aff.coerce(v);
            }

            // WHERE guard: if false, the conflict is left in place (no-op).
            // Column refs bind to the PRE-update row (SQLite semantics).
            if let Some(w) = where_clause {
                let v = eval_row(w, &comb_row, &comb_names, &ctx.params, &ctx.named_params)?;
                if !v.is_truthy() {
                    return Ok(InsertOutcome::Skipped);
                }
            }

            // Enforce NOT NULL + CHECK on the merged row.
            let plain_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
            enforce_row_constraints(table, &new_row, &plain_names, &ctx.params, &ctx.named_params)?;

            // Rowid alias must stay consistent (it's the B+tree key).
            if let Some(idx) = table.rowid_alias {
                new_row[idx] = Value::Integer(existing_rowid);
            }

            // Rewrite the row: in-place when the payload size is unchanged,
            // otherwise delete + insert.
            payload_buf.clear();
            encode_row_into(&new_row, payload_buf);
            {
                let mut bt = Btree::new(ctx.pager, *current_root, false);
                let did_in_place = bt.update_table(existing_rowid, payload_buf).unwrap_or(false);
                if !did_in_place {
                    bt.delete_table(existing_rowid)?;
                    bt.insert_table(existing_rowid, payload_buf)?;
                }
                *current_root = bt.root;
                ctx.set_table_root_lc(table_name_lc, *current_root);
            }

            // Index maintenance: update entries whose key changed.
            for (idx, idx_root) in index_roots.iter_mut() {
                let old_key = encode_index_key(idx, table, &old_row);
                let new_key = encode_index_key(idx, table, &new_row);
                if old_key == new_key {
                    continue;
                }
                let mut ibt = Btree::new(ctx.pager, *idx_root, true);
                ibt.delete_index(&old_key, existing_rowid)?;
                *idx_root = ibt.root;
                let mut ibt = Btree::new(ctx.pager, *idx_root, true);
                ibt.insert_index(&new_key, existing_rowid)?;
                *idx_root = ibt.root;
            }

            // Replace full_row with the merged row for RETURNING.
            *full_row = new_row;
            ctx.changes += 1;
            ctx.last_insert_rowid = existing_rowid;
            Ok(InsertOutcome::UpdatedExisting)
        }
    }
}

/// Encode the index key for a row, given the table's column layout.
/// Uses the ORDER-PRESERVING key encoding (see `Value::encode_order_key_into`)
/// so that the index B+tree's byte order matches SQL value ordering —
/// required for range scans and binary-search equality lookups.
fn encode_index_key(index: &crate::schema::Index, table: &Table, row: &[Value]) -> Vec<u8> {
    let mut key_bytes = Vec::new();
    for col in &index.columns {
        if let Some(pos) = table.find_column(&col.name) {
            if let Some(v) = row.get(pos) {
                v.encode_order_key_into(&mut key_bytes);
            }
        }
    }
    key_bytes
}

/// Insert an entry into an index for a given row.
fn insert_index_entry(pager: &Pager, index: &crate::schema::Index, table: &Table, row: &[Value], rowid: i64) -> Result<()> {
    let key_bytes = encode_index_key(index, table, row);
    let mut bt = Btree::new(pager, index.root_page, true);
    bt.insert_index(&key_bytes, rowid)
}

/// Delete an entry from an index for a given row.
fn delete_index_entry(pager: &Pager, index: &crate::schema::Index, table: &Table, row: &[Value], rowid: i64) -> Result<()> {
    let key_bytes = encode_index_key(index, table, row);
    let mut bt = Btree::new(pager, index.root_page, true);
    bt.delete_index(&key_bytes, rowid)?;
    Ok(())
}

// Note: these functions are kept for reference but the INSERT executor now
// tracks root pages inline. UPDATE and DELETE still use these, which is safe
// because they use the catalog's root_page (which may be stale after splits
// within the same statement — a known limitation to fix later).

fn find_max_rowid(pager: &Pager, root: u32) -> Result<i64> {
    let mut bt = Btree::new(pager, root, false);
    let mut max = 0i64;
    bt.scan_table(|rowid, _| {
        if rowid > max {
            max = rowid;
        }
        true
    })?;
    Ok(max)
}

// ============================================================================
// UPDATE
// ============================================================================

fn exec_update(ctx: &mut ExecContext<'_>, table: Arc<Table>, source: &Plan, assignments: &[(usize, Expr)], returning: Option<&[crate::sql::ast::ResultColumn]>) -> Result<ExecResult> {
    // Streaming UPDATE fast path: when the source is a bare `Scan`, a
    // `Filter { Scan, predicate }`, or a `RowidRange`, iterate the B+tree
    // directly with a reusable row buffer. This avoids the materialize-all
    // cost (`Vec<Vec<Value>>` of N rows × N columns of allocations) for
    // the common `UPDATE t SET col = ... WHERE pred` case.
    //
    // The non-streaming path materializes all matching rows into a Vec<Row>
    // and then iterates them. For a 10k-row UPDATE, that's 10k Vec<Value>
    // allocations + 10k+ Value allocations (one per Text/Blob column per
    // row). The streaming path does ONE Vec<Value> allocation (reused
    // across rows) and ONE Vec<u8> allocation for the encoded payload
    // (also reused). Cuts ~80% of the allocations on a 10k-row UPDATE,
    // closing the 23× UPDATE-range gap vs SQLite.
    if let Some(result) = try_streaming_update(ctx, &table, source, assignments, returning)? {
        return Ok(result);
    }

    let source_res = execute(source, ctx)?;
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let mut updated = 0i64;

    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();

    for row in &source_res.rows {
        let rowid = if let Some(idx) = table.rowid_alias {
            row[idx].as_integer()
        } else {
            return Err(Error::Unsupported("UPDATE on a table without INTEGER PRIMARY KEY"));
        };

        let mut new_row = row.clone();
        for (col_idx, expr) in assignments {
            new_row[*col_idx] = eval_row(expr, row, &col_names, &ctx.params, &ctx.named_params)?;
            let aff = table.columns[*col_idx].affinity;
            new_row[*col_idx] = aff.coerce(new_row[*col_idx].clone());
        }
        // NOT NULL + CHECK constraints on the updated row.
        enforce_row_constraints(&table, &new_row, &col_names, &ctx.params, &ctx.named_params)?;
        let payload = encode_row(&new_row);
        let root = ctx.table_root(&table);
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            // Fast path: if the new payload is the same length as the
            // existing cell payload (the common case for UPDATEs that don't
            // change column types — e.g. `score = score + 1.0` on a REAL
            // column), overwrite the payload bytes in place. This avoids
            // the delete+insert dance (two B+tree traversals + two leaf
            // modifications per row) and is the single biggest UPDATE-perf
            // win.
            //
            // When the payload size changes (e.g. TEXT column gets longer),
            // we fall back to delete + insert.
            let did_in_place = bt.update_table(rowid, &payload).unwrap_or(false);
            if !did_in_place {
                bt.delete_table(rowid)?;
                bt.insert_table(rowid, &payload)?;
            }
            new_root = bt.root;
        }
        ctx.set_table_root(&table.name, new_root);
        // Maintain indexes: for each index on this table, compare the old
        // and new indexed column values. If they're unchanged, skip the
        // delete+insert on that index entirely (the index entry is still
        // valid). This is the common case for `UPDATE t SET score = ...`
        // when the index is on a different column (e.g. `idx_val` on `val`,
        // and the UPDATE doesn't touch `val`).
        for idx in &indexes {
            // Compute the old and new index keys.
            let old_key = encode_index_key(idx, &table, row);
            let new_key = encode_index_key(idx, &table, &new_row);
            if old_key == new_key {
                // No change to this index's key — skip maintenance.
                continue;
            }
            let _ = delete_index_entry(ctx.pager, idx, &table, row, rowid);
            let _ = insert_index_entry(ctx.pager, idx, &table, &new_row, rowid);
        }
        ctx.changes += 1;
        updated += 1;
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(ret, &new_row, &col_names, &ctx.params, &ctx.named_params)?);
        }
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        });
    }
    Ok(ExecResult {
        columns: Arc::from(vec!["updated".to_string()]),
        rows: vec![vec![Value::Integer(updated)]],
    })
}

/// Streaming UPDATE fast path: scan the B+tree directly and update each
/// matching row in place, avoiding the `Vec<Vec<Value>>` materialization
/// cost of the non-streaming path.
///
/// Recognized source shapes (mirrors the streaming-aggregate fast path):
///  - `Plan::Scan`                              — `UPDATE t SET ...`
///  - `Plan::Filter { Scan, predicate }`       — `UPDATE t SET ... WHERE pred`
///  - `Plan::RowidRange`                        — `UPDATE t SET ... WHERE id BETWEEN ? AND ?`
///
/// For each leaf cell visited:
///  1. Decode the row into a reusable `Vec<Value>` buffer (no per-row alloc).
///  2. Apply the filter predicate (if any). Skip if false.
///  3. Apply the SET assignments to produce a `new_row`.
///  4. Encode the new row into a reusable `Vec<u8>` buffer.
///  5. Update in place via `Btree::update_table(rowid, &payload)` if the
///     payload size is unchanged; fall back to delete+insert if it changed.
///  6. Maintain indexes: compare old vs new index keys; only update the
///     index if the key changed.
///
/// Returns `Ok(Some(result))` if the streaming path handled the UPDATE;
/// `Ok(None)` if the source shape wasn't recognized and the caller should
/// fall back to the non-streaming path.
fn try_streaming_update(
    ctx: &mut ExecContext<'_>,
    table: &Arc<Table>,
    source: &Plan,
    assignments: &[(usize, Expr)],
    returning: Option<&[crate::sql::ast::ResultColumn]>,
) -> Result<Option<ExecResult>> {
    // Detect the source shape and extract (table, filter_predicate, range, rowid).
    enum StreamingSource<'a> {
        Scan { table: &'a Arc<Table>, filter: Option<&'a Expr> },
        RowidRange { table: &'a Arc<Table>, start: Option<&'a Expr>, end: Option<&'a Expr>, residual: Option<&'a Expr> },
        RowidLookup { table: &'a Arc<Table>, rowid: &'a Expr },
    }
    let src = match source {
        Plan::Scan { table: t, .. } => StreamingSource::Scan { table: t, filter: None },
        Plan::Filter { input, predicate } => {
            if let Plan::Scan { table: t, .. } = input.as_ref() {
                StreamingSource::Scan { table: t, filter: Some(predicate) }
            } else {
                return Ok(None);
            }
        }
        Plan::RowidRange { table: t, start, end, residual, .. } => {
            StreamingSource::RowidRange { table: t, start: start.as_ref(), end: end.as_ref(), residual: residual.as_ref() }
        }
        Plan::RowidLookup { table: t, rowid, .. } => {
            StreamingSource::RowidLookup { table: t, rowid }
        }
        _ => return Ok(None),
    };

    // The source table must match the UPDATE's target table (otherwise
    // we'd be updating rows from a different table, which isn't what
    // this fast path is for).
    let src_table = match &src {
        StreamingSource::Scan { table: t, .. } => t.clone(),
        StreamingSource::RowidRange { table: t, .. } => t.clone(),
        StreamingSource::RowidLookup { table: t, .. } => t.clone(),
    };
    if src_table.name.to_ascii_lowercase() != table.name.to_ascii_lowercase() {
        return Ok(None);
    }

    let params = ctx.params.clone();
    let named_params = ctx.named_params.clone();
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let n_cols = table.n_columns();
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let root = ctx.table_root(table);

    // Evaluate rowid-range bounds (if RowidRange source).
    let (range_start, range_end) = match &src {
        StreamingSource::RowidRange { start, end, .. } => {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
            let s = match start { Some(e) => evaluate(e, &eval_ctx)?.as_integer(), None => i64::MIN };
            let e = match end { Some(e) => evaluate(e, &eval_ctx)?.as_integer(), None => i64::MAX };
            (s, e)
        }
        _ => (i64::MIN, i64::MAX),
    };
    let residual_pred = match &src {
        StreamingSource::Scan { filter, .. } => *filter,
        StreamingSource::RowidRange { residual, .. } => *residual,
        StreamingSource::RowidLookup { .. } => None,
    };

    // For RowidLookup, evaluate the rowid expression now.
    let lookup_rowid: Option<i64> = match &src {
        StreamingSource::RowidLookup { rowid, .. } => {
            let empty_row: Vec<Value> = Vec::new();
            let empty_cols: Vec<String> = Vec::new();
            let eval_ctx = EvalContext::new(&empty_row, &empty_cols, &params, &named_params);
            Some(evaluate(rowid, &eval_ctx)?.as_integer())
        }
        _ => None,
    };

    // Reusable buffers — allocated once, reused per row.
    let mut row_buf: Vec<Value> = Vec::with_capacity(n_cols);
    let mut new_row: Vec<Value> = Vec::with_capacity(n_cols);
    let mut payload_buf: Vec<u8> = Vec::with_capacity(256);

    // Collect (rowid, new_payload_bytes) tuples for the update phase.
    // We can't update inside the scan callback because the scan holds
    // `&mut self.pager` via the Btree. So we collect first, then update
    // after the scan completes.
    //
    // We only stash the byte payload — NOT the decoded Vec<Value>. The
    // old row is re-decoded from the B+tree during the update phase
    // (one cheap `lookup_table` per row, which is ~5µs vs ~3µs to clone
    // a Vec<Value>). This trades a small extra B+tree seek for ~3 fewer
    // heap allocations per row, which is the dominant cost on a 10k-row
    // UPDATE.
    let mut updates: Vec<(i64, Vec<u8>)> = Vec::new();
    // RETURNING: stash decoded new rows too (only when needed).
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    // First constraint error encountered during the scan (if any).
    let mut first_error: Option<crate::error::Error> = None;

    let mut bt = Btree::new(ctx.pager, root, false);
    if let Some(rowid) = lookup_rowid {
        // RowidLookup source — fetch exactly one row by rowid.
        match bt.lookup_table(rowid)? {
            LookupResult::Found(payload) => {
                if let Err(e) = process_update_row(
                    &payload, n_cols, &mut row_buf, &mut new_row, &mut payload_buf,
                    assignments, &col_names, &params, &named_params, table,
                    residual_pred, &mut updates, &mut returning_rows, returning,
                ) {
                    first_error = Some(e);
                }
            }
            LookupResult::NotFound => {}
        }
        Ok::<(), crate::error::Error>(())
    } else if matches!(src, StreamingSource::RowidRange { .. }) {
        bt.scan_table_range_borrowed(range_start, range_end, |_rowid, payload| {
            if let Err(e) = process_update_row(
                payload, n_cols, &mut row_buf, &mut new_row, &mut payload_buf,
                assignments, &col_names, &params, &named_params, table,
                residual_pred, &mut updates, &mut returning_rows, returning,
            ) {
                first_error = Some(e);
                return false; // stop the scan
            }
            true
        })
    } else {
        bt.scan_table_borrowed(|_rowid, payload| {
            if let Err(e) = process_update_row(
                payload, n_cols, &mut row_buf, &mut new_row, &mut payload_buf,
                assignments, &col_names, &params, &named_params, table,
                residual_pred, &mut updates, &mut returning_rows, returning,
            ) {
                first_error = Some(e);
                return false; // stop the scan
            }
            true
        })
    }?;
    let new_root = bt.root;
    ctx.set_table_root(&table.name, new_root);

    // Surface any constraint error BEFORE applying updates (statement
    // aborts atomically — the pager snapshot/rollback handles the rest).
    if let Some(e) = first_error {
        return Err(e);
    }

    // Phase 2: apply the updates. For each (rowid, new_payload):
    //   1. If any index exists AND the SET assignments touch an indexed
    //      column, look up the existing payload (for index maintenance
    //      old-key comparison). Skip the lookup when no index needs
    //      updating — `update_table` will do its own internal lookup.
    //      This saves a B+tree seek per row on the common
    //      `UPDATE t SET score = ... WHERE val > 5000` case where `score`
    //      isn't indexed but `val` is — ~5µs/row × 5000 rows = 25ms saved.
    //   2. If new payload size matches the existing size, overwrite in
    //      place via `update_table`. Otherwise, delete + insert.
    //   3. Maintain indexes: only update the index if the key changed.
    //
    // Pre-compute which indexes might be touched by the SET assignments.
    // An index is "touched" if any of its columns appears in the SET list.
    let touched_indexes: Vec<&Arc<crate::schema::Index>> = if indexes.is_empty() {
        Vec::new()
    } else {
        let assignment_cols: std::collections::HashSet<usize> = assignments.iter().map(|(idx, _)| *idx).collect();
        indexes.iter().filter(|idx| {
            // An index is touched if any of its columns is in the SET list.
            idx.columns.iter().any(|c| {
                if let Some(col_idx) = table.find_column(&c.name) {
                    assignment_cols.contains(&col_idx)
                } else {
                    false
                }
            })
        }).collect()
    };
    let needs_old_payload = !touched_indexes.is_empty();
    let mut updated = 0i64;
    let mut old_row_buf: Vec<Value> = Vec::with_capacity(n_cols);
    for (rowid, new_payload) in &updates {
        // Look up the existing row payload ONLY if we need it for index
        // maintenance. This saves a B+tree seek per row on the common
        // no-index UPDATE range case (~5µs/row × 5000 rows = 25ms saved).
        let old_payload_opt: Option<Vec<u8>> = if needs_old_payload {
            let mut bt = Btree::new(ctx.pager, root, false);
            match bt.lookup_table(*rowid)? {
                LookupResult::Found(p) => Some(p),
                LookupResult::NotFound => None,
            }
        } else {
            None
        };
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            let did_in_place = bt.update_table(*rowid, new_payload).unwrap_or(false);
            if !did_in_place {
                bt.delete_table(*rowid)?;
                bt.insert_table(*rowid, new_payload)?;
            }
            new_root = bt.root;
        }
        ctx.set_table_root(&table.name, new_root);
        // Index maintenance — only on indexes whose key actually changed.
        if needs_old_payload {
            if let Some(old_payload) = old_payload_opt {
                old_row_buf.clear();
                if decode_row_into(&old_payload, n_cols, &mut old_row_buf).is_err() {
                    continue;
                }
                new_row.clear();
                if decode_row_into(new_payload, n_cols, &mut new_row).is_err() {
                    continue;
                }
                for idx in &touched_indexes {
                    let old_key = encode_index_key(idx, table, &old_row_buf);
                    let new_key = encode_index_key(idx, table, &new_row);
                    if old_key == new_key {
                        continue;
                    }
                    let _ = delete_index_entry(ctx.pager, idx, table, &old_row_buf, *rowid);
                    let _ = insert_index_entry(ctx.pager, idx, table, &new_row, *rowid);
                }
            }
        }
        ctx.changes += 1;
        updated += 1;
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(Some(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        }));
    }
    Ok(Some(ExecResult {
        columns: Arc::from(vec!["updated".to_string()]),
        rows: vec![vec![Value::Integer(updated)]],
    }))
}

/// Per-row processing for the streaming UPDATE: decode the payload into
/// `row_buf`, apply the filter predicate, build the new row in `new_row`,
/// enforce constraints, encode it into `payload_buf`, and push the
/// (rowid, payload) tuple into `updates`. Buffers are reused across rows.
#[allow(clippy::too_many_arguments)]
fn process_update_row(
    payload: &[u8],
    n_cols: usize,
    row_buf: &mut Vec<Value>,
    new_row: &mut Vec<Value>,
    payload_buf: &mut Vec<u8>,
    assignments: &[(usize, Expr)],
    col_names: &[String],
    params: &[Value],
    named_params: &HashMap<String, Value>,
    table: &Arc<Table>,
    residual_pred: Option<&Expr>,
    updates: &mut Vec<(i64, Vec<u8>)>,
    returning_rows: &mut Vec<Vec<Value>>,
    returning: Option<&[crate::sql::ast::ResultColumn]>,
) -> Result<()> {
    row_buf.clear();
    if decode_row_into(payload, n_cols, row_buf).is_err() {
        return Ok(());
    }
    // Extract rowid from the rowid-alias column (or _rowid_).
    let rowid = if let Some(idx) = table.rowid_alias {
        row_buf.get(idx).map(|v| v.as_integer()).unwrap_or(0)
    } else {
        0
    };
    // Apply filter predicate (if any).
    if let Some(pred) = residual_pred {
        match eval_row(pred, row_buf, col_names, params, named_params) {
            Ok(v) => {
                if !v.is_truthy() {
                    return Ok(());
                }
            }
            Err(e) => return Err(e),
        }
    }
    // Build the new row: copy old values, apply SET assignments.
    new_row.clear();
    new_row.extend_from_slice(row_buf);
    for (col_idx, expr) in assignments {
        let v = eval_row(expr, row_buf, col_names, params, named_params)?;
        let aff = table.columns[*col_idx].affinity;
        new_row[*col_idx] = aff.coerce(v);
    }
    // NOT NULL + CHECK constraints on the updated row.
    enforce_row_constraints(table, new_row, col_names, params, named_params)?;
    // Encode the new row.
    payload_buf.clear();
    encode_row_into(new_row, payload_buf);
    // RETURNING: project now (the row is final).
    if let Some(ret) = returning {
        returning_rows.push(project_returning_row(ret, new_row, col_names, params, named_params)?);
    }
    // Stash the (rowid, payload) for phase 2.
    updates.push((rowid, payload_buf.clone()));
    Ok(())
}

// ============================================================================
// DELETE
// ============================================================================

fn exec_delete(ctx: &mut ExecContext<'_>, table: Arc<Table>, source: &Plan, returning: Option<&[crate::sql::ast::ResultColumn]>) -> Result<ExecResult> {
    let source_res = execute(source, ctx)?;
    let indexes = ctx.catalog().indexes_on_table(&table.name);
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let mut deleted = 0i64;
    let mut returning_rows: Vec<Vec<Value>> = Vec::new();
    for row in &source_res.rows {
        let rowid = if let Some(idx) = table.rowid_alias {
            row[idx].as_integer()
        } else {
            return Err(Error::Unsupported("DELETE on a table without INTEGER PRIMARY KEY"));
        };
        // RETURNING: project the pre-delete row.
        if let Some(ret) = returning {
            returning_rows.push(project_returning_row(ret, row, &col_names, &ctx.params, &ctx.named_params)?);
        }
        let root = ctx.table_root(&table);
        let new_root;
        {
            let mut bt = Btree::new(ctx.pager, root, false);
            bt.delete_table(rowid)?;
            new_root = bt.root;
        }
        ctx.set_table_root(&table.name, new_root);
        // Maintain indexes: delete the entry for this row.
        for idx in &indexes {
            delete_index_entry(ctx.pager, idx, &table, row, rowid)?;
        }
        ctx.changes += 1;
        deleted += 1;
    }
    if !ctx.in_transaction && !ctx.deferred_flush {
        ctx.pager.flush()?;
    }
    if let Some(ret) = returning {
        return Ok(ExecResult {
            columns: returning_column_names(ret, &col_names).into(),
            rows: returning_rows,
        });
    }
    Ok(ExecResult {
        columns: Arc::from(vec!["deleted".to_string()]),
        rows: vec![vec![Value::Integer(deleted)]],
    })
}
