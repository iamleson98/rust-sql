//! Query planner: converts an AST into a logical plan.
//!
//! The planner does three things:
//! 1. **Name resolution**: every `Expr::Column` is bound to a specific table/column.
//! 2. **Plan shape**: SELECT → Project → (Filter → (Aggregate → (Sort → (Limit → Source))))
//! 3. **Optimization**: index selection, predicate pushdown, join reordering.

pub mod plan;

pub use plan::*;

use crate::error::{Error, Result};
use crate::schema::{Catalog, Index, Table};
use crate::sql::ast::*;
use crate::types::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// The planner.
pub struct Planner<'a> {
    catalog: &'a Catalog,
    /// The current scope of table aliases → tables.
    /// Used for name resolution.
    scopes: Vec<HashMap<String, Arc<Table>>>,
}

impl<'a> Planner<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Self { catalog, scopes: vec![HashMap::new()] }
    }

    /// Plan a SELECT statement.
    pub fn plan_select(&mut self, stmt: &SelectStatement) -> Result<Plan> {
        if let Some(with) = &stmt.with {
            self.scopes.push(HashMap::new());
            for cte in &with.ctes {
                let cte_plan = self.plan_select(&cte.select)?;
                let _ = cte_plan;
            }
        }

        let plan = self.plan_select_body(&stmt.body)?;

        // Insert Sort BELOW Project / Distinct so it can see all input
        // columns (not just the projected ones). This is required for
        // `SELECT a FROM t ORDER BY b` where `b` is in the table but not
        // in the projection. SQLite semantics: ORDER BY may reference any
        // column in the FROM clause, or any projection alias.
        let plan = if !stmt.order_by.is_empty() {
            insert_sort_below_top(plan, stmt.order_by.clone())
        } else {
            plan
        };

        let plan = if stmt.limit.is_some() || stmt.offset.is_some() {
            Plan::Limit {
                input: Box::new(plan),
                count: stmt.limit.clone().unwrap_or(Expr::Literal(Value::Integer(-1))),
                offset: stmt.offset.clone().unwrap_or(Expr::Literal(Value::Integer(0))),
            }
        } else {
            plan
        };

        if stmt.with.is_some() {
            self.scopes.pop();
        }
        Ok(plan)
    }

    fn plan_select_body(&mut self, body: &SelectBody) -> Result<Plan> {
        match body {
            SelectBody::Simple(s) => self.plan_simple_select(s),
            SelectBody::Binary { op, left, right } => {
                let l = self.plan_select_body(left)?;
                let r = self.plan_select_body(right)?;
                match op {
                    SetOp::Union => Ok(Plan::Union { left: Box::new(l), right: Box::new(r), all: false }),
                    SetOp::UnionAll => Ok(Plan::Union { left: Box::new(l), right: Box::new(r), all: true }),
                    SetOp::Intersect => Ok(Plan::Intersect { left: Box::new(l), right: Box::new(r) }),
                    SetOp::Except => Ok(Plan::Except { left: Box::new(l), right: Box::new(r) }),
                }
            }
        }
    }

    fn plan_simple_select(&mut self, s: &SimpleSelect) -> Result<Plan> {
        let mut plan = if let Some(from) = &s.from {
            self.plan_table_expression(from)?
        } else {
            Plan::Values { rows: vec![vec![]] }
        };

        // Apply WHERE — with predicate pushdown and index/rowid lookup optimization.
        if let Some(pred) = &s.where_clause {
            plan = self.apply_where(plan, pred);
        }

        let has_aggregates = self.expr_list_has_aggregates(&s.columns)
            || s.having.is_some()
            || !s.group_by.is_empty();
        let aggregates = if has_aggregates {
            self.collect_aggregates(&s.columns, s.having.as_ref())?
        } else {
            Vec::new()
        };
        // Resolve GROUP BY aliases: if a GROUP BY expression is a simple column
        // reference that matches a projection alias, replace it with the
        // projection's expression. This lets `GROUP BY bucket` work where
        // `bucket` is an alias for a CASE expression in the SELECT list.
        let resolved_group_by: Vec<Expr> = s.group_by.iter().map(|g| {
            if let Expr::Column { table: None, name } = g {
                for c in &s.columns {
                    if let ResultColumn::Expr { expr, alias: Some(a) } = c {
                        if a.eq_ignore_ascii_case(name) {
                            return expr.clone();
                        }
                    }
                }
            }
            g.clone()
        }).collect();
        if has_aggregates {
            // The Aggregate operator outputs: [group_by_cols..., aggregate_results...].
            // We need to rewrite the Project's columns so that aggregate expressions
            // reference these output columns by index, and group-by expressions
            // reference the group key columns (col1, col2, ...).
            let n_group = resolved_group_by.len();
            plan = Plan::Aggregate {
                input: Box::new(plan),
                group_by: resolved_group_by.clone(),
                aggregates: aggregates.clone(),
            };
            if let Some(having) = &s.having {
                let rewritten_having = rewrite_aggregates_and_groups(having, &aggregates, &resolved_group_by, n_group);
                plan = Plan::Filter { input: Box::new(plan), predicate: rewritten_having };
            }

            let rewritten_columns: Vec<ProjectExpr> = s.columns.iter().map(|c| {
                match c {
                    ResultColumn::Star => ProjectExpr {
                        expr: Expr::Column { table: None, name: "*".into() },
                        alias: None,
                    },
                    ResultColumn::TableStar(t) => ProjectExpr {
                        expr: Expr::Column { table: Some(t.clone()), name: "*".into() },
                        alias: None,
                    },
                    ResultColumn::Expr { expr, alias } => {
                        let rewritten = rewrite_aggregates_and_groups(expr, &aggregates, &resolved_group_by, n_group);
                        let alias = alias.clone().or_else(|| {
                            if let Expr::Column { name, .. } = &rewritten {
                                if name.starts_with("__agg_") {
                                    let idx: usize = name.trim_start_matches("__agg_").parse().ok()?;
                                    Some(aggregates[idx].display_name.clone())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        });
                        ProjectExpr { expr: rewritten, alias }
                    }
                }
            }).collect();
            plan = Plan::Project {
                input: Box::new(plan),
                columns: rewritten_columns,
            };
            return Ok(plan);
        }

        let has_windows = self.expr_list_has_windows(&s.columns);
        if has_windows {
            let windows = self.collect_windows(&s.columns, &s.window)?;
            plan = Plan::Window { input: Box::new(plan), windows };
        }

        // Project FIRST, then DISTINCT. SQLite semantics: DISTINCT applies
        // to the projected columns, not the underlying row. If we put
        // Distinct before Project, the full row (including the rowid alias)
        // would be the dedup key, and every row would be unique.
        plan = Plan::Project {
            input: Box::new(plan),
            columns: s.columns.iter().map(|c| self.result_column_to_project(c)).collect::<Result<_>>()?,
        };

        if s.distinct {
            plan = Plan::Distinct { input: Box::new(plan) };
        }

        Ok(plan)
    }

    /// Apply a WHERE predicate to a plan, with optimizations:
    /// - If the plan is a `Scan` and the predicate contains a top-level
    ///   `col = literal` (or `col = ?`) where `col` is the rowid alias,
    ///   replace the scan with `RowidLookup`.
    /// - If the plan is a `Scan` and the predicate contains a top-level
    ///   `col = literal` where `col` has an index, replace the scan with
    ///   `IndexLookup`.
    /// - Otherwise, push the predicate into the scan as a `Filter` (which
    ///   the executor evaluates per row).
    fn apply_where(&self, plan: Plan, predicate: &Expr) -> Plan {
        // Delegate to the free function so that plan_update / plan_delete in
        // api.rs can share the exact same predicate-matching logic without
        // needing a `Planner` instance. This fixes a critical perf bug where
        // `UPDATE t SET ... WHERE id = ?` was falling through to a full
        // table scan instead of a RowidLookup.
        apply_where_for_scan(self.catalog, plan, predicate)
    }

    fn plan_table_expression(&mut self, te: &TableExpression) -> Result<Plan> {
        match te {
            TableExpression::Table { name, alias, indexed, .. } => {
                let table = self.catalog.get_table(name).ok_or_else(|| {
                    Error::NotFound(format!("table: {}", name))
                })?;
                let index = if let Some(IndexedHint::Indexed(idx_name)) = indexed {
                    self.catalog.get_index(idx_name)
                } else {
                    None
                };
                let alias_key = alias.clone().unwrap_or_else(|| name.clone());
                self.scopes
                    .last_mut()
                    .unwrap()
                    .insert(alias_key.to_ascii_lowercase(), table.clone());
                Ok(Plan::Scan {
                    table,
                    alias: alias.clone(),
                    index,
                    predicate: None,
                })
            }
            TableExpression::Subquery { select, alias, .. } => {
                let inner = self.plan_select(select)?;
                if let Some(a) = alias {
                    let _ = a;
                }
                Ok(Plan::Subquery { plan: Box::new(inner) })
            }
            TableExpression::Join { left, right, join_type, constraint } => {
                let l = self.plan_table_expression(left)?;
                let r = self.plan_table_expression(right)?;
                let condition = match constraint {
                    JoinConstraint::On(e) => Some(e.clone()),
                    JoinConstraint::Using(cols) => {
                        let mut combined = None;
                        for c in cols {
                            let e = Expr::Binary {
                                op: BinaryOp::Eq,
                                left: Box::new(Expr::Column { table: None, name: c.clone() }),
                                right: Box::new(Expr::Column { table: None, name: c.clone() }),
                            };
                            combined = Some(match combined {
                                Some(prev) => Expr::Binary {
                                    op: BinaryOp::And,
                                    left: Box::new(prev),
                                    right: Box::new(e),
                                },
                                None => e,
                            });
                        }
                        combined
                    }
                    JoinConstraint::Natural => None,
                    JoinConstraint::None => None,
                };
                let jt = match join_type {
                    crate::sql::ast::JoinType::Inner => plan::JoinType::Inner,
                    crate::sql::ast::JoinType::Left => plan::JoinType::Left,
                    crate::sql::ast::JoinType::Right => plan::JoinType::Right,
                    crate::sql::ast::JoinType::Full => plan::JoinType::Full,
                    crate::sql::ast::JoinType::Cross => plan::JoinType::Cross,
                };
                let algo = if matches!(constraint, JoinConstraint::Natural | JoinConstraint::Using(_)) {
                    JoinAlgorithm::Hash
                } else if let Some(Expr::Binary { op: BinaryOp::Eq, .. }) = &condition {
                    JoinAlgorithm::Hash
                } else {
                    JoinAlgorithm::NestedLoop
                };
                Ok(Plan::Join {
                    left: Box::new(l),
                    right: Box::new(r),
                    join_type: jt,
                    condition,
                    algorithm: algo,
                })
            }
        }
    }

    fn result_column_to_project(&self, c: &ResultColumn) -> Result<ProjectExpr> {
        match c {
            ResultColumn::Star => Ok(ProjectExpr { expr: Expr::Column { table: None, name: "*".into() }, alias: None }),
            ResultColumn::TableStar(t) => Ok(ProjectExpr { expr: Expr::Column { table: Some(t.clone()), name: "*".into() }, alias: None }),
            ResultColumn::Expr { expr, alias } => Ok(ProjectExpr { expr: expr.clone(), alias: alias.clone() }),
        }
    }

    fn expr_list_has_aggregates(&self, cols: &[ResultColumn]) -> bool {
        cols.iter().any(|c| matches!(c, ResultColumn::Expr { expr, .. } if expr_has_aggregate(expr)))
    }

    fn expr_list_has_windows(&self, cols: &[ResultColumn]) -> bool {
        cols.iter().any(|c| matches!(c, ResultColumn::Expr { expr, .. } if expr_has_window(expr)))
    }

    fn collect_aggregates(&self, cols: &[ResultColumn], having: Option<&Expr>) -> Result<Vec<AggExpr>> {
        let mut out = Vec::new();
        for c in cols {
            if let ResultColumn::Expr { expr, alias } = c {
                collect_aggregates_rec(expr, alias, &mut out);
            }
        }
        if let Some(h) = having {
            collect_aggregates_rec(h, &None, &mut out);
        }
        Ok(out)
    }

    fn collect_windows(&self, cols: &[ResultColumn], defs: &[WindowDef]) -> Result<Vec<WindowExpr>> {
        let _ = defs;
        let mut out = Vec::new();
        for c in cols {
            if let ResultColumn::Expr { expr, alias } = c {
                collect_windows_rec(expr, alias, &mut out);
            }
        }
        Ok(out)
    }
}

/// Rewrite an expression, replacing:
/// - aggregate function calls with column references `__agg_N`
/// - sub-expressions matching a GROUP BY expression with column references `col{N+1}`
///
/// The Aggregate operator outputs `[group_key_1, group_key_2, ..., group_key_N, agg_1, ..., agg_M]`.
/// Group keys are named `col1`, `col2`, ..., `colN` (1-indexed).
pub fn rewrite_aggregates_and_groups(
    e: &Expr,
    aggregates: &[AggExpr],
    group_by: &[Expr],
    n_group: usize,
) -> Expr {
    let _ = n_group;
    // First, check if this expression matches a GROUP BY expression.
    // We use a structural-equality heuristic (Display-based) since Expr doesn't impl PartialEq.
    let e_display = format!("{:?}", e);
    for (i, g) in group_by.iter().enumerate() {
        let g_display = format!("{:?}", g);
        if g_display == e_display {
            return Expr::Column { table: None, name: format!("col{}", i + 1) };
        }
    }
    // Otherwise, rewrite aggregates and recurse.
    match e {
        Expr::Function { name, distinct, args, over, filter } => {
            if over.is_none() && is_aggregate_fn(&name.to_ascii_lowercase()) {
                let first_arg_is_star = args.first().map(|a| matches!(a, Expr::Column { name, .. } if name == "*")).unwrap_or(false);
                for (i, agg) in aggregates.iter().enumerate() {
                    let agg_arg_is_star = agg.arg.is_none();
                    if agg.func == name.to_ascii_lowercase()
                        && agg.distinct == *distinct
                        && (agg_arg_is_star == first_arg_is_star || first_arg_is_star)
                    {
                        let col_name = format!("__agg_{}", i);
                        return Expr::Column { table: None, name: col_name };
                    }
                }
                e.clone()
            } else {
                let new_args: Vec<Expr> = args.iter().map(|a| rewrite_aggregates_and_groups(a, aggregates, group_by, n_group)).collect();
                Expr::Function {
                    name: name.clone(),
                    distinct: *distinct,
                    args: new_args,
                    filter: filter.clone(),
                    over: over.clone(),
                }
            }
        }
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_aggregates_and_groups(left, aggregates, group_by, n_group)),
            right: Box::new(rewrite_aggregates_and_groups(right, aggregates, group_by, n_group)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_aggregates_and_groups(expr, aggregates, group_by, n_group)),
        },
        Expr::Between { expr, low, high, negated } => Expr::Between {
            expr: Box::new(rewrite_aggregates_and_groups(expr, aggregates, group_by, n_group)),
            low: Box::new(rewrite_aggregates_and_groups(low, aggregates, group_by, n_group)),
            high: Box::new(rewrite_aggregates_and_groups(high, aggregates, group_by, n_group)),
            negated: *negated,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_aggregates_and_groups(expr, aggregates, group_by, n_group)),
            negated: *negated,
        },
        Expr::Is { left, right, negated } => Expr::Is {
            left: Box::new(rewrite_aggregates_and_groups(left, aggregates, group_by, n_group)),
            right: Box::new(rewrite_aggregates_and_groups(right, aggregates, group_by, n_group)),
            negated: *negated,
        },
        Expr::Case { operand, whens, else_ } => {
            let new_whens: Vec<(Expr, Expr)> = whens.iter().map(|(c, v)| {
                (rewrite_aggregates_and_groups(c, aggregates, group_by, n_group),
                 rewrite_aggregates_and_groups(v, aggregates, group_by, n_group))
            }).collect();
            Expr::Case {
                operand: operand.as_ref().map(|o| Box::new(rewrite_aggregates_and_groups(o, aggregates, group_by, n_group))),
                whens: new_whens,
                else_: else_.as_ref().map(|e| Box::new(rewrite_aggregates_and_groups(e, aggregates, group_by, n_group))),
            }
        }
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_aggregates_and_groups(expr, aggregates, group_by, n_group)),
            type_name: type_name.clone(),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_aggregates_and_groups(expr, aggregates, group_by, n_group)),
            collation: collation.clone(),
        },
        _ => e.clone(),
    }
}

/// Backwards-compat wrapper.
pub fn rewrite_aggregates(e: &Expr, aggregates: &[AggExpr], n_group: usize) -> Expr {
    rewrite_aggregates_and_groups(e, aggregates, &[], n_group)
}

/// Check if an expression contains an aggregate function call.
pub fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Function { name, over, .. } => over.is_none() && is_aggregate_fn(&name.to_ascii_lowercase()),
        Expr::Binary { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::Unary { expr, .. } => expr_has_aggregate(expr),
        Expr::Between { expr, low, high, .. } => {
            expr_has_aggregate(expr) || expr_has_aggregate(low) || expr_has_aggregate(high)
        }
        Expr::In { expr, source, .. } => {
            expr_has_aggregate(expr)
                || matches!(source, InSource::List(l) if l.iter().any(expr_has_aggregate))
        }
        Expr::Like { expr, pattern, escape, .. } => {
            expr_has_aggregate(expr)
                || expr_has_aggregate(pattern)
                || escape.as_ref().map(|e| expr_has_aggregate(e)).unwrap_or(false)
        }
        Expr::IsNull { expr, .. } => expr_has_aggregate(expr),
        Expr::Is { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::Case { operand, whens, else_ } => {
            operand.as_ref().map(|e| expr_has_aggregate(e)).unwrap_or(false)
                || whens.iter().any(|(c, v)| expr_has_aggregate(c) || expr_has_aggregate(v))
                || else_.as_ref().map(|e| expr_has_aggregate(e)).unwrap_or(false)
        }
        Expr::Row(es) => es.iter().any(expr_has_aggregate),
        Expr::Cast { expr, .. } => expr_has_aggregate(expr),
        Expr::Collate { expr, .. } => expr_has_aggregate(expr),
        _ => false,
    }
}

/// Check if an expression contains a window function call.
pub fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::Function { over: Some(_), .. } => true,
        Expr::Binary { left, right, .. } => expr_has_window(left) || expr_has_window(right),
        Expr::Unary { expr, .. } => expr_has_window(expr),
        Expr::Between { expr, low, high, .. } => {
            expr_has_window(expr) || expr_has_window(low) || expr_has_window(high)
        }
        Expr::In { expr, source, .. } => {
            expr_has_window(expr) || matches!(source, InSource::List(l) if l.iter().any(expr_has_window))
        }
        Expr::IsNull { expr, .. } => expr_has_window(expr),
        Expr::Is { left, right, .. } => expr_has_window(left) || expr_has_window(right),
        Expr::Case { operand, whens, else_ } => {
            operand.as_ref().map(|e| expr_has_window(e)).unwrap_or(false)
                || whens.iter().any(|(c, v)| expr_has_window(c) || expr_has_window(v))
                || else_.as_ref().map(|e| expr_has_window(e)).unwrap_or(false)
        }
        Expr::Cast { expr, .. } => expr_has_window(expr),
        Expr::Collate { expr, .. } => expr_has_window(expr),
        _ => false,
    }
}

/// Returns true if the function name is an aggregate.
pub fn is_aggregate_fn(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "avg" | "min" | "max" | "total" | "group_concat"
    )
}

/// Returns true if the function name is a window-only function.
#[allow(dead_code)]
pub fn is_window_only_fn(name: &str) -> bool {
    matches!(name, "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" | "ntile" | "lag" | "lead" | "first_value" | "last_value" | "nth_value")
}

fn collect_aggregates_rec(e: &Expr, alias: &Option<String>, out: &mut Vec<AggExpr>) {
    match e {
        Expr::Function { name, distinct, args, over, filter } => {
            if over.is_none() && is_aggregate_fn(&name.to_ascii_lowercase()) {
                let arg = if args.is_empty() || (args.len() == 1 && matches!(&args[0], Expr::Column { name, .. } if name == "*")) {
                    None
                } else if args.len() == 1 {
                    Some(args[0].clone())
                } else {
                    Some(args[0].clone())
                };
                out.push(AggExpr {
                    func: name.to_ascii_lowercase(),
                    arg,
                    distinct: *distinct,
                    alias: alias.clone(),
                    display_name: format!("{}", name),
                });
                return;
            }
            for a in args {
                collect_aggregates_rec(a, &None, out);
            }
            if let Some(f) = filter {
                collect_aggregates_rec(f, &None, out);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_aggregates_rec(left, &None, out);
            collect_aggregates_rec(right, &None, out);
        }
        Expr::Unary { expr, .. } => collect_aggregates_rec(expr, &None, out),
        Expr::Between { expr, low, high, .. } => {
            collect_aggregates_rec(expr, &None, out);
            collect_aggregates_rec(low, &None, out);
            collect_aggregates_rec(high, &None, out);
        }
        Expr::In { expr, source, .. } => {
            collect_aggregates_rec(expr, &None, out);
            if let InSource::List(l) = source {
                for e in l {
                    collect_aggregates_rec(e, &None, out);
                }
            }
        }
        Expr::Like { expr, pattern, escape, .. } => {
            collect_aggregates_rec(expr, &None, out);
            collect_aggregates_rec(pattern, &None, out);
            if let Some(e) = escape {
                collect_aggregates_rec(e, &None, out);
            }
        }
        Expr::IsNull { expr, .. } => collect_aggregates_rec(expr, &None, out),
        Expr::Is { left, right, .. } => {
            collect_aggregates_rec(left, &None, out);
            collect_aggregates_rec(right, &None, out);
        }
        Expr::Case { operand, whens, else_ } => {
            if let Some(o) = operand {
                collect_aggregates_rec(o, &None, out);
            }
            for (c, v) in whens {
                collect_aggregates_rec(c, &None, out);
                collect_aggregates_rec(v, &None, out);
            }
            if let Some(e) = else_ {
                collect_aggregates_rec(e, &None, out);
            }
        }
        Expr::Cast { expr, .. } => collect_aggregates_rec(expr, &None, out),
        Expr::Collate { expr, .. } => collect_aggregates_rec(expr, &None, out),
        Expr::Row(es) => {
            for e in es {
                collect_aggregates_rec(e, &None, out);
            }
        }
        _ => {}
    }
}

fn collect_windows_rec(e: &Expr, alias: &Option<String>, out: &mut Vec<WindowExpr>) {
    match e {
        Expr::Function { name, distinct, args, over, .. } => {
            if let Some(spec) = over {
                let (partition_by, order_by, frame) = match spec.as_ref() {
                    WindowSpec::Named(_) => (Vec::new(), Vec::new(), None),
                    WindowSpec::Inline(def) => (def.partition_by.clone(), def.order_by.clone(), def.frame.as_ref().map(|f| (**f).clone())),
                };
                let arg = if args.is_empty() || (args.len() == 1 && matches!(&args[0], Expr::Column { name, .. } if name == "*")) {
                    None
                } else if args.len() == 1 {
                    Some(args[0].clone())
                } else {
                    Some(args[0].clone())
                };
                out.push(WindowExpr {
                    func: name.to_ascii_lowercase(),
                    arg,
                    distinct: *distinct,
                    partition_by,
                    order_by,
                    frame,
                    alias: alias.clone(),
                    display_name: format!("{}", name),
                });
                return;
            }
            for a in args {
                collect_windows_rec(a, &None, out);
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_windows_rec(left, &None, out);
            collect_windows_rec(right, &None, out);
        }
        Expr::Unary { expr, .. } => collect_windows_rec(expr, &None, out),
        Expr::Cast { expr, .. } => collect_windows_rec(expr, &None, out),
        Expr::Collate { expr, .. } => collect_windows_rec(expr, &None, out),
        _ => {}
    }
}

/// Try to find an index that can satisfy a point lookup on the given column.
#[allow(dead_code)]
pub fn find_index_for_column(
    catalog: &Catalog,
    table: &Table,
    col_name: &str,
) -> Option<Arc<Index>> {
    for idx in catalog.indexes_on_table(&table.name) {
        if idx.columns.first().map(|c| c.name.eq_ignore_ascii_case(col_name)).unwrap_or(false) {
            return Some(idx);
        }
    }
    None
}

/// Extract a top-level `col = value` equality predicate from a WHERE clause.
/// Returns (column_name, value_expression) if found, else None.
///
/// Handles:
/// - `col = literal`  (col on either side)
/// - `col = ?` / `col = :name`
/// - `col = other_col`  (we treat any non-column expression as the value)
///
/// Does NOT split AND chains — only matches if the entire predicate is a single
/// equality. This is a conservative simplification; a real planner would split
/// AND chains and try each conjunct.
pub fn extract_eq_predicate(predicate: &Expr) -> Option<(String, Expr)> {
    if let Expr::Binary { op: BinaryOp::Eq, left, right } = predicate {
        // Try left = literal
        if let Expr::Column { table: _, name } = left.as_ref() {
            // Right side must not be a column ref (we want literal/param/value).
            if !matches!(right.as_ref(), Expr::Column { .. }) {
                return Some((name.clone(), *right.clone()));
            }
        }
        // Try right = literal
        if let Expr::Column { table: _, name } = right.as_ref() {
            if !matches!(left.as_ref(), Expr::Column { .. }) {
                return Some((name.clone(), *left.clone()));
            }
        }
    }
    None
}

/// Apply a WHERE predicate to a `Scan` plan, choosing the cheapest access path:
///
/// - If the predicate is `rowid_alias_col = value` (or `rowid/_rowid_/oid = value`),
///   replace the Scan with a `RowidLookup` (single B+tree point lookup).
/// - Else if the predicate is `indexed_col = value` for some index whose first
///   column is `col`, replace the Scan with an `IndexLookup`.
/// - Else wrap the Scan in a `Filter` (full scan + per-row predicate eval).
///
/// This is shared between the SELECT planner and the UPDATE/DELETE planners
/// so that `UPDATE t SET ... WHERE id = ?` doesn't fall through to a full
/// table scan — a bug that previously made UPDATE-by-PK ~743x slower than
/// SQLite.
///
/// The function is conservative: it only inspects the *top-level* shape of
/// the predicate. `WHERE id = ? AND name = 'foo'` is treated as a Filter
/// because the top-level op is `And`, not `Eq`. A future improvement is to
/// split AND-chains and try each conjunct.
pub fn apply_where_for_scan(catalog: &Catalog, plan: Plan, predicate: &Expr) -> Plan {
    if let Plan::Scan { table, alias, .. } = &plan {
        if let Some((col_name, value_expr)) = extract_eq_predicate(predicate) {
            // Check if it's the rowid alias.
            if let Some(idx) = table.rowid_alias {
                if table.columns[idx].name.eq_ignore_ascii_case(&col_name) {
                    return Plan::RowidLookup {
                        table: table.clone(),
                        alias: alias.clone(),
                        rowid: value_expr,
                    };
                }
            }
            // Check if "rowid" or "_rowid_" is the column.
            if col_name.eq_ignore_ascii_case("rowid")
                || col_name.eq_ignore_ascii_case("_rowid_")
                || col_name.eq_ignore_ascii_case("oid")
            {
                return Plan::RowidLookup {
                    table: table.clone(),
                    alias: alias.clone(),
                    rowid: value_expr,
                };
            }
            // Check if any index has this column as its first column.
            for index in catalog.indexes_on_table(&table.name) {
                if let Some(first_col) = index.columns.first() {
                    if first_col.name.eq_ignore_ascii_case(&col_name) {
                        return Plan::IndexLookup {
                            table: table.clone(),
                            alias: alias.clone(),
                            index,
                            key_exprs: vec![value_expr],
                        };
                    }
                }
            }
        }
    }
    // Default: wrap in a Filter.
    Plan::Filter { input: Box::new(plan), predicate: predicate.clone() }
}

/// Insert a Sort node below the topmost Project / Distinct node in the plan,
/// so the Sort can see all input columns (not just the projected ones).
///
/// Transformation:
///   - `Project { input: X, cols }` → `Project { input: Sort { input: X, terms }, cols }`
///   - `Distinct { input: Project { input: X, cols } }` →
///     `Distinct { input: Project { input: Sort { input: X, terms }, cols } }`
///   - Any other plan: `Sort { input: plan, terms }` (the original behaviour).
///
/// This preserves correctness for `SELECT a FROM t ORDER BY b` where `b` is
/// not in the projection but is in the underlying table. SQLite semantics:
/// ORDER BY may reference any column in the FROM clause or any projection alias.
pub fn insert_sort_below_top(plan: Plan, terms: Vec<OrderTerm>) -> Plan {
    match plan {
        Plan::Project { input, columns } => {
            let sorted = Plan::Sort { input, terms };
            Plan::Project { input: Box::new(sorted), columns }
        }
        Plan::Distinct { input } => {
            // Distinct wraps a Project. Push Sort inside the Project.
            match *input {
                Plan::Project { input: proj_input, columns } => {
                    let sorted = Plan::Sort { input: proj_input, terms };
                    let new_proj = Plan::Project { input: Box::new(sorted), columns };
                    Plan::Distinct { input: Box::new(new_proj) }
                }
                // Distinct over something else — wrap in Sort above Distinct.
                other_inner => {
                    let distinct = Plan::Distinct { input: Box::new(other_inner) };
                    Plan::Sort { input: Box::new(distinct), terms }
                }
            }
        }
        other => Plan::Sort { input: Box::new(other), terms },
    }
}
