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
    /// Materialized CTE results visible in this SELECT's FROM clause
    /// (lowercase CTE name -> (rows, qualified column names)). Populated
    /// by api.rs before planning; FROM references resolve against CTEs
    /// FIRST so a CTE shadows a real table of the same name (SQL standard).
    ctes: HashMap<String, crate::types::CteMaterialization>,
    /// Parent planner's CTE map, merged when a nested SELECT is planned
    /// (CTEs stay visible inside subqueries in the same statement).
    outer_ctes: Option<HashMap<String, crate::types::CteMaterialization>>,
    /// View-expansion recursion depth. Guards against circular view
    /// definitions (`CREATE VIEW t AS SELECT ... FROM t`) and absurdly
    /// deep view nesting, both of which would otherwise recurse until the
    /// stack overflows. Mirrors SQLite's "view X is circularly defined"
    /// error (fuzzers routinely produce self-referencing views).
    view_depth: usize,
}

/// Maximum view-nesting depth before "circularly defined" is reported.
/// Legitimate view chains never get anywhere near this; the limit exists
/// to turn infinite recursion into a graceful semantic error.
const MAX_VIEW_DEPTH: usize = 64;

impl<'a> Planner<'a> {
    pub fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            scopes: vec![HashMap::new()],
            ctes: HashMap::new(),
            outer_ctes: None,
            view_depth: 0,
        }
    }

    /// Plan a SELECT statement.
    ///
    /// WITH clauses are NOT planned here — api.rs materializes CTEs into
    /// rows BEFORE planning and hands them over via `set_ctes`; FROM
    /// references resolve against them (see plan_table_expression). The
    /// old vestigial pass re-planned CTE bodies without CTE scope, which
    /// broke nested WITH clauses.
    pub fn plan_select(&mut self, stmt: &SelectStatement) -> Result<Plan> {
        let plan = self.plan_select_body(&stmt.body)?;

        // Insert Sort BELOW Project / Distinct so it can see all input
        // columns (not just the projected ones). This is required for
        // `SELECT a FROM t ORDER BY b` where `b` is in the table but not
        // in the projection. SQLite semantics: ORDER BY may reference any
        // column in the FROM clause, or any projection alias.
        let plan = if !stmt.order_by.is_empty() {
            let terms = self.resolve_order_by_terms(&stmt.body, &stmt.order_by)?;
            insert_sort_below_top(plan, terms)
        } else {
            plan
        };

        let plan = if stmt.limit.is_some() || stmt.offset.is_some() {
            Plan::Limit {
                input: Box::new(plan),
                count: stmt
                    .limit
                    .clone()
                    .unwrap_or(Expr::Literal(Value::Integer(-1))),
                offset: stmt
                    .offset
                    .clone()
                    .unwrap_or(Expr::Literal(Value::Integer(0))),
            }
        } else {
            plan
        };

        Ok(plan)
    }

    /// Resolve ORDER BY terms so the Sort operator (which sits below the
    /// Project) can evaluate them:
    ///
    /// 1. **Alias resolution** — a bare column reference that matches a
    ///    projection alias (`ORDER BY parity` where the SELECT list has
    ///    `v % 2 AS parity`) is replaced with the aliased expression.
    ///    Previously the Sort evaluated the alias name against the input's
    ///    column list, found nothing, and sorted on all-NULL keys — so
    ///    `SELECT v % 2 AS parity, COUNT(*) ... GROUP BY v % 2 ORDER BY parity`
    ///    silently kept first-seen group order instead of parity order.
    /// 2. **Aggregate rewrite** — when the query is an aggregate query, the
    ///    terms are rewritten with `rewrite_aggregates_and_groups` so
    ///    `ORDER BY COUNT(*)` becomes `__agg_N` and `ORDER BY <group expr>`
    ///    becomes the group-key column, both of which exist in the
    ///    Aggregate operator's output.
    fn resolve_order_by_terms(
        &mut self,
        body: &SelectBody,
        terms: &[OrderTerm],
    ) -> Result<Vec<OrderTerm>> {
        let s = match body {
            SelectBody::Simple(s) => s,
            SelectBody::Binary { .. } => {
                // Set operations resolve their own ORDER BY against the
                // combined output; no rewrite needed here.
                return Ok(terms.to_vec());
            }
        };
        // Ordinal range validation (SQLite errors on out-of-range
        // ordinals for explicit projections; star projections have an
        // unknown width at plan time and are validated at execution).
        let has_star = s
            .columns
            .iter()
            .any(|c| !matches!(c, ResultColumn::Expr { .. }));
        if !has_star {
            for t in terms {
                if let Expr::Literal(Value::Integer(k)) = &t.expr {
                    if *k >= 1 && (*k as usize) > s.columns.len() {
                        return Err(Error::semantic(format!(
                            "{}rd ORDER BY term out of range ({} output columns)",
                            k,
                            s.columns.len()
                        )));
                    }
                }
            }
        }
        let resolved: Vec<OrderTerm> = terms
            .iter()
            .map(|t| {
                let mut expr = t.expr.clone();
                // 0. Ordinal resolution (SQLite semantics): a bare
                //    *positive integer literal* K in ORDER BY refers to
                //    the K-th OUTPUT column of the projection, not to a
                //    constant. `SELECT b, a FROM t ORDER BY 1` sorts by b.
                //    We resolve it here to the K-th projection expression
                //    when that arm is an explicit expression; star/table
                //    projections and compound bodies keep the literal and
                //    let exec_sort resolve it against the materialized row
                //    width (where input order == output order).
                if let Expr::Literal(Value::Integer(k)) = &expr {
                    if *k >= 1 {
                        if let Some(ResultColumn::Expr { expr: ce, .. }) =
                            s.columns.get(*k as usize - 1)
                        {
                            expr = ce.clone();
                        }
                        // else: star projection (validated at execution) —
                        // exec_sort turns the literal into `row[k-1]`.
                    }
                }
                // 1. Alias resolution.
                if let Expr::Column { table: None, name } = &expr {
                    for c in &s.columns {
                        if let ResultColumn::Expr {
                            expr: ce,
                            alias: Some(a),
                        } = c
                        {
                            if a.eq_ignore_ascii_case(name) {
                                expr = ce.clone();
                                break;
                            }
                        }
                    }
                }
                // 2. Aggregate rewrite (mirrors plan_simple_select).
                let has_aggregates = self.expr_list_has_aggregates(&s.columns)
                    || s.having.is_some()
                    || !s.group_by.is_empty();
                if has_aggregates {
                    let aggregates = self
                        .collect_aggregates(&s.columns, s.having.as_ref())
                        .unwrap_or_default();
                    let resolved_group_by: Vec<Expr> = s
                        .group_by
                        .iter()
                        .map(|g| {
                            if let Expr::Column { table: None, name } = g {
                                for c in &s.columns {
                                    if let ResultColumn::Expr {
                                        expr,
                                        alias: Some(a),
                                    } = c
                                    {
                                        if a.eq_ignore_ascii_case(name) {
                                            return expr.clone();
                                        }
                                    }
                                }
                            }
                            g.clone()
                        })
                        .collect();
                    expr = rewrite_aggregates_and_groups(
                        &expr,
                        &aggregates,
                        &resolved_group_by,
                        resolved_group_by.len(),
                    );
                }
                OrderTerm {
                    expr,
                    order: t.order,
                    nulls: t.nulls,
                }
            })
            .collect();
        Ok(resolved)
    }

    fn plan_select_body(&mut self, body: &SelectBody) -> Result<Plan> {
        match body {
            SelectBody::Simple(s) => self.plan_simple_select(s),
            SelectBody::Binary { op, left, right } => {
                let l = self.plan_select_body(left)?;
                let r = self.plan_select_body(right)?;
                match op {
                    SetOp::Union => Ok(Plan::Union {
                        left: Box::new(l),
                        right: Box::new(r),
                        all: false,
                    }),
                    SetOp::UnionAll => Ok(Plan::Union {
                        left: Box::new(l),
                        right: Box::new(r),
                        all: true,
                    }),
                    SetOp::Intersect => Ok(Plan::Intersect {
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    SetOp::Except => Ok(Plan::Except {
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                }
            }
        }
    }

    fn plan_simple_select(&mut self, s: &SimpleSelect) -> Result<Plan> {
        // SQLite's collation resolution: a comparison whose operand is a
        // column with a DECLARED collation (e.g. `email TEXT COLLATE
        // NOCASE`) uses that collation even without an explicit COLLATE in
        // the query. Attach explicit COLLATE nodes once, at plan time, so
        // every evaluation path (compiled predicate fallback, general
        // evaluator, join conditions) sees them.
        let coll_scope: Vec<(std::sync::Arc<crate::schema::Table>, String)> = s
            .from
            .as_ref()
            .map(|from| collect_collation_scope(self.catalog, from))
            .unwrap_or_default();
        let where_rewritten = s
            .where_clause
            .as_ref()
            .map(|p| rewrite_column_collations(self.catalog, p, &coll_scope));
        let having_rewritten = s
            .having
            .as_ref()
            .map(|p| rewrite_column_collations(self.catalog, p, &coll_scope));

        let mut plan = if let Some(from) = &s.from {
            self.plan_table_expression(from)?
        } else {
            Plan::Values { rows: vec![vec![]] }
        };

        // Apply WHERE — with predicate pushdown and index/rowid lookup optimization.
        if let Some(pred) = &where_rewritten {
            plan = self.apply_where(plan, pred);
        }

        // Post-pass: rewrite eligible Hash joins to IndexNestedLoopJoin when
        // the inner side has an index on the join key. This is the single
        // biggest perf win for filtered joins (closes the 240× gap on the
        // 2-table join benchmark).
        plan = optimize_index_nested_loop_join(self.catalog, plan);

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
        let resolved_group_by: Vec<Expr> = s
            .group_by
            .iter()
            .map(|g| {
                if let Expr::Column { table: None, name } = g {
                    for c in &s.columns {
                        if let ResultColumn::Expr {
                            expr,
                            alias: Some(a),
                        } = c
                        {
                            if a.eq_ignore_ascii_case(name) {
                                return expr.clone();
                            }
                        }
                    }
                }
                g.clone()
            })
            .collect();
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
            if let Some(having) = &having_rewritten {
                let rewritten_having =
                    rewrite_aggregates_and_groups(having, &aggregates, &resolved_group_by, n_group);
                plan = Plan::Filter {
                    input: Box::new(plan),
                    predicate: rewritten_having,
                };
            }

            let rewritten_columns: Vec<ProjectExpr> = s
                .columns
                .iter()
                .map(|c| match c {
                    ResultColumn::Star => ProjectExpr {
                        expr: Expr::Column {
                            table: None,
                            name: "*".into(),
                        },
                        alias: None,
                    },
                    ResultColumn::TableStar(t) => ProjectExpr {
                        expr: Expr::Column {
                            table: Some(t.clone()),
                            name: "*".into(),
                        },
                        alias: None,
                    },
                    ResultColumn::Expr { expr, alias } => {
                        let rewritten = rewrite_aggregates_and_groups(
                            expr,
                            &aggregates,
                            &resolved_group_by,
                            n_group,
                        );
                        let alias = alias.clone().or_else(|| {
                            if let Expr::Column { name, .. } = &rewritten {
                                if name.starts_with("__agg_") {
                                    let idx: usize =
                                        name.trim_start_matches("__agg_").parse().ok()?;
                                    Some(aggregates[idx].display_name.clone())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        });
                        ProjectExpr {
                            expr: rewritten,
                            alias,
                        }
                    }
                })
                .collect();
            plan = Plan::Project {
                input: Box::new(plan),
                columns: rewritten_columns,
            };
            return Ok(plan);
        }

        let has_windows = self.expr_list_has_windows(&s.columns);
        if has_windows {
            let windows = self.collect_windows(&s.columns, &s.window)?;
            plan = Plan::Window {
                input: Box::new(plan),
                windows,
            };
        }

        // Project FIRST, then DISTINCT. SQLite semantics: DISTINCT applies
        // to the projected columns, not the underlying row. If we put
        // Distinct before Project, the full row (including the rowid alias)
        // would be the dedup key, and every row would be unique.
        plan = Plan::Project {
            input: Box::new(plan),
            columns: s
                .columns
                .iter()
                .map(|c| self.result_column_to_project(c))
                .collect::<Result<_>>()?,
        };

        if s.distinct {
            plan = Plan::Distinct {
                input: Box::new(plan),
            };
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
        pushdown_filter(self.catalog, plan, predicate)
    }

    /// Provide the materialized CTE map for this statement (api.rs).
    pub fn set_ctes(&mut self, ctes: HashMap<String, crate::types::CteMaterialization>) {
        self.ctes = ctes;
    }

    /// Effective CTE map: own + inherited from the enclosing planner.
    fn effective_cte(&self, name: &str) -> Option<crate::types::CteMaterialization> {
        if let Some(v) = self.ctes.get(name) {
            return Some(v.clone());
        }
        self.outer_ctes.as_ref().and_then(|m| m.get(name).cloned())
    }

    /// Public wrapper for `UPDATE ... FROM` (SQLite 3.33+): plan an
    /// arbitrary FROM-side table expression (table / subquery / join).
    pub(crate) fn plan_table_expression_pub(&mut self, te: &TableExpression) -> Result<Plan> {
        self.plan_table_expression(te)
    }

    fn plan_table_expression(&mut self, te: &TableExpression) -> Result<Plan> {
        match te {
            TableExpression::Table {
                name,
                alias,
                indexed,
                ..
            } => {
                // CTE reference? (WITH ... name AS (...)). CTEs shadow real
                // tables of the same name.
                if indexed.is_none() {
                    if let Some((rows, cols)) = self.effective_cte(&name.to_ascii_lowercase()) {
                        // Rebind the column names to the effective alias so
                        // `SELECT c.x FROM cte c` resolves: qualify with the
                        // alias when present, else the CTE name.
                        let prefix = alias.clone().unwrap_or_else(|| name.clone());
                        let ql: Arc<[String]> = if prefix.eq_ignore_ascii_case(name) {
                            cols.clone()
                        } else {
                            cols.iter()
                                .map(|c| {
                                    let suffix = c.rsplit('.').next().unwrap_or(c);
                                    format!("{}.{}", prefix, suffix)
                                })
                                .collect::<Vec<String>>()
                                .into()
                        };
                        return Ok(Plan::CteRows {
                            rows: rows.clone(),
                            columns: ql,
                        });
                    }
                }
                // VIEW reference: expand to the view's SELECT (recursively —
                // views may reference other views). The statement cache is
                // invalidated on CREATE/DROP VIEW, so plans never hold a
                // stale view definition.
                if indexed.is_none() {
                    if let Some(view) = self.catalog.get_view(name) {
                        // Circular-view guard: a view whose SELECT (directly
                        // or transitively) references itself would recurse
                        // plan_table_expression -> plan_select -> ... until
                        // the stack overflows. Depth-limit it into the same
                        // graceful error SQLite produces.
                        if self.view_depth >= MAX_VIEW_DEPTH {
                            return Err(Error::semantic(format!(
                                "view {} is circularly defined (or nested more than {} levels deep)",
                                name,
                                MAX_VIEW_DEPTH
                            )));
                        }
                        self.view_depth += 1;
                        let inner = self.plan_select(&view.select);
                        self.view_depth -= 1;
                        let inner = inner?;
                        // Optional column rename (CREATE VIEW v(a, b) AS
                        // ...): wrap in a Project aliasing the view select's
                        // top-level output columns positionally.
                        let plan = match (&view.columns, top_level_output_names(&view.select)) {
                            (Some(renames), Some(inner_names))
                                if renames.len() == inner_names.len() =>
                            {
                                let prefix = alias.clone().unwrap_or_else(|| name.clone());
                                let cols: Vec<crate::planner::plan::ProjectExpr> = renames
                                    .iter()
                                    .zip(inner_names.iter())
                                    .map(|(new, old)| crate::planner::plan::ProjectExpr {
                                        expr: Expr::Column {
                                            table: None,
                                            name: old.clone(),
                                        },
                                        alias: Some(format!("{}.{}", prefix, new)),
                                    })
                                    .collect();
                                Plan::Project {
                                    input: Box::new(Plan::Subquery {
                                        plan: Box::new(inner),
                                    }),
                                    columns: cols,
                                }
                            }
                            (Some(renames), _) => {
                                return Err(Error::semantic(format!(
                                    "view {} declares {} columns but its SELECT is too complex to rename (use explicit column aliases)",
                                    name,
                                    renames.len()
                                )));
                            }
                            _ => Plan::Subquery {
                                plan: Box::new(inner),
                            },
                        };
                        return Ok(plan);
                    }
                }
                let table = self
                    .catalog
                    .get_table(name)
                    .ok_or_else(|| Error::NotFound(format!("table: {}", name)))?;
                // Pending virtual table (module not registered yet): the
                // column list is unknown until xConnect, so planning
                // would produce a wrong schema. Modules must be
                // registered with `Database::create_module` first (the
                // registration connects pending vtabs) — same rule as
                // SQLite's runtime module linkage.
                if let Some(vt) = &table.vtab {
                    if vt.is_pending() {
                        return Err(Error::semantic(format!(
                            "no such module: {} (register it with Database::create_module before use)",
                            vt.module_name
                        )));
                    }
                }
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
                Ok(Plan::Subquery {
                    plan: Box::new(inner),
                })
            }
            TableExpression::Join {
                left,
                right,
                join_type,
                constraint,
            } => {
                let l = self.plan_table_expression(left)?;
                let r = self.plan_table_expression(right)?;
                // Collation scope for the join condition spans BOTH sides.
                let mut join_scope = collect_collation_scope(self.catalog, left);
                join_scope.extend(collect_collation_scope(self.catalog, right));
                let condition = match constraint {
                    JoinConstraint::On(e) => {
                        Some(rewrite_column_collations(self.catalog, e, &join_scope))
                    }
                    JoinConstraint::Using(cols) => {
                        let mut combined = None;
                        for c in cols {
                            let e = Expr::Binary {
                                op: BinaryOp::Eq,
                                left: Box::new(Expr::Column {
                                    table: None,
                                    name: c.clone(),
                                }),
                                right: Box::new(Expr::Column {
                                    table: None,
                                    name: c.clone(),
                                }),
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
                // JoinType is shared with the AST — no conversion needed.
                let jt = *join_type;
                let algo = if matches!(
                    constraint,
                    JoinConstraint::Natural | JoinConstraint::Using(_)
                ) {
                    JoinAlgorithm::Hash
                } else if let Some(Expr::Binary {
                    op: BinaryOp::Eq, ..
                }) = &condition
                {
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
            ResultColumn::Star => Ok(ProjectExpr {
                expr: Expr::Column {
                    table: None,
                    name: "*".into(),
                },
                alias: None,
            }),
            ResultColumn::TableStar(t) => Ok(ProjectExpr {
                expr: Expr::Column {
                    table: Some(t.clone()),
                    name: "*".into(),
                },
                alias: None,
            }),
            ResultColumn::Expr { expr, alias } => Ok(ProjectExpr {
                expr: expr.clone(),
                alias: alias.clone(),
            }),
        }
    }

    fn expr_list_has_aggregates(&self, cols: &[ResultColumn]) -> bool {
        cols.iter()
            .any(|c| matches!(c, ResultColumn::Expr { expr, .. } if expr_has_aggregate(expr)))
    }

    fn expr_list_has_windows(&self, cols: &[ResultColumn]) -> bool {
        cols.iter()
            .any(|c| matches!(c, ResultColumn::Expr { expr, .. } if expr_has_window(expr)))
    }

    fn collect_aggregates(
        &self,
        cols: &[ResultColumn],
        having: Option<&Expr>,
    ) -> Result<Vec<AggExpr>> {
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

    fn collect_windows(
        &self,
        cols: &[ResultColumn],
        defs: &[WindowDef],
    ) -> Result<Vec<WindowExpr>> {
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
            // Match the column-naming convention used by exec_aggregate
            // (named after the source expression, falling back to "colN").
            // Without this consistency, the rewritten column reference
            // wouldn't resolve in the Aggregate's output and the Project
            // would emit NULLs for what should be the group key.
            let name = match g {
                Expr::Column { table: None, name } => name.clone(),
                Expr::Column {
                    table: Some(t),
                    name,
                } => format!("{}.{}", t, name),
                _ => format!("col{}", i + 1),
            };
            return Expr::Column { table: None, name };
        }
    }
    // Otherwise, rewrite aggregates and recurse.
    match e {
        Expr::Function {
            name,
            distinct,
            args,
            over,
            filter,
        } => {
            // Use is_aggregate_call so the polymorphic scalar forms
            // (MIN(a,b), MAX(a,b,c)) are NOT rewritten to aggregate
            // columns when a real same-name aggregate exists elsewhere in
            // the query.
            if over.is_none() && is_aggregate_call(&name.to_ascii_lowercase(), args.len()) {
                // The aggregate's INPUT expression: `None` for star / no-arg
                // calls (COUNT(*)), otherwise the first argument.
                let call_arg: Option<&Expr> = if args.is_empty()
                    || args
                        .first()
                        .map(|a| matches!(a, Expr::Column { name, .. } if name == "*"))
                        .unwrap_or(false)
                {
                    None
                } else {
                    args.first()
                };
                for (i, agg) in aggregates.iter().enumerate() {
                    if agg.func != name.to_ascii_lowercase() || agg.distinct != *distinct {
                        continue;
                    }
                    // Match on the argument expression as well: two calls
                    // of the same function with DIFFERENT arguments are
                    // different aggregates. Previously only the function
                    // name + distinct + star-ness were compared, so
                    // `SELECT SUM(qty), SUM(price) ...` rewrote BOTH to
                    // __agg_0 and the second aggregate silently reported
                    // the first one's value. Expr has no PartialEq, so use
                    // the same Display-based structural heuristic as the
                    // GROUP BY matcher above.
                    let args_match = match (&agg.arg, call_arg) {
                        (None, None) => true,
                        (Some(a), Some(c)) => format!("{:?}", a) == format!("{:?}", c),
                        _ => false,
                    };
                    if args_match {
                        let col_name = format!("__agg_{}", i);
                        return Expr::Column {
                            table: None,
                            name: col_name,
                        };
                    }
                }
                e.clone()
            } else {
                let new_args: Vec<Expr> = args
                    .iter()
                    .map(|a| rewrite_aggregates_and_groups(a, aggregates, group_by, n_group))
                    .collect();
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
            left: Box::new(rewrite_aggregates_and_groups(
                left, aggregates, group_by, n_group,
            )),
            right: Box::new(rewrite_aggregates_and_groups(
                right, aggregates, group_by, n_group,
            )),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_aggregates_and_groups(
                expr, aggregates, group_by, n_group,
            )),
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rewrite_aggregates_and_groups(
                expr, aggregates, group_by, n_group,
            )),
            low: Box::new(rewrite_aggregates_and_groups(
                low, aggregates, group_by, n_group,
            )),
            high: Box::new(rewrite_aggregates_and_groups(
                high, aggregates, group_by, n_group,
            )),
            negated: *negated,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_aggregates_and_groups(
                expr, aggregates, group_by, n_group,
            )),
            negated: *negated,
        },
        Expr::Is {
            left,
            right,
            negated,
        } => Expr::Is {
            left: Box::new(rewrite_aggregates_and_groups(
                left, aggregates, group_by, n_group,
            )),
            right: Box::new(rewrite_aggregates_and_groups(
                right, aggregates, group_by, n_group,
            )),
            negated: *negated,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            let new_whens: Vec<(Expr, Expr)> = whens
                .iter()
                .map(|(c, v)| {
                    (
                        rewrite_aggregates_and_groups(c, aggregates, group_by, n_group),
                        rewrite_aggregates_and_groups(v, aggregates, group_by, n_group),
                    )
                })
                .collect();
            Expr::Case {
                operand: operand.as_ref().map(|o| {
                    Box::new(rewrite_aggregates_and_groups(
                        o, aggregates, group_by, n_group,
                    ))
                }),
                whens: new_whens,
                else_: else_.as_ref().map(|e| {
                    Box::new(rewrite_aggregates_and_groups(
                        e, aggregates, group_by, n_group,
                    ))
                }),
            }
        }
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_aggregates_and_groups(
                expr, aggregates, group_by, n_group,
            )),
            type_name: type_name.clone(),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_aggregates_and_groups(
                expr, aggregates, group_by, n_group,
            )),
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
        Expr::Function {
            name,
            over,
            args,
            filter,
            ..
        } => {
            if over.is_none() && is_aggregate_call(&name.to_ascii_lowercase(), args.len()) {
                return true;
            }
            // Aggregates can nest inside scalar-function arguments —
            // `COALESCE(SUM(x), 0)`, `ABS(AVG(x))`, `ROUND(SUM(x), 2)` are
            // aggregate queries. The top-level call alone must NOT decide
            // (it previously did, so those shapes silently lost their
            // Aggregate plan and evaluated per-row instead of per-group).
            args.iter().any(expr_has_aggregate)
                || filter
                    .as_ref()
                    .map(|e| expr_has_aggregate(e))
                    .unwrap_or(false)
        }
        Expr::Binary { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::Unary { expr, .. } => expr_has_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => expr_has_aggregate(expr) || expr_has_aggregate(low) || expr_has_aggregate(high),
        Expr::In { expr, source, .. } => {
            expr_has_aggregate(expr)
                || matches!(source, InSource::List(l) if l.iter().any(expr_has_aggregate))
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_has_aggregate(expr)
                || expr_has_aggregate(pattern)
                || escape
                    .as_ref()
                    .map(|e| expr_has_aggregate(e))
                    .unwrap_or(false)
        }
        Expr::IsNull { expr, .. } => expr_has_aggregate(expr),
        Expr::Is { left, right, .. } => expr_has_aggregate(left) || expr_has_aggregate(right),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand
                .as_ref()
                .map(|e| expr_has_aggregate(e))
                .unwrap_or(false)
                || whens
                    .iter()
                    .any(|(c, v)| expr_has_aggregate(c) || expr_has_aggregate(v))
                || else_
                    .as_ref()
                    .map(|e| expr_has_aggregate(e))
                    .unwrap_or(false)
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
        Expr::Between {
            expr, low, high, ..
        } => expr_has_window(expr) || expr_has_window(low) || expr_has_window(high),
        Expr::In { expr, source, .. } => {
            expr_has_window(expr)
                || matches!(source, InSource::List(l) if l.iter().any(expr_has_window))
        }
        Expr::IsNull { expr, .. } => expr_has_window(expr),
        Expr::Is { left, right, .. } => expr_has_window(left) || expr_has_window(right),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand
                .as_ref()
                .map(|e| expr_has_window(e))
                .unwrap_or(false)
                || whens
                    .iter()
                    .any(|(c, v)| expr_has_window(c) || expr_has_window(v))
                || else_.as_ref().map(|e| expr_has_window(e)).unwrap_or(false)
        }
        Expr::Cast { expr, .. } => expr_has_window(expr),
        Expr::Collate { expr, .. } => expr_has_window(expr),
        _ => false,
    }
}

/// Returns true if the function name is an aggregate.
///
/// Note: `min` and `max` are *polymorphic* in SQLite — they're aggregates
/// when called with a single argument (e.g. `MAX(score)` over all rows),
/// but scalar functions when called with 2+ arguments (e.g. `MAX(1, 5, 3)`
/// returns 5). The planner uses `is_aggregate_call(name, n_args)` to
/// disambiguate.
pub fn is_aggregate_fn(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "avg" | "min" | "max" | "total" | "group_concat"
    ) || crate::plugin::lookup_aggregate(&name.to_ascii_lowercase()).is_some()
}

/// Returns true if the function name is an aggregate *when called with the
/// given number of arguments*. This is what the planner should use — it
/// correctly handles the polymorphic `min`/`max` distinction:
///   - `MAX(col)`        → 1 arg → aggregate.
///   - `MAX(1, 5, 3)`    → 3 args → scalar.
pub fn is_aggregate_call(name: &str, n_args: usize) -> bool {
    let lc = name.to_ascii_lowercase();
    match lc.as_str() {
        "count" | "sum" | "avg" | "total" | "group_concat" => true,
        "min" | "max" => n_args <= 1,
        _ => crate::plugin::lookup_aggregate(&lc).is_some(),
    }
}

/// Returns true if the function name is a window-only function.
#[allow(dead_code)]
pub fn is_window_only_fn(name: &str) -> bool {
    matches!(
        name,
        "row_number"
            | "rank"
            | "dense_rank"
            | "percent_rank"
            | "cume_dist"
            | "ntile"
            | "lag"
            | "lead"
            | "first_value"
            | "last_value"
            | "nth_value"
    )
}

/// SQLite-style output name for an aggregate/window call: `COUNT(*)`,
/// `SUM(x)`, `COUNT(DISTINCT y)`. (SQLite's short-column-name rule.)
fn aggregate_display_name(name: &str, distinct: bool, args: &[crate::sql::ast::Expr]) -> String {
    use crate::sql::ast::Expr;
    let rendered: Vec<String> = args
        .iter()
        .map(|a| match a {
            Expr::Column { name, .. } if name == "*" => "*".to_string(),
            Expr::Column { name, .. } => name.clone(),
            Expr::Literal(v) => format!("{}", v),
            other => crate::executor::expr_display_name(other),
        })
        .collect();
    if rendered.is_empty() {
        return format!("{}(*)", name);
    }
    if distinct {
        format!("{}(DISTINCT {})", name, rendered.join(", "))
    } else {
        format!("{}({})", name, rendered.join(", "))
    }
}

fn collect_aggregates_rec(e: &Expr, alias: &Option<String>, out: &mut Vec<AggExpr>) {
    match e {
        Expr::Function {
            name,
            distinct,
            args,
            over,
            filter,
        } => {
            // Use is_aggregate_call (not is_aggregate_fn) so that the
            // polymorphic min/max distinction is respected: MAX(col) is
            // an aggregate (1 arg), MAX(1, 5, 3) is a scalar call (3 args).
            if over.is_none() && is_aggregate_call(&name.to_ascii_lowercase(), args.len()) {
                let arg = if args.is_empty()
                    || (args.len() == 1
                        && matches!(&args[0], Expr::Column { name, .. } if name == "*"))
                {
                    None
                } else {
                    // MAX(x) / MIN(x) / SUM(x): the single argument is the
                    // aggregate input; multi-arg calls (e.g. MAX(1,5,3))
                    // also use the first argument.
                    Some(args[0].clone())
                };
                out.push(AggExpr {
                    func: name.to_ascii_lowercase(),
                    arg,
                    distinct: *distinct,
                    alias: alias.clone(),
                    display_name: aggregate_display_name(name, *distinct, args),
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
        Expr::Between {
            expr, low, high, ..
        } => {
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
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
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
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
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
        Expr::Function {
            name,
            distinct,
            args,
            over,
            ..
        } => {
            if let Some(spec) = over {
                let (partition_by, order_by, frame) = match spec.as_ref() {
                    WindowSpec::Named(_) => (Vec::new(), Vec::new(), None),
                    WindowSpec::Inline(def) => (
                        def.partition_by.clone(),
                        def.order_by.clone(),
                        def.frame.as_ref().map(|f| (**f).clone()),
                    ),
                };
                let arg = if args.is_empty()
                    || (args.len() == 1
                        && matches!(&args[0], Expr::Column { name, .. } if name == "*"))
                {
                    None
                } else {
                    // MAX(x) / MIN(x) / SUM(x): the single argument is the
                    // aggregate input; multi-arg calls (e.g. MAX(1,5,3))
                    // also use the first argument.
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
                    display_name: aggregate_display_name(name, *distinct, args),
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
pub fn find_index_for_column(
    catalog: &Catalog,
    table: &Table,
    col_name: &str,
) -> Option<Arc<Index>> {
    catalog
        .indexes_on_table(&table.name)
        .into_iter()
        .find(|idx| {
            idx.columns
                .first()
                .map(|c| c.name.eq_ignore_ascii_case(col_name))
                .unwrap_or(false)
        })
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
    if let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = predicate
    {
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
/// - If the predicate is `rowid_alias_col = value`, use `RowidLookup`.
/// - Else if the predicate is an AND-chain of range conjuncts on the rowid-alias
///   column (`col > v`, `col >= v`, `col < v`, `col <= v`, `col BETWEEN a AND b`),
///   use `RowidRange` with the tightest (start, end) bounds and a residual
///   predicate for any conjuncts that can't be expressed as a bound.
/// - Else if the predicate is `indexed_col = value` for some index whose first
///   column is `col`, use `IndexLookup`.
/// - Else wrap the Scan in a `Filter` (full scan + per-row predicate eval).
///
/// This is shared between the SELECT planner and the UPDATE/DELETE planners
/// so that `UPDATE t SET ... WHERE id = ?` doesn't fall through to a full
/// table scan — a bug that previously made UPDATE-by-PK ~743x slower than
/// SQLite.
pub fn apply_where_for_scan(catalog: &Catalog, plan: Plan, predicate: &Expr) -> Plan {
    if let Plan::Scan { table, alias, .. } = &plan {
        // Split AND-chains so we can pick the best access path per-conjunct.
        let conjuncts = split_and_chain(predicate);
        // Try to construct a RowidRange from conjuncts that reference the
        // rowid-alias column. Returns (start, end, residual_conjuncts).
        if let Some((start, end, residual)) = try_rowid_range(&conjuncts, table) {
            // If we have a single equality `id = ?`, RowidLookup is preferable
            // (one B+tree seek vs a range walk).
            if let (Some(s), Some(e)) = (&start, &end) {
                // Structural compare via Debug string — Expr doesn't impl PartialEq
                // and adding it would be a wider change. This is planning-only,
                // not per-row, so the format!() cost is negligible.
                if format!("{:?}", s) == format!("{:?}", e) {
                    return Plan::RowidLookup {
                        table: table.clone(),
                        alias: alias.clone(),
                        rowid: s.clone(),
                    };
                }
            }
            return Plan::RowidRange {
                table: table.clone(),
                alias: alias.clone(),
                start,
                end,
                residual,
            };
        }
        // Rowid IN-list: `WHERE id IN (v1, v2, ...)` — a batched
        // multi-seek instead of a full scan + per-row IN evaluation (which
        // previously cost a 10k-row table scan for 10 literal rowids).
        if let Some((in_plan, residual_conjuncts)) = try_rowid_in(&conjuncts, table, alias) {
            if residual_conjuncts.is_empty() {
                return in_plan;
            }
            return Plan::Filter {
                input: Box::new(in_plan),
                predicate: combine_and(&residual_conjuncts),
            };
        }
        // Indexed-column IN-list: `WHERE indexed_col IN (v1, v2, ...)` —
        // one index seek per member instead of a full table scan.
        if let Some((in_plan, residual_conjuncts)) = try_index_in(catalog, &conjuncts, table, alias)
        {
            if residual_conjuncts.is_empty() {
                return in_plan;
            }
            return Plan::Filter {
                input: Box::new(in_plan),
                predicate: combine_and(&residual_conjuncts),
            };
        }
        // Fall back to per-conjunct equality handling (original path).
        for conjunct in &conjuncts {
            if let Some((col_name, value_expr)) = extract_eq_predicate(conjunct) {
                // Check if it's the rowid alias.
                if let Some(idx) = table.rowid_alias {
                    if table.columns[idx].name.eq_ignore_ascii_case(&col_name) {
                        // For `id = ? AND other = ?`, use RowidLookup for `id = ?`
                        // and put the remaining conjuncts in a top-level Filter.
                        let other_conjuncts: Vec<Expr> = conjuncts
                            .iter()
                            .filter(|c| !exprs_equal_conjunct(c, conjunct))
                            .cloned()
                            .collect();
                        let lookup = Plan::RowidLookup {
                            table: table.clone(),
                            alias: alias.clone(),
                            rowid: value_expr,
                        };
                        if other_conjuncts.is_empty() {
                            return lookup;
                        }
                        return Plan::Filter {
                            input: Box::new(lookup),
                            predicate: combine_and(&other_conjuncts),
                        };
                    }
                }
                // Check if "rowid" or "_rowid_" is the column.
                if col_name.eq_ignore_ascii_case("rowid")
                    || col_name.eq_ignore_ascii_case("_rowid_")
                    || col_name.eq_ignore_ascii_case("oid")
                {
                    let other_conjuncts: Vec<Expr> = conjuncts
                        .iter()
                        .filter(|c| !exprs_equal_conjunct(c, conjunct))
                        .cloned()
                        .collect();
                    let lookup = Plan::RowidLookup {
                        table: table.clone(),
                        alias: alias.clone(),
                        rowid: value_expr,
                    };
                    if other_conjuncts.is_empty() {
                        return lookup;
                    }
                    return Plan::Filter {
                        input: Box::new(lookup),
                        predicate: combine_and(&other_conjuncts),
                    };
                }
                // Check if any index has this column as its first column.
                for index in catalog.indexes_on_table(&table.name) {
                    if let Some(first_col) = index.columns.first() {
                        if first_col.name.eq_ignore_ascii_case(&col_name) {
                            let other_conjuncts: Vec<Expr> = conjuncts
                                .iter()
                                .filter(|c| !exprs_equal_conjunct(c, conjunct))
                                .cloned()
                                .collect();
                            let lookup = Plan::IndexLookup {
                                table: table.clone(),
                                alias: alias.clone(),
                                index,
                                key_exprs: vec![value_expr],
                            };
                            if other_conjuncts.is_empty() {
                                return lookup;
                            }
                            return Plan::Filter {
                                input: Box::new(lookup),
                                predicate: combine_and(&other_conjuncts),
                            };
                        }
                    }
                }
            }
        }
        // No rowid/equality access path matched. Try an IndexRange from
        // conjuncts that are range predicates on the first column of some
        // index (e.g. `val > 5000` with idx_val, or `val BETWEEN 10 AND 20`).
        if let Some(range_plan) = try_index_range(catalog, &conjuncts, table, alias) {
            return range_plan;
        }
    }
    // Default: wrap in a Filter.
    Plan::Filter {
        input: Box::new(plan),
        predicate: predicate.clone(),
    }
}

/// Compare two expressions for structural equality (used to filter out the
/// conjunct that became the access-path predicate from the residual list).
fn exprs_equal_conjunct(a: &Expr, b: &Expr) -> bool {
    // Cheap structural compare via Debug string. Good enough — this is
    // only called during planning, not per-row.
    format!("{:?}", a) == format!("{:?}", b)
}

/// Detect `first-indexed-col IN (list-of-expressions)` among the
/// conjuncts. Only single-column-key indexes (the common case) — the
/// IN-list replaces the equality key. Non-negated only.
fn try_index_in(
    catalog: &Catalog,
    conjuncts: &[Expr],
    table: &Arc<Table>,
    alias: &Option<String>,
) -> Option<(Plan, Vec<Expr>)> {
    for (i, conjunct) in conjuncts.iter().enumerate() {
        if let Expr::In {
            expr,
            source: InSource::List(list),
            negated: false,
        } = conjunct
        {
            if let Expr::Column { table: None, name } = expr.as_ref() {
                for index in catalog.indexes_on_table(&table.name) {
                    // Single-column index whose (only) column matches: each
                    // list member becomes one equality seek.
                    if index.columns.len() == 1 && index.columns[0].name.eq_ignore_ascii_case(name)
                    {
                        let others: Vec<Expr> = conjuncts
                            .iter()
                            .enumerate()
                            .filter(|(j, _)| *j != i)
                            .map(|(_, c)| c.clone())
                            .collect();
                        return Some((
                            Plan::IndexIn {
                                table: table.clone(),
                                alias: alias.clone(),
                                index,
                                key_exprs: list.clone(),
                                residual: None,
                            },
                            others,
                        ));
                    }
                }
            }
        }
    }
    None
}

/// Detect `rowid-alias-col IN (list-of-expressions)` among the conjuncts.
/// Returns the RowidIn plan plus the remaining conjuncts (for a residual
/// Filter). Only handles the positive (non-negated) form; `NOT IN` keeps
/// the generic Filter path (it must scan everything anyway).
fn try_rowid_in(
    conjuncts: &[Expr],
    table: &Arc<Table>,
    alias: &Option<String>,
) -> Option<(Plan, Vec<Expr>)> {
    for (i, conjunct) in conjuncts.iter().enumerate() {
        if let Expr::In {
            expr,
            source: InSource::List(list),
            negated: false,
        } = conjunct
        {
            if let Expr::Column { table: None, name } = expr.as_ref() {
                let is_rowid_alias = table
                    .rowid_alias
                    .map(|idx| table.columns[idx].name.eq_ignore_ascii_case(name))
                    .unwrap_or(false);
                let is_rowid_pseudo = name.eq_ignore_ascii_case("rowid")
                    || name.eq_ignore_ascii_case("_rowid_")
                    || name.eq_ignore_ascii_case("oid");
                if is_rowid_alias || is_rowid_pseudo {
                    let others: Vec<Expr> = conjuncts
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, c)| c.clone())
                        .collect();
                    return Some((
                        Plan::RowidIn {
                            table: table.clone(),
                            alias: alias.clone(),
                            values: list.clone(),
                            residual: None,
                        },
                        others,
                    ));
                }
            }
        }
    }
    None
}

/// Try to build a RowidRange (start, end, residual) from a list of conjuncts
/// where at least one references the rowid-alias column (or rowid/_rowid_/oid).
///
/// Returns `None` if no conjunct references the rowid-alias column, or if the
/// only such reference is an equality `id = ?` (which RowidLookup handles
/// better than RowidRange).
///
/// Recognized conjunct forms on the rowid-alias column:
/// - `col BETWEEN ? AND ?` — sets both start and end
/// - `col > ?` / `col >= ?`  — sets start
/// - `col < ?` / `col <= ?`  — sets end
/// - `? > col` / `? >= col` — sets end (col on right)
/// - `? < col` / `? <= col` — sets start
///
/// For an equality `col = ?` we DO take it as setting both start and end to
/// the same value (so RowidRange degenerates to a point). The caller checks
/// for this case and prefers RowidLookup.
fn try_rowid_range(
    conjuncts: &[Expr],
    table: &Table,
) -> Option<(Option<Expr>, Option<Expr>, Option<Expr>)> {
    let rowid_col_name = table
        .rowid_alias
        .and_then(|idx| table.columns.get(idx))
        .map(|c| c.name.clone());
    let mut start: Option<Expr> = None;
    let mut end: Option<Expr> = None;
    let mut residual: Vec<Expr> = Vec::new();
    let mut saw_rowid_ref = false;

    for conjunct in conjuncts {
        // Try `col BETWEEN ? AND ?`
        if let Some((col, lo, hi)) = extract_between(conjunct) {
            if is_rowid_col(&col, &rowid_col_name) {
                saw_rowid_ref = true;
                start = Some(lo);
                end = Some(hi);
                continue;
            }
        }
        // Try `col OP value` or `value OP col` for <, <=, >, >=
        if let Some((col, op, val)) = extract_range(conjunct) {
            if is_rowid_col(&col, &rowid_col_name) {
                saw_rowid_ref = true;
                match op.as_str() {
                    ">" => {
                        // col > v  → start = v + 1 (inclusive)
                        // We model as start = v with strict flag, but RowidRange
                        // is inclusive. To keep semantics correct we transform
                        // `col > v` into `col >= v + 1` at execution time via
                        // a residual predicate; here we set start = v as a hint
                        // and push the strict `col > v` back into residual.
                        start = Some(val.clone());
                        residual.push(conjunct.clone());
                    }
                    ">=" => {
                        start = Some(val);
                    }
                    "<" => {
                        end = Some(val.clone());
                        residual.push(conjunct.clone());
                    }
                    "<=" => {
                        end = Some(val);
                    }
                    _ => {}
                }
                continue;
            }
        }
        // Equality `col = ?` — if this is the rowid-alias, set both start and
        // end to the same value (degenerate range). The caller will prefer
        // RowidLookup, but if there are other conjuncts we still want the
        // residual to capture them.
        if let Some((col, val)) = extract_eq_predicate(conjunct) {
            if is_rowid_col(&col, &rowid_col_name) {
                saw_rowid_ref = true;
                start = Some(val.clone());
                end = Some(val);
                continue;
            }
        }
        // Otherwise it's a residual predicate.
        residual.push(conjunct.clone());
    }

    if !saw_rowid_ref {
        return None;
    }
    let residual_opt = if residual.is_empty() {
        None
    } else {
        Some(combine_and(&residual))
    };
    Some((start, end, residual_opt))
}

/// Check if a column name (possibly with table qualifier) refers to the
/// rowid-alias column or to the magic `rowid`/`_rowid_`/`oid` names.
fn is_rowid_col(col_name: &str, rowid_alias_name: &Option<String>) -> bool {
    // Strip any table qualifier: "u.id" → "id".
    let bare = col_name.rsplit('.').next().unwrap_or(col_name);
    if let Some(alias) = rowid_alias_name {
        if bare.eq_ignore_ascii_case(alias) {
            return true;
        }
    }
    bare.eq_ignore_ascii_case("rowid")
        || bare.eq_ignore_ascii_case("_rowid_")
        || bare.eq_ignore_ascii_case("oid")
}

/// Try to build an IndexRange plan from range predicates on the first
/// column of some index on the table.
///
/// Recognized conjunct forms (on the index's FIRST column):
/// - `col BETWEEN lo AND hi` — sets both bounds (inclusive)
/// - `col > ?` / `col >= ?`  — sets the lower bound
/// - `col < ?` / `col <= ?`  — sets the upper bound
/// - `? > col` / `? >= col` — sets the upper bound (col on right)
/// - `? < col` / `? <= col` — sets the lower bound
///
/// Equality conjuncts are NOT consumed here — the IndexLookup path in
/// `apply_where_for_scan` handles those (and runs first).
///
/// Returns None when no indexed column has a range predicate.
fn try_index_range(
    catalog: &Catalog,
    conjuncts: &[Expr],
    table: &Arc<Table>,
    alias: &Option<String>,
) -> Option<Plan> {
    let indexes = catalog.indexes_on_table(&table.name);
    if indexes.is_empty() {
        return None;
    }
    // For each index, try to collect bounds on its first column.
    for index in &indexes {
        let first_col = index.columns.first()?;
        let first_name = first_col.name.to_ascii_lowercase();
        let mut start: Option<(Expr, bool)> = None;
        let mut end: Option<(Expr, bool)> = None;
        let mut residual: Vec<Expr> = Vec::new();
        let mut matched = false;

        for conjunct in conjuncts {
            let bare_col =
                |s: &str| -> String { s.rsplit('.').next().unwrap_or(s).to_ascii_lowercase() };
            // BETWEEN.
            if let Some((col, lo, hi)) = extract_between(conjunct) {
                if bare_col(&col) == first_name {
                    matched = true;
                    start = Some((lo, true));
                    end = Some((hi, true));
                    continue;
                }
            }
            // Range ops.
            if let Some((col, op, val)) = extract_range(conjunct) {
                if bare_col(&col) == first_name {
                    matched = true;
                    match op.as_str() {
                        ">" => start = Some((val, false)),
                        ">=" => start = Some((val, true)),
                        "<" => end = Some((val, false)),
                        "<=" => end = Some((val, true)),
                        _ => {}
                    }
                    continue;
                }
            }
            residual.push(conjunct.clone());
        }

        if !matched {
            continue;
        }
        let residual_opt = if residual.is_empty() {
            None
        } else {
            Some(combine_and(&residual))
        };
        return Some(Plan::IndexRange {
            table: table.clone(),
            alias: alias.clone(),
            index: index.clone(),
            start,
            end,
            residual: residual_opt,
        });
    }
    None
}

/// Extract `col BETWEEN lo AND hi` from an expression.
/// Returns (col_name, lo, hi) on match.
fn extract_between(expr: &Expr) -> Option<(String, Expr, Expr)> {
    if let Expr::Between {
        expr, low, high, ..
    } = expr
    {
        if let Expr::Column { name, .. } = expr.as_ref() {
            return Some((name.clone(), *low.clone(), *high.clone()));
        }
    }
    None
}

/// Extract a range comparison `col OP value` or `value OP col` where OP is
/// one of `<`, `<=`, `>`, `>=`. Returns (col_name, op_string, value_expr).
fn extract_range(expr: &Expr) -> Option<(String, String, Expr)> {
    if let Expr::Binary { op, left, right } = expr {
        let op_str = match op {
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            _ => return None,
        };
        // `col OP value`
        if let (Expr::Column { name: col, .. }, rhs) = (left.as_ref(), right.as_ref()) {
            if !matches!(rhs, Expr::Column { .. }) {
                return Some((col.clone(), op_str.to_string(), *right.clone()));
            }
        }
        // `value OP col` — flip the direction.
        if let (Expr::Column { name: col, .. }, lhs) = (right.as_ref(), left.as_ref()) {
            if !matches!(lhs, Expr::Column { .. }) {
                let flipped = match op_str {
                    "<" => ">",
                    "<=" => ">=",
                    ">" => "<",
                    ">=" => "<=",
                    _ => return None,
                };
                return Some((col.clone(), flipped.to_string(), *left.clone()));
            }
        }
    }
    None
}

/// Split a predicate that may be an AND-chain into its individual conjuncts.
/// `a AND b AND c` → `[a, b, c]`. A single conjunct returns `[conjunct]`.
///
/// This is the prerequisite for predicate pushdown: we want to push each
/// conjunct as deep into the plan as the columns it references allow, rather
/// than treating the whole predicate as one indivisible unit (which forces
/// it to stay as a top-level Filter).
pub fn split_and_chain(predicate: &Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    split_and_chain_rec(predicate, &mut out);
    out
}

fn split_and_chain_rec(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = expr
    {
        split_and_chain_rec(left, out);
        split_and_chain_rec(right, out);
    } else {
        out.push(expr.clone());
    }
}

/// Combine a list of conjuncts back into a single Expr with AND nodes.
/// Empty list returns a literal TRUE (so it's a no-op when wrapped in a Filter).
pub fn combine_and(conjuncts: &[Expr]) -> Expr {
    if conjuncts.is_empty() {
        return Expr::Literal(Value::Integer(1));
    }
    let mut iter = conjuncts.iter();
    let mut acc = iter.next().unwrap().clone();
    for c in iter {
        acc = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(acc),
            right: Box::new(c.clone()),
        };
    }
    acc
}

/// Collect all (table_alias_opt, column_name) references in an expression.
/// Used for predicate pushdown: we determine which side(s) of a Join the
/// predicate depends on by inspecting the columns it references.
///
/// Returns a Vec of `(Option<String>, String)` where the first element is the
/// table alias/qualifier (if the SQL said `u.id`) and the second is the
/// column name.
pub fn collect_column_refs(expr: &Expr) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    collect_column_refs_rec(expr, &mut out);
    out
}

fn collect_column_refs_rec(expr: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match expr {
        Expr::Column { table, name } => {
            out.push((table.clone(), name.clone()));
        }
        Expr::Binary { left, right, .. } => {
            collect_column_refs_rec(left, out);
            collect_column_refs_rec(right, out);
        }
        Expr::Unary { expr, .. } => collect_column_refs_rec(expr, out),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_refs_rec(expr, out);
            collect_column_refs_rec(low, out);
            collect_column_refs_rec(high, out);
        }
        Expr::In { expr, source, .. } => {
            collect_column_refs_rec(expr, out);
            if let crate::sql::ast::InSource::List(es) = source {
                for e in es {
                    collect_column_refs_rec(e, out);
                }
            }
            // Subquery sources don't reference outer columns by name in our AST.
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_column_refs_rec(expr, out);
            collect_column_refs_rec(pattern, out);
            if let Some(e) = escape {
                collect_column_refs_rec(e, out);
            }
        }
        Expr::IsNull { expr, .. } => collect_column_refs_rec(expr, out),
        Expr::Is { left, right, .. } => {
            collect_column_refs_rec(left, out);
            collect_column_refs_rec(right, out);
        }
        Expr::Function { args, filter, .. } => {
            for a in args {
                collect_column_refs_rec(a, out);
            }
            if let Some(f) = filter {
                collect_column_refs_rec(f, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_column_refs_rec(o, out);
            }
            for (w, t) in whens {
                collect_column_refs_rec(w, out);
                collect_column_refs_rec(t, out);
            }
            if let Some(e) = else_ {
                collect_column_refs_rec(e, out);
            }
        }
        Expr::Row(es) => {
            for e in es {
                collect_column_refs_rec(e, out);
            }
        }
        Expr::Cast { expr, .. } => collect_column_refs_rec(expr, out),
        Expr::Collate { expr, .. } => collect_column_refs_rec(expr, out),
        Expr::Raise { message, .. } => {
            if let Some(m) = message {
                collect_column_refs_rec(m, out);
            }
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Subquery(_) | Expr::Exists(_) => {}
    }
}

/// Infer the columns a plan produces, as a set of `(Option<alias_or_table_name>, column_name)`
/// pairs. For predicate pushdown we only need this for `Scan`, `Filter`,
/// `Join`, and `Subquery` — the shapes that appear in a FROM clause.
///
/// - Scan: (alias or table.name, each column)
/// - Filter: same as input
/// - Join: union of left + right
/// - Subquery: empty (we can't introspect)
/// - Other (Project, Aggregate, etc.): empty (we don't push past them)
pub fn plan_column_refs(plan: &Plan) -> Vec<(Option<String>, String)> {
    match plan {
        Plan::Scan { table, alias, .. } => {
            let prefix = alias.clone().unwrap_or_else(|| table.name.clone());
            table
                .columns
                .iter()
                .map(|c| (Some(prefix.clone()), c.name.clone()))
                .collect()
        }
        Plan::Filter { input, .. } | Plan::Subquery { plan: input } => plan_column_refs(input),
        Plan::Join { left, right, .. } => {
            let mut v = plan_column_refs(left);
            v.extend(plan_column_refs(right));
            v
        }
        _ => Vec::new(),
    }
}

/// Returns true if every column ref in `conjunct` is bound by `cols`.
/// A column with `table=None` matches any column with the same `name` regardless of prefix.
/// A column with `table=Some(t)` requires a matching `(Some(t), name)`.
fn conjunct_bound_by(conjunct: &Expr, cols: &[(Option<String>, String)]) -> bool {
    // A conjunct containing a subquery can hide OUTER column references
    // (`WHERE (SELECT COUNT(*) FROM t WHERE t.k = o.id) >= 2` — `o.id`
    // lives inside the subquery, invisible to collect_column_refs).
    // Bound-by would see zero refs and vacuously match BOTH sides, pushing
    // the conjunct into one side where the outer alias resolves to the
    // wrong column (or NULL). Keep subquery conjuncts at the top of the
    // Join, where the full combined row is in scope.
    if crate::executor::expr_has_subquery(conjunct) {
        return false;
    }
    let refs = collect_column_refs(conjunct);
    if refs.is_empty() {
        // No column references — it's a constant; safe to push down (or keep up).
        return true;
    }
    refs.iter().all(|(t, n)| match t {
        Some(prefix) => cols
            .iter()
            .any(|(p, c)| p.as_deref() == Some(prefix.as_str()) && c == n),
        None => cols.iter().any(|(_, c)| c == n),
    })
}

/// Push a WHERE predicate as deep into a plan as the columns it references allow.
///
/// The predicate is first split into AND-conjuncts. Each conjunct is categorized:
/// - If it only references columns from the LEFT side of a Join, push into left.
/// - If it only references columns from the RIGHT side, push into right.
/// - If it references columns from both sides (or is non-decomposable), keep
///   as a top-level Filter around the (rewritten) Join.
///
/// For non-Join plans, falls through to `apply_where_for_scan` which handles
/// `Plan::Scan` (RowidLookup / IndexLookup / Filter{Scan}).
///
/// Example:
///   `SELECT ... FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 500`
/// Before:  Filter { Join { Scan u, Scan o, on u.id=o.user_id }, u.id=500 }
/// After:   Join { RowidLookup u (id=500), Scan o, on u.id=o.user_id }
///          (no top-level Filter needed)
///
/// This is the single biggest perf win for filtered joins — the previous
/// 312x regression on the 2-table-join benchmark came from hashing all 10k
/// orders + probing with all 1k users, when the WHERE actually filtered
/// the left side to a single user.
/// Output column names of a SELECT's top level, when statically known:
/// explicit aliases or bare column references. `None` when any output is a
/// `*` or a complex expression without an alias (renaming then requires
/// execution).
pub fn top_level_output_names(sel: &SelectStatement) -> Option<Vec<String>> {
    match &sel.body {
        SelectBody::Simple(s) => {
            let mut names = Vec::with_capacity(s.columns.len());
            for c in &s.columns {
                match c {
                    crate::sql::ast::ResultColumn::Expr { expr, alias } => {
                        if let Some(a) = alias {
                            names.push(a.clone());
                        } else if let Expr::Column { name, .. } = expr {
                            names.push(name.clone());
                        } else {
                            return None;
                        }
                    }
                    _ => return None, // Star / TableStar
                }
            }
            Some(names)
        }
        _ => None, // compound selects: defer to execution-time names
    }
}

pub fn pushdown_filter(catalog: &Catalog, plan: Plan, predicate: &Expr) -> Plan {
    let conjuncts = split_and_chain(predicate);

    // If the plan is a Join, try to split conjuncts into left-only / right-only
    // / both-sides, and push down accordingly.
    if let Plan::Join {
        left,
        right,
        join_type,
        condition,
        algorithm,
    } = &plan
    {
        let left_cols = plan_column_refs(left);
        let right_cols = plan_column_refs(right);

        // Predicate pushdown below a join is only valid when the pushed
        // side's rows appear in the join output UNCHANGED. For outer joins
        // the null-extended side's rows are manufactured by the join, so a
        // WHERE predicate on that side must be evaluated AFTER the join
        // (pushing it into the scan would silently rewrite the ON clause):
        //
        //   INNER/CROSS: both sides pushable.
        //   LEFT:        left pushable; right side is null-extended.
        //   RIGHT:       right pushable; left side is null-extended.
        //   FULL:        neither side pushable.
        //
        // (SQLite additionally converts a LEFT JOIN to INNER when the WHERE
        // predicate is null-rejecting — an optimization we can add later;
        // keeping the predicate on top is always correct.)
        let left_pushable = matches!(
            join_type,
            JoinType::Inner | JoinType::Cross | JoinType::Left
        );
        let right_pushable = matches!(
            join_type,
            JoinType::Inner | JoinType::Cross | JoinType::Right
        );

        let mut left_preds: Vec<Expr> = Vec::new();
        let mut right_preds: Vec<Expr> = Vec::new();
        let mut top_preds: Vec<Expr> = Vec::new();

        for c in conjuncts {
            if conjunct_bound_by(&c, &left_cols) && !conjunct_bound_by(&c, &right_cols) {
                if left_pushable {
                    left_preds.push(c);
                } else {
                    top_preds.push(c);
                }
            } else if conjunct_bound_by(&c, &right_cols) && !conjunct_bound_by(&c, &left_cols) {
                if right_pushable {
                    right_preds.push(c);
                } else {
                    top_preds.push(c);
                }
            } else if conjunct_bound_by(&c, &left_cols) && conjunct_bound_by(&c, &right_cols) {
                // Column names collide across both sides (e.g. both have "id").
                // If the conjunct has explicit table qualifier, use it to
                // disambiguate; otherwise keep at top.
                let refs = collect_column_refs(&c);
                let all_qualified = refs.iter().all(|(t, _)| t.is_some());
                if all_qualified {
                    let left_only = refs.iter().all(|(t, _)| {
                        t.as_ref()
                            .map(|prefix| {
                                left_cols
                                    .iter()
                                    .any(|(p, _)| p.as_deref() == Some(prefix.as_str()))
                            })
                            .unwrap_or(false)
                    });
                    let right_only = refs.iter().all(|(t, _)| {
                        t.as_ref()
                            .map(|prefix| {
                                right_cols
                                    .iter()
                                    .any(|(p, _)| p.as_deref() == Some(prefix.as_str()))
                            })
                            .unwrap_or(false)
                    });
                    if left_only && left_pushable {
                        left_preds.push(c);
                    } else if right_only && right_pushable {
                        right_preds.push(c);
                    } else {
                        top_preds.push(c);
                    }
                } else {
                    top_preds.push(c);
                }
            } else {
                top_preds.push(c);
            }
        }

        // Recurse into each side.
        let new_left = if left_preds.is_empty() {
            (**left).clone()
        } else {
            pushdown_filter(catalog, (**left).clone(), &combine_and(&left_preds))
        };
        let new_right = if right_preds.is_empty() {
            (**right).clone()
        } else {
            pushdown_filter(catalog, (**right).clone(), &combine_and(&right_preds))
        };

        let new_join = Plan::Join {
            left: Box::new(new_left),
            right: Box::new(new_right),
            join_type: *join_type,
            condition: condition.clone(),
            algorithm: *algorithm,
        };

        if top_preds.is_empty() {
            new_join
        } else {
            // Wrap remaining (non-pushable) conjuncts in a top-level Filter,
            // but try the cheap index/rowid path on the Join as a whole first.
            apply_where_for_scan(catalog, new_join, &combine_and(&top_preds))
        }
    } else {
        // Not a Join — let apply_where_for_scan handle the Scan + index path,
        // or wrap in a Filter for other plan shapes.
        apply_where_for_scan(catalog, plan, &combine_and(&conjuncts))
    }
}

/// Heuristic: is the outer plan's output expected to be SMALL relative to the
/// underlying table? If yes, INLJ is profitable (one index lookup per outer
/// row). If no, Hash join is faster (single full scan + hash build).
///
/// Selective plans (good INLJ candidates):
/// - `RowidLookup` — returns ≤ 1 row.
/// - `IndexLookup` — returns few rows (point lookup on indexed column).
/// - `RowidRange` — returns rows in [start, end]; selective when the range
///   is narrow (we can't know that statically, but it's at most the table
///   size).
/// - `Filter { input, .. }` — WHERE predicate applied; assumed selective
///   (the planner wouldn't have added a Filter for an always-true predicate).
///
/// Non-selective plans (Hash join is better):
/// - Bare `Scan` — returns the entire table.
/// - `Project`, `Sort`, `Limit`, `Distinct` — passthrough wrappers; their
///   selectivity depends on their input, so recurse.
/// - `Aggregate`, `Window` — typically produce few output rows, but their
///   input is the full table. For now, treat as non-selective (Hash join is
///   fine — the outer is already an aggregate, not a raw scan).
fn outer_is_selective(plan: &Plan) -> bool {
    match plan {
        Plan::RowidLookup { .. } | Plan::IndexLookup { .. } | Plan::RowidRange { .. } => true,
        Plan::Filter { .. } => true,
        // If the outer is itself an IndexNestedLoopJoin, its outer was
        // already deemed selective (we only pick INLJ when
        // `outer_is_selective` returns true). So a chained INLJ is also
        // selective — important for 3-table joins where the inner join
        // produces a small filtered set that's then joined to a third table.
        Plan::IndexNestedLoopJoin { .. } => true,
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input } => outer_is_selective(input),
        _ => false,
    }
}

/// Post-pass optimization: rewrite eligible Hash joins to
/// `IndexNestedLoopJoin` when the inner side is a base table scan with an
/// index whose first column matches the join key.
///
/// Recursively walks the plan tree. For each `Plan::Join { algorithm: Hash,
/// join_type: Inner|Cross, condition: Some(eq_chain) }`:
/// 1. Extract the equi-join key pairs (left_col, right_col).
/// 2. Check whether the right side is a `Scan` on table R, and there's an
///    index on R whose first column is `right_col`.
/// 3. Check whether the left side is a `Scan` on table L, and there's an
///    index on L whose first column is `left_col`.
/// 4. If both sides qualify, pick the smaller side as outer (heuristic:
///    the side whose scan is more selective wins; we use the left as the
///    outer by default since apply_where usually pushes selective predicates
///    to the left).
/// 5. If only one side qualifies, use that side as the inner (the other
///    becomes the outer).
/// 6. Otherwise, leave the Hash join in place.
///
/// This is the canonical optimization for OLTP joins: `JOIN orders o ON
/// u.id = o.user_id WHERE u.id = ?` should never decode all 10k orders when
/// `idx_orders_user` can fetch the ~10 matching rows directly.
pub fn optimize_index_nested_loop_join(catalog: &Catalog, plan: Plan) -> Plan {
    match plan {
        Plan::Join {
            left,
            right,
            join_type,
            condition,
            algorithm,
        } => {
            // Recurse into children first.
            let left = Box::new(optimize_index_nested_loop_join(catalog, *left));
            let right = Box::new(optimize_index_nested_loop_join(catalog, *right));

            // Only INNER/CROSS joins qualify — outer joins must preserve all
            // rows from the preserved side, which forbids index-only access
            // to the inner side (the join needs to emit NULL-extended rows
            // when no match exists, which requires materializing the outer).
            let is_inner = matches!(join_type, JoinType::Inner | JoinType::Cross);
            let is_hash = matches!(algorithm, plan::JoinAlgorithm::Hash);
            if !is_inner || !is_hash {
                return Plan::Join {
                    left,
                    right,
                    join_type,
                    condition,
                    algorithm,
                };
            }

            // Extract equi-join key pairs from the ON condition.
            let eq_pairs = match condition.as_ref() {
                Some(c) => extract_equi_join_keys_for_planner(c, &left, &right),
                None => {
                    return Plan::Join {
                        left,
                        right,
                        join_type,
                        condition,
                        algorithm,
                    }
                }
            };
            if eq_pairs.is_empty() {
                return Plan::Join {
                    left,
                    right,
                    join_type,
                    condition,
                    algorithm,
                };
            }

            // Try the right side as inner (the common case: scan right table
            // via its index). The outer (left) plan is kept verbatim — we
            // don't unwrap Filter/Project around it, because doing so would
            // drop the WHERE clause.
            //
            // Selectivity heuristic: only pick INLJ when the outer is filtered
            // (has a WHERE clause pushed down, or is a point/range lookup).
            // When the outer is a bare full-table Scan, Hash join is faster:
            // 1000 separate index lookups (~3 ms) is more expensive than
            // decoding the inner table once and hashing it (~1.5 ms). This is
            // what makes `SELECT u.dept, COUNT(*), SUM(o.total) FROM users
            // u JOIN orders o ON u.id = o.user_id GROUP BY u.dept` (no WHERE)
            // ~3× faster than the INLJ path.
            if let Plan::Scan {
                table: r_table,
                alias: r_alias,
                ..
            } = right.as_ref()
            {
                if outer_is_selective(&left) {
                    for (l_qual, l_col, _r_qual, r_col) in &eq_pairs {
                        if let Some(outer_key_col) =
                            resolve_outer_col_index(&left, l_qual.as_deref(), l_col)
                        {
                            if let Some(idx) = find_index_for_column(catalog, r_table, r_col) {
                                return Plan::IndexNestedLoopJoin {
                                    outer: left.clone(),
                                    inner_table: r_table.clone(),
                                    inner_alias: r_alias.clone(),
                                    inner_index: idx,
                                    outer_key_col,
                                };
                            }
                        }
                    }
                }
            }

            // Symmetric: try the left side as inner. The outer (right) plan
            // is kept verbatim.
            if let Plan::Scan {
                table: l_table,
                alias: l_alias,
                ..
            } = left.as_ref()
            {
                if outer_is_selective(&right) {
                    for (_l_qual, l_col, r_qual, r_col) in &eq_pairs {
                        if let Some(outer_key_col) =
                            resolve_outer_col_index(&right, r_qual.as_deref(), r_col)
                        {
                            if let Some(idx) = find_index_for_column(catalog, l_table, l_col) {
                                return Plan::IndexNestedLoopJoin {
                                    outer: right.clone(),
                                    inner_table: l_table.clone(),
                                    inner_alias: l_alias.clone(),
                                    inner_index: idx,
                                    outer_key_col,
                                };
                            }
                        }
                    }
                }
            }

            Plan::Join {
                left,
                right,
                join_type,
                condition,
                algorithm,
            }
        }

        // Recurse into wrapper nodes.
        Plan::Filter { input, predicate } => Plan::Filter {
            input: Box::new(optimize_index_nested_loop_join(catalog, *input)),
            predicate,
        },
        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(optimize_index_nested_loop_join(catalog, *input)),
            columns,
        },
        Plan::Sort { input, terms } => Plan::Sort {
            input: Box::new(optimize_index_nested_loop_join(catalog, *input)),
            terms,
        },
        Plan::Limit {
            input,
            count,
            offset,
        } => Plan::Limit {
            input: Box::new(optimize_index_nested_loop_join(catalog, *input)),
            count,
            offset,
        },
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Plan::Aggregate {
            input: Box::new(optimize_index_nested_loop_join(catalog, *input)),
            group_by,
            aggregates,
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(optimize_index_nested_loop_join(catalog, *input)),
        },
        other => other, // Leaves and other node types: no rewrite.
    }
}

/// Extract equi-join key pairs from a join condition. Returns one tuple
/// per `left.col = right.col` conjunct (AND-chain). The tuple is
/// `(left_qualifier, left_col, right_qualifier, right_col)`.
///
/// The executor needs to know which column in the OUTER row supplies the
/// join key, and which column on the INNER table the index is built on.
/// We return column names (and optional table qualifiers) as strings here;
/// the executor resolves them to column indices via `resolve_outer_key` below.
fn extract_equi_join_keys_for_planner(
    cond: &Expr,
    left_plan: &Plan,
    right_plan: &Plan,
) -> Vec<(Option<String>, String, Option<String>, String)> {
    let mut out = Vec::new();
    let mut stack = vec![cond.clone()];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Binary {
                op: BinaryOp::And,
                left,
                right,
            } => {
                stack.push(*left);
                stack.push(*right);
            }
            Expr::Binary {
                op: BinaryOp::Eq,
                left,
                right,
            } => {
                // Both sides must be column refs.
                if let (
                    Expr::Column {
                        table: lt,
                        name: ln,
                    },
                    Expr::Column {
                        table: rt,
                        name: rn,
                    },
                ) = (left.as_ref(), right.as_ref())
                {
                    // SIDE-AWARE classification. The textual operand order
                    // of the ON clause is irrelevant: `A JOIN B ON b.k =
                    // a.k` must produce the same (left_col, right_col) pair
                    // as `ON a.k = b.k`. Resolve each operand against BOTH
                    // plan sides and canonicalize:
                    //   - operand resolves only on the left → left key
                    //   - operand resolves only on the right → right key
                    //   - resolves on both (unqualified, name exists in
                    //     both tables) → fall back to textual order (best
                    //     effort; the executor's residual filter still
                    //     guarantees correctness)
                    //   - both operands on the SAME side → not an equi-join
                    //     key at all (it's a pushed-down filter); skip.
                    let a_on_left = resolve_outer_col_index(left_plan, lt.as_deref(), ln).is_some();
                    let a_on_right =
                        resolve_outer_col_index(right_plan, lt.as_deref(), ln).is_some();
                    let b_on_left = resolve_outer_col_index(left_plan, rt.as_deref(), rn).is_some();
                    let b_on_right =
                        resolve_outer_col_index(right_plan, rt.as_deref(), rn).is_some();

                    let pair = match (a_on_left, a_on_right, b_on_left, b_on_right) {
                        // A = left key, B = right key (canonical order).
                        (true, false, false, true) => {
                            Some((lt.clone(), ln.clone(), rt.clone(), rn.clone()))
                        }
                        // A = right key, B = left key — SWAP to canonical.
                        (false, true, true, false) => {
                            Some((rt.clone(), rn.clone(), lt.clone(), ln.clone()))
                        }
                        // Ambiguous on one side: keep textual order.
                        (true, true, _, _) | (_, _, true, true) => {
                            Some((lt.clone(), ln.clone(), rt.clone(), rn.clone()))
                        }
                        // Same side only / unresolvable: not a join key.
                        _ => None,
                    };
                    if let Some(p) = pair {
                        out.push(p);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Given a plan and a column name (with optional table qualifier),
/// resolve which OUTPUT column index of that plan the column refers to.
/// Returns just the index — the plan itself is kept verbatim by the caller,
/// because unwrapping `Filter`/`Project` around the plan would drop the
/// WHERE clause / projection the planner has already applied.
///
/// The output column index is determined by walking the plan tree:
/// - `Scan { table, alias }` → index of the column in `table.columns`.
///   Match by alias if the qualifier matches the alias; otherwise match by
///   table name.
/// - `RowidLookup { table, .. }`, `IndexLookup { table, .. }`,
///   `RowidRange { table, .. }` → same as Scan (these all emit table rows
///   in column order).
/// - `Filter { input, .. }` → recurse into input (Filter is transparent:
///   its output columns are the input's).
/// - `IndexNestedLoopJoin { outer, inner_table, .. }` → first try the inner
///   table's columns; if matched, return `outer.n_cols + inner_idx`. Else
///   recurse into outer.
/// - `Join { left, right, .. }` → try left first (recurse); if no match,
///   try right with a column offset of `left.n_cols`.
/// - Other plan shapes → None (we don't optimize joins on top of them).
fn resolve_outer_col_index(
    plan: &Plan,
    table_qualifier: Option<&str>,
    col_name: &str,
) -> Option<usize> {
    match plan {
        Plan::Scan { table, alias, .. } => {
            resolve_table_col_index(table, alias.as_deref(), table_qualifier, col_name)
        }

        Plan::RowidLookup { table, alias, .. } => {
            resolve_table_col_index(table, alias.as_deref(), table_qualifier, col_name)
        }

        Plan::IndexLookup { table, alias, .. } => {
            resolve_table_col_index(table, alias.as_deref(), table_qualifier, col_name)
        }

        Plan::IndexRange { table, alias, .. } => {
            resolve_table_col_index(table, alias.as_deref(), table_qualifier, col_name)
        }

        Plan::RowidRange { table, alias, .. } => {
            resolve_table_col_index(table, alias.as_deref(), table_qualifier, col_name)
        }

        Plan::Filter { input, .. } => {
            // Filter is transparent — its output columns are the input's.
            resolve_outer_col_index(input, table_qualifier, col_name)
        }

        Plan::IndexNestedLoopJoin {
            outer,
            inner_table,
            inner_alias,
            ..
        } => {
            // Try inner table first; if not matched, recurse into outer.
            if let Some(idx) = resolve_table_col_index(
                inner_table,
                inner_alias.as_deref(),
                table_qualifier,
                col_name,
            ) {
                // Inner table columns come AFTER outer columns in the output.
                let outer_n = plan_output_width(outer);
                return Some(outer_n + idx);
            }
            resolve_outer_col_index(outer, table_qualifier, col_name)
        }

        Plan::Join { left, right, .. } => {
            // Try left first; if not matched, try right with offset.
            if let Some(idx) = resolve_outer_col_index(left, table_qualifier, col_name) {
                return Some(idx);
            }
            let left_n = plan_output_width(left);
            resolve_outer_col_index(right, table_qualifier, col_name).map(|i| left_n + i)
        }

        _ => None,
    }
}

/// Helper: resolve a column index within a single table (used by Scan,
/// RowidLookup, IndexLookup, RowidRange — all of which emit table rows in
/// column order).
fn resolve_table_col_index(
    table: &Table,
    alias: Option<&str>,
    qualifier: Option<&str>,
    col_name: &str,
) -> Option<usize> {
    if let Some(q) = qualifier {
        let alias_match = alias.map(|a| a.eq_ignore_ascii_case(q)).unwrap_or(false);
        let name_match = q.eq_ignore_ascii_case(&table.name);
        if !alias_match && !name_match {
            return None;
        }
    }
    table
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(col_name))
}

/// Helper: compute the output column width of a plan.
/// Used by `resolve_outer_col_index` to offset into Join/IndexNestedLoopJoin
/// outputs.
fn plan_output_width(plan: &Plan) -> usize {
    match plan {
        Plan::CteRows { rows, columns } => {
            let _ = rows;
            columns.len()
        }
        Plan::Scan { table, .. } => table.n_columns(),
        Plan::RowidLookup { table, .. } => table.n_columns(),
        Plan::RowidIn { table, .. } => table.n_columns(),
        Plan::IndexIn { table, .. } => table.n_columns(),
        Plan::IndexLookup { table, .. } => table.n_columns(),
        Plan::IndexRange { table, .. } => table.n_columns(),
        Plan::RowidRange { table, .. } => table.n_columns(),
        Plan::Values { rows } => rows.first().map(|r| r.len()).unwrap_or(0),
        Plan::Filter { input, .. } => plan_output_width(input),
        Plan::Project { input, columns } => {
            // Project may shrink or grow (via star expansion). We can't
            // compute that without expanding stars, so we use a heuristic:
            // count non-star columns + 1 per star (which is wrong in general
            // but only used when no better signal is available).
            let star_count = columns
                .iter()
                .filter(|c| matches!(&c.expr, Expr::Column { name, .. } if name == "*"))
                .count();
            if star_count == 0 {
                columns.len()
            } else {
                // Fall back to the input's width — only correct if a star
                // expands to the full input width.
                plan_output_width(input)
            }
        }
        Plan::Sort { input, .. } => plan_output_width(input),
        Plan::Limit { input, .. } => plan_output_width(input),
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => plan_output_width(input) + group_by.len() + aggregates.len(),
        Plan::Window { input, windows } => plan_output_width(input) + windows.len(),
        Plan::Join { left, right, .. } => plan_output_width(left) + plan_output_width(right),
        Plan::IndexNestedLoopJoin {
            outer, inner_table, ..
        } => plan_output_width(outer) + inner_table.n_columns(),
        Plan::Subquery { plan } => plan_output_width(plan),
        Plan::Distinct { input } => plan_output_width(input),
        Plan::Union { left, .. } => plan_output_width(left),
        Plan::Intersect { left, .. } => plan_output_width(left),
        Plan::Except { left, .. } => plan_output_width(left),
        Plan::Insert { .. } | Plan::Update { .. } | Plan::Delete { .. } => 0,
    }
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
            Plan::Project {
                input: Box::new(sorted),
                columns,
            }
        }
        Plan::Distinct { input } => {
            // Distinct wraps a Project. Push Sort inside the Project.
            match *input {
                Plan::Project {
                    input: proj_input,
                    columns,
                } => {
                    let sorted = Plan::Sort {
                        input: proj_input,
                        terms,
                    };
                    let new_proj = Plan::Project {
                        input: Box::new(sorted),
                        columns,
                    };
                    Plan::Distinct {
                        input: Box::new(new_proj),
                    }
                }
                // Distinct over something else — wrap in Sort above Distinct.
                other_inner => {
                    let distinct = Plan::Distinct {
                        input: Box::new(other_inner),
                    };
                    Plan::Sort {
                        input: Box::new(distinct),
                        terms,
                    }
                }
            }
        }
        other => Plan::Sort {
            input: Box::new(other),
            terms,
        },
    }
}

// ---------------------------------------------------------------------------
// Column-declared collation attachment (SQLite collation resolution)
// ---------------------------------------------------------------------------

/// Collect the (table, effective-alias) pairs visible in a FROM scope for
/// collation resolution. Subqueries / CTEs have no declared collations and
/// are skipped.
pub(crate) fn collect_collation_scope(
    catalog: &Catalog,
    te: &TableExpression,
) -> Vec<(std::sync::Arc<crate::schema::Table>, String)> {
    let mut out = Vec::new();
    collect_scope_rec(catalog, te, &mut out);
    out
}

fn collect_scope_rec(
    catalog: &Catalog,
    te: &TableExpression,
    out: &mut Vec<(std::sync::Arc<crate::schema::Table>, String)>,
) {
    match te {
        TableExpression::Table { name, alias, .. } => {
            if let Some(t) = catalog.get_table(name) {
                out.push((t, alias.clone().unwrap_or_else(|| name.clone())));
            }
        }
        TableExpression::Subquery { .. } => {}
        TableExpression::Join { left, right, .. } => {
            collect_scope_rec(catalog, left, out);
            collect_scope_rec(catalog, right, out);
        }
    }
}

/// The declared collation of a column reference, if it resolves in scope
/// and is not BINARY.
fn column_declared_collation(
    catalog: &Catalog,
    e: &Expr,
    scope: &[(std::sync::Arc<crate::schema::Table>, String)],
) -> Option<String> {
    let (qualifier, name) = match e {
        Expr::Column { table, name } => (table.as_deref(), name),
        _ => return None,
    };
    for (t, alias) in scope {
        let qualified_match = match qualifier {
            Some(q) => q.eq_ignore_ascii_case(alias) || q.eq_ignore_ascii_case(&t.name),
            None => true,
        };
        if qualified_match {
            if let Some(i) = t.find_column(name) {
                let coll = t.columns[i].collation.clone();
                if !coll.eq_ignore_ascii_case("BINARY") {
                    return Some(coll);
                }
                return None;
            }
        }
    }
    let _ = catalog;
    None
}

/// Does an expression (or a nested operand) carry an explicit COLLATE?
fn has_explicit_collate(e: &Expr) -> bool {
    match e {
        Expr::Collate { .. } => true,
        Expr::Unary { expr, .. } => has_explicit_collate(expr),
        Expr::Binary { left, right, .. } => {
            has_explicit_collate(left) || has_explicit_collate(right)
        }
        _ => false,
    }
}

/// SQLite collation attachment: rewrite comparisons so that a column with
/// a DECLARED collation compares through it. Explicit COLLATE anywhere in
/// the comparison already wins (left operand first, per SQLite). The
/// rewrite attaches `Expr::Collate` to the column operand, which every
/// evaluation path (general evaluator, compiled-predicate fallback, join
/// conditions) already honors.
pub(crate) fn rewrite_column_collations(
    catalog: &Catalog,
    e: &Expr,
    scope: &[(std::sync::Arc<crate::schema::Table>, String)],
) -> Expr {
    match e {
        Expr::Binary { op, left, right } => match op {
            BinaryOp::And => Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(rewrite_column_collations(catalog, left, scope)),
                right: Box::new(rewrite_column_collations(catalog, right, scope)),
            },
            BinaryOp::Or => Expr::Binary {
                op: BinaryOp::Or,
                left: Box::new(rewrite_column_collations(catalog, left, scope)),
                right: Box::new(rewrite_column_collations(catalog, right, scope)),
            },
            op if matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
            ) =>
            {
                // Explicit COLLATE on either operand wins — leave as-is.
                if has_explicit_collate(left) || has_explicit_collate(right) {
                    return e.clone();
                }
                // Left operand's declared collation, else the right's
                // (SQLite's rule).
                if let Some(coll) = column_declared_collation(catalog, left, scope) {
                    return Expr::Binary {
                        op: *op,
                        left: Box::new(Expr::Collate {
                            expr: left.clone(),
                            collation: coll,
                        }),
                        right: right.clone(),
                    };
                }
                if let Some(coll) = column_declared_collation(catalog, right, scope) {
                    return Expr::Binary {
                        op: *op,
                        left: left.clone(),
                        right: Box::new(Expr::Collate {
                            expr: right.clone(),
                            collation: coll,
                        }),
                    };
                }
                e.clone()
            }
            _ => e.clone(),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_column_collations(catalog, expr, scope)),
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            if let Some(coll) = column_declared_collation(catalog, expr, scope) {
                Expr::Between {
                    expr: Box::new(Expr::Collate {
                        expr: expr.clone(),
                        collation: coll,
                    }),
                    low: low.clone(),
                    high: high.clone(),
                    negated: *negated,
                }
            } else {
                e.clone()
            }
        }
        Expr::In {
            expr,
            source,
            negated,
        } => {
            if let Some(coll) = column_declared_collation(catalog, expr, scope) {
                Expr::In {
                    expr: Box::new(Expr::Collate {
                        expr: expr.clone(),
                        collation: coll,
                    }),
                    source: source.clone(),
                    negated: *negated,
                }
            } else {
                e.clone()
            }
        }
        _ => e.clone(),
    }
}

#[cfg(test)]
mod plan_dump_tests {
    use crate::api::Database;

    #[test]
    fn dump_limit_plan_shapes() {
        let mut db = Database::open_in_memory().unwrap();
        db.execute("CREATE TABLE bench (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b REAL NOT NULL, c TEXT NOT NULL)", []).unwrap();
        for sql in [
            "SELECT a FROM bench WHERE a BETWEEN ? AND ? LIMIT 1",
            "SELECT a FROM bench WHERE a % 10 = 0 LIMIT 5",
            "SELECT * FROM bench WHERE a > 3 LIMIT 5",
        ] {
            let stmt = crate::sql::parser::parse(sql).unwrap();
            let plan = Database::plan_for_statement(db.catalog_ref(), &stmt).unwrap();
            let mut s = String::new();
            if let Some(p) = plan {
                fn walk(p: &crate::planner::plan::Plan, s: &mut String, depth: usize) {
                    let name = match p {
                        crate::planner::plan::Plan::Scan {
                            predicate, index, ..
                        } => {
                            if index.is_some() {
                                "Scan(idx)"
                            } else if predicate.is_some() {
                                "Scan(pred)"
                            } else {
                                "Scan"
                            }
                        }
                        crate::planner::plan::Plan::Project { .. } => "Project",
                        crate::planner::plan::Plan::Filter { .. } => "Filter",
                        crate::planner::plan::Plan::Limit { .. } => "Limit",
                        crate::planner::plan::Plan::Sort { .. } => "Sort",
                        crate::planner::plan::Plan::Aggregate { .. } => "Aggregate",
                        _ => "Other",
                    };
                    s.push_str(&"  ".repeat(depth));
                    s.push_str(name);
                    s.push('\n');
                    let kids: Vec<&crate::planner::plan::Plan> = match p {
                        crate::planner::plan::Plan::Project { input, .. }
                        | crate::planner::plan::Plan::Filter { input, .. }
                        | crate::planner::plan::Plan::Limit { input, .. }
                        | crate::planner::plan::Plan::Sort { input, .. }
                        | crate::planner::plan::Plan::Aggregate { input, .. }
                        | crate::planner::plan::Plan::Distinct { input }
                        | crate::planner::plan::Plan::Window { input, .. } => vec![input.as_ref()],
                        _ => vec![],
                    };
                    for k in kids {
                        walk(k, s, depth + 1);
                    }
                }
                walk(&p, &mut s, 0);
            }
            println!("{sql}\n{s}");
        }
    }
}
